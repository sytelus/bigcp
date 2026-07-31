//! Shared semantic file engine.
//!
//! Plain small files are fully read and source-revalidated before the
//! destination is touched, then written directly to their final name
//! (rerun-repair crash contract, ADR 0030). Files with destination-representable
//! auxiliary streams or EAs and all large files use an opaque temporary so one
//! logical file is published atomically (ADRs 0034/0035).

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use bigcp_win::{
    DestinationFinal, DestinationStream, DestinationTemp, ObjectMetadata, SourceFile, SourceStream,
    StreamInfo, is_encrypted, is_readonly, is_sparse, list_streams, metadata_at,
    read_extended_attributes_checked, read_protected_dacl_checked, set_basic_at_checked,
    without_readonly,
};
use xxhash_rust::xxh3::Xxh3;

use crate::error::{ErrorCategory, OperationError};
use crate::journal::{Checkpoint, CheckpointFileIdentity, Journal, JournalEvent};
use crate::model::{Counters, EntrySnapshot};
use crate::phase::PhaseTracker;

/// Successful engine result consumed by the common outcome path.
pub struct EngineResult {
    /// Logical bytes successfully copied across the unnamed and named streams.
    pub bytes: u64,
    /// xxh3-128 digest when policy enabled hashing.
    pub digest: Option<String>,
    /// Named stream digests captured for post-copy verification.
    pub stream_digests: Vec<(StreamInfo, String)>,
    /// EA payload digest captured for post-copy verification.
    pub ea_digest: Option<String>,
    /// Named streams dropped because the destination lacks capability.
    pub streams_dropped: u32,
    /// Whether source EAs were omitted because the destination lacks support.
    pub eas_dropped: bool,
    /// Whether checkpointing was disabled after a journal append failure.
    pub journal_degraded: bool,
    /// Whether source EFS state could not be represented at the destination.
    pub efs_downgraded: bool,
    /// Whether this file had a persistent checkpoint that must be retired.
    pub checkpoint_used: bool,
}

/// Parameters shared by the two size strategies.
pub struct EngineRequest<'a> {
    /// Absolute source path.
    pub source_path: &'a Path,
    /// Absolute final destination path.
    pub destination_path: &'a Path,
    /// Relative source identity for errors.
    pub relative_path: &'a Path,
    /// Enumeration snapshot.
    pub source_snapshot: &'a EntrySnapshot,
    /// Replacement snapshot, if any.
    pub replacement_snapshot: Option<&'a EntrySnapshot>,
    /// Unique run identifier for temp ownership.
    pub run_id: &'a str,
    /// Size threshold between pre-read and streamed strategies.
    pub large_threshold: u64,
    /// Hash small files for post-copy verification.
    pub verify: bool,
    /// Flush final data and metadata.
    pub flush: bool,
    /// Whether the destination can represent named streams.
    pub destination_supports_streams: bool,
    /// Whether source stream discovery is meaningful on this filesystem.
    pub source_supports_streams: bool,
    /// Whether source EA reads are meaningful on this filesystem.
    pub source_supports_eas: bool,
    /// Whether the destination volume advertises extended-attribute support.
    pub destination_supports_eas: bool,
    /// Composed profile chunk bytes.
    pub chunk_bytes: usize,
    /// Whether sparse layout can and should be preserved.
    pub preserve_sparse: bool,
    /// Minimum per-stream size eligible for partial resume checkpoints.
    pub checkpoint_threshold: u64,
    /// Whether the destination advertises EFS support.
    pub destination_supports_encryption: bool,
    /// Whether protected destination DACLs can exist and need preservation.
    pub destination_supports_persistent_acls: bool,
    /// Whether publication can use POSIX unlink/rename semantics.
    pub destination_supports_posix_unlink_rename: bool,
    /// Source metadata projected to fields the destination can represent.
    pub destination_metadata: bigcp_win::BasicMetadata,
    /// Whether data writes require a final FAT-family metadata restamp.
    pub destination_requires_post_write_stamp: bool,
    /// Graceful-cancel probe checked between chunks so very large files do
    /// not delay a requested stop until they finish (the in-flight temp
    /// self-deletes or resumes from its last verified checkpoint).
    pub cancel: &'a dyn Fn() -> bool,
    /// When set, a discovered stream at or above this size aborts before any
    /// write with [`PROMOTED_TO_COORDINATOR`]: worker dispatch routes by the
    /// enumerated unnamed size alone (no coordinator-side stream probe), so a
    /// file hiding a huge ADS is handed back to the coordinator, which owns
    /// the journal and the responsive cancel probe. Inline callers pass None.
    pub promote_threshold: Option<u64>,
    /// Streams already discovered by the scheduler, avoiding a duplicate call.
    pub known_streams: Option<&'a [StreamInfo]>,
    /// Per-run phase measurements shared by the coordinator and workers.
    pub phases: &'a PhaseTracker,
}

/// Copies one ordinary file through the fidelity- and size-appropriate
/// strategy: direct final-name write only for plain small files, otherwise
/// temp+rename.
pub fn copy_file(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    mut journal: Option<&mut Journal>,
) -> Result<EngineResult, OperationError> {
    let timer = std::time::Instant::now();
    let mut source = SourceFile::open(request.source_path)
        .map_err(|error| source_open_error("open_src", request.relative_path, &error))?;
    ensure_source_unchanged(
        request.source_snapshot,
        source.opened_metadata(),
        request.relative_path,
        "open_src",
    )?;
    request.phases.record(0, timer.elapsed());

    let discovered_streams;
    let synthetic_streams;
    let streams = if let Some(streams) = request.known_streams {
        streams
    } else if !request.source_supports_streams {
        synthetic_streams = vec![StreamInfo::unnamed(request.source_snapshot.metadata.size)];
        &synthetic_streams
    } else {
        let timer = std::time::Instant::now();
        discovered_streams = list_streams(request.source_path)
            .map_err(|error| source_open_error("list_streams", request.relative_path, &error))?;
        request.phases.record(1, timer.elapsed());
        &discovered_streams
    };
    let routing = route_streams(
        streams,
        request.source_snapshot.metadata.size,
        request.destination_supports_streams,
        journal.is_some(),
        request.checkpoint_threshold,
    );
    let largest_stream = routing.largest_representable;
    if let Some(threshold) = request.promote_threshold
        && largest_stream >= threshold
    {
        // Nothing has been created or written yet; the coordinator reruns
        // this file inline with checkpoints and the cancel probe.
        return Err(OperationError::semantic(
            ErrorCategory::Internal,
            PROMOTED_TO_COORDINATOR,
            request.relative_path.to_path_buf(),
            "stream set requires coordinator streaming",
        ));
    }
    let checkpoint_eligible = routing.checkpoint_eligible;
    let should_hash =
        request.verify || largest_stream >= request.large_threshold || checkpoint_eligible;
    let source_has_eas =
        request.source_supports_eas && request.source_snapshot.metadata.ea_size > 0;
    let eas_dropped = source_has_eas && !request.destination_supports_eas;
    let extended_attributes = (source_has_eas && request.destination_supports_eas)
        .then(|| {
            read_extended_attributes_checked(
                request.source_path,
                request.source_snapshot.metadata.identity,
            )
        })
        .transpose()
        .map_err(|error| source_open_error("read_ea", request.relative_path, &error))?;
    let named_streams = routing.named_streams;
    let named_streams_dropped = routing.named_streams_dropped;
    let has_representable_auxiliary_data = extended_attributes.is_some()
        || (request.destination_supports_streams && named_streams > 0);
    if request.preserve_sparse && is_sparse(request.source_snapshot.metadata.basic.attributes) {
        copy_sparse(
            request,
            counters,
            &mut source,
            streams,
            extended_attributes.as_ref(),
            eas_dropped,
            should_hash,
            journal.as_deref_mut(),
        )
    } else if largest_stream < request.large_threshold
        && !checkpoint_eligible
        && !has_representable_auxiliary_data
    {
        copy_plain_small(
            request,
            counters,
            &mut source,
            should_hash,
            named_streams_dropped,
            eas_dropped,
        )
    } else {
        copy_streamed(
            request,
            counters,
            &mut source,
            streams,
            extended_attributes.as_ref(),
            eas_dropped,
            should_hash,
            journal.as_deref_mut(),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StreamRouting {
    largest_representable: u64,
    checkpoint_eligible: bool,
    named_streams: usize,
    named_streams_dropped: u32,
}

/// Computes routing only from streams the destination can represent. A large
/// ADS that will be dropped must not promote an otherwise-small FAT/exFAT file
/// to the transactional/checkpoint path.
fn route_streams(
    streams: &[StreamInfo],
    unnamed_size: u64,
    destination_supports_streams: bool,
    journal_available: bool,
    checkpoint_threshold: u64,
) -> StreamRouting {
    let representable = streams
        .iter()
        .filter(|stream| destination_supports_streams || stream.is_unnamed());
    let largest_representable = representable
        .clone()
        .map(|stream| stream.size)
        .max()
        .unwrap_or(unnamed_size);
    let checkpoint_eligible = journal_available
        && representable
            .clone()
            .any(|stream| stream.size >= checkpoint_threshold);
    let named_streams = streams.iter().filter(|stream| !stream.is_unnamed()).count();
    let named_streams_dropped = if destination_supports_streams {
        0
    } else {
        u32::try_from(named_streams).unwrap_or(u32::MAX)
    };
    StreamRouting {
        largest_representable,
        checkpoint_eligible,
        named_streams,
        named_streams_dropped,
    }
}

fn copy_sparse(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut SourceFile,
    streams: &[StreamInfo],
    extended_attributes: Option<&bigcp_win::ExtendedAttributes>,
    source_eas_dropped: bool,
    should_hash: bool,
    mut journal: Option<&mut Journal>,
) -> Result<EngineResult, OperationError> {
    let logical_size = request.source_snapshot.metadata.size;
    let ranges = source
        .allocated_ranges(logical_size)
        .map_err(|error| operation_error("query_sparse", request.relative_path, &error))?;
    let parent = destination_parent(request)?;
    let path_key = crate::journal::path_key(request.relative_path);
    let checkpoint = journal
        .as_deref()
        .and_then(|value| value.checkpoint_owned(&path_key, ""));
    let resumed = checkpoint
        .as_ref()
        .map(|checkpoint| resume_unnamed(request, counters, source, parent, checkpoint))
        .transpose()?
        .flatten();
    let (mut temp, mut hash_offset, mut hasher) = if let Some(state) = resumed {
        state
    } else {
        // EFS + sparse are mutually exclusive on NTFS, so a sparse copy never
        // requests encryption; an encrypted source takes the non-sparse paths.
        let temp = DestinationTemp::create(parent, request.run_id, false)
            .map_err(|error| operation_error("create_dst_temp", request.relative_path, &error))?;
        temp.mark_sparse()
            .map_err(|error| operation_error("set_sparse", request.relative_path, &error))?;
        (temp, 0, should_hash.then(Xxh3::new))
    };
    let efs_downgraded = efs_downgraded_for(request, &temp);
    temp.set_len(logical_size)
        .map_err(|error| operation_error("set_eof", request.relative_path, &error))?;

    let mut buffer = vec![0_u8; request.chunk_bytes];
    if buffer.is_empty() {
        return Err(OperationError::semantic(
            ErrorCategory::Internal,
            "allocate_buffer",
            request.relative_path.to_path_buf(),
            "the composed copy profile selected a zero-byte chunk",
        ));
    }
    let checkpoint_eligible = logical_size >= request.checkpoint_threshold && journal.is_some();
    let interval = checkpoint_interval(logical_size);
    let mut next_checkpoint =
        checkpoint_eligible.then(|| next_checkpoint_after(hash_offset, interval, logical_size));
    let mut journal_degraded = false;
    for range in ranges {
        check_cancel(request)?;
        let range_end = range.offset.checked_add(range.length).ok_or_else(|| {
            OperationError::semantic(
                ErrorCategory::Internal,
                "query_sparse",
                request.relative_path.to_path_buf(),
                "allocated source range overflowed",
            )
        })?;
        if range_end > logical_size {
            return Err(OperationError::semantic(
                ErrorCategory::Internal,
                "query_sparse",
                request.relative_path.to_path_buf(),
                "allocated source ranges were overlapping or out of bounds",
            ));
        }
        if range_end <= hash_offset {
            continue;
        }
        let range_start = range.offset.max(hash_offset);
        while hash_offset < range_start {
            let until_checkpoint = next_checkpoint
                .map_or(range_start - hash_offset, |boundary| boundary - hash_offset);
            let count = (range_start - hash_offset)
                .min(until_checkpoint)
                .min(buffer.len() as u64);
            hash_zero_count(&mut hasher, count);
            hash_offset += count;
            maybe_checkpoint_boundary(
                request,
                &mut temp,
                &mut journal,
                &mut next_checkpoint,
                &mut journal_degraded,
                interval,
                logical_size,
                hash_offset,
                hasher.as_ref(),
            )?;
        }
        source
            .seek(std::io::SeekFrom::Start(range_start))
            .map_err(|error| operation_error("seek_src", request.relative_path, &error))?;
        temp.seek(std::io::SeekFrom::Start(range_start))
            .map_err(|error| operation_error("seek_dst", request.relative_path, &error))?;
        let mut remaining = range_end - range_start;
        while remaining > 0 {
            check_cancel(request)?;
            let until_checkpoint =
                next_checkpoint.map_or(remaining, |boundary| boundary - hash_offset);
            let requested = usize::try_from(
                remaining.min(until_checkpoint).min(buffer.len() as u64),
            )
            .map_err(|_| {
                OperationError::semantic(
                    ErrorCategory::Internal,
                    "read_sparse",
                    request.relative_path.to_path_buf(),
                    "sparse request does not fit address space",
                )
            })?;
            let count = source
                .read(&mut buffer[..requested])
                .map_err(|error| operation_error("read_sparse", request.relative_path, &error))?;
            if count == 0 {
                return Err(OperationError::semantic(
                    ErrorCategory::SourceChanged,
                    "read_sparse",
                    request.relative_path.to_path_buf(),
                    "source ended inside an allocated range",
                ));
            }
            if let Some(hasher) = &mut hasher {
                hasher.update(&buffer[..count]);
            }
            temp.write_all(&buffer[..count])
                .map_err(|error| operation_error("write_sparse", request.relative_path, &error))?;
            counters.bytes_read_source = counters.bytes_read_source.saturating_add(count as u64);
            counters.bytes_written_destination = counters
                .bytes_written_destination
                .saturating_add(count as u64);
            remaining -= count as u64;
            hash_offset += count as u64;
            maybe_checkpoint_boundary(
                request,
                &mut temp,
                &mut journal,
                &mut next_checkpoint,
                &mut journal_degraded,
                interval,
                logical_size,
                hash_offset,
                hasher.as_ref(),
            )?;
        }
    }
    while hash_offset < logical_size {
        let until_checkpoint = next_checkpoint.map_or(logical_size - hash_offset, |boundary| {
            boundary - hash_offset
        });
        let count = (logical_size - hash_offset)
            .min(until_checkpoint)
            .min(buffer.len() as u64);
        hash_zero_count(&mut hasher, count);
        hash_offset += count;
        maybe_checkpoint_boundary(
            request,
            &mut temp,
            &mut journal,
            &mut next_checkpoint,
            &mut journal_degraded,
            interval,
            logical_size,
            hash_offset,
            hasher.as_ref(),
        )?;
    }
    post_read_validate(request, source)?;
    journal_degraded |= ensure_base_checkpoint_for_named(
        request,
        &mut temp,
        streams,
        logical_size,
        hasher.as_ref(),
        &mut journal,
    )?;
    let named = copy_named_streams(
        request,
        counters,
        &mut temp,
        streams,
        journal.as_deref_mut(),
    )?;
    journal_degraded |= named.journal_degraded;
    copy_eas(request, &temp, extended_attributes)?;
    post_read_validate(request, source)?;
    let dacl = precommit_validate(request)?;
    if let Some(dacl) = &dacl {
        temp.apply_protected_dacl(dacl)
            .map_err(|error| operation_error("preserve_dacl", request.relative_path, &error))?;
    }
    let checkpoint_used = temp.is_persistent();
    temp.commit(
        request.destination_path,
        request.replacement_snapshot.is_some(),
        request.destination_metadata,
        request.flush,
        request.destination_supports_posix_unlink_rename,
    )
    .map_err(|error| operation_error("commit", request.relative_path, &error))?;
    Ok(EngineResult {
        bytes: logical_size.saturating_add(named.bytes),
        digest: hasher.map(|value| format!("xxh3:{:032x}", value.digest128())),
        stream_digests: named.digests,
        ea_digest: (request.verify && !source_eas_dropped)
            .then(|| digest_bytes(extended_attributes.map_or(&[], |value| value.as_bytes()))),
        streams_dropped: named.dropped,
        eas_dropped: source_eas_dropped,
        journal_degraded,
        efs_downgraded,
        checkpoint_used,
    })
}

fn hash_zero_count(hasher: &mut Option<Xxh3>, mut length: u64) {
    static ZERO_PAGE: [u8; 64 * 1024] = [0; 64 * 1024];
    let Some(hasher) = hasher else {
        return;
    };
    while length > 0 {
        let count = usize::try_from(length.min(ZERO_PAGE.len() as u64)).unwrap_or(ZERO_PAGE.len());
        hasher.update(&ZERO_PAGE[..count]);
        length -= count as u64;
    }
}

fn copy_plain_small(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut SourceFile,
    should_hash: bool,
    streams_dropped: u32,
    eas_dropped: bool,
) -> Result<EngineResult, OperationError> {
    let expected = usize::try_from(request.source_snapshot.metadata.size).map_err(|_| {
        OperationError::semantic(
            ErrorCategory::Internal,
            "allocate_buffer",
            request.relative_path.to_path_buf(),
            "small-file size does not fit address space",
        )
    })?;
    let reservation = expected.checked_add(1).ok_or_else(|| {
        OperationError::semantic(
            ErrorCategory::Internal,
            "allocate_buffer",
            request.relative_path.to_path_buf(),
            "small-file sentinel buffer size overflowed",
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(reservation).map_err(|error| {
        OperationError::semantic(
            ErrorCategory::Internal,
            "allocate_buffer",
            request.relative_path.to_path_buf(),
            error.to_string(),
        )
    })?;
    let timer = std::time::Instant::now();
    // Bound a concurrently growing source to the enumerated size plus one
    // sentinel byte. The extra byte proves growth without letting a violated
    // stable-source assumption expand this worker's allocation without limit.
    source
        .by_ref()
        .take(request.source_snapshot.metadata.size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| operation_error("read", request.relative_path, &error))?;
    request.phases.record(2, timer.elapsed());
    counters.bytes_read_source = counters
        .bytes_read_source
        .saturating_add(bytes.len() as u64);
    if bytes.len() != expected {
        return Err(OperationError::semantic(
            ErrorCategory::SourceChanged,
            "read",
            request.relative_path.to_path_buf(),
            format!("expected {expected} bytes but read {}", bytes.len()),
        ));
    }
    post_read_validate(request, source)?;

    // ADR 0030: plain small files write directly to their final name. Atomic
    // publication measured at ~2× the AV-filter cost of a direct write on
    // small-file floods, and VISION's rerun contract makes the
    // partial-on-interrupt window acceptable. Routing guarantees this path
    // has only the unnamed stream and no EAs: any mid-write interruption is
    // shorter than the source, while a full write already completes the
    // file's logical payload. Auxiliary data uses atomic temp publication so
    // it cannot be hidden by the size+mtime rerun heuristic (ADR 0034).
    // Stamp timestamps+attributes at create (coalesced into the create's
    // MFT window — the measured 2 ms metadata round-trip on write-through
    // USB volumes disappears; the explicit set freezes mtime against writes
    // *on this handle*, and crash repair rides on the size check since the
    // replacement is truncated before the write). Destination revalidation
    // happens inside the create itself: `CREATE_NEW` polices a concurrently
    // appeared New name and a replacement is identity/metadata-checked on
    // the exact opened handle before truncation — a separate path-based
    // probe would add one metadata operation per file to this measured hot
    // path while proving strictly less than the same-handle check.
    let timer = std::time::Instant::now();
    let destination = create_final(request)?;
    request.phases.record(3, timer.elapsed());
    let efs_downgraded =
        wants_encryption(request) && !destination.basic_attributes().is_ok_and(is_encrypted);
    let mut destination = destination;
    let timer = std::time::Instant::now();
    destination
        .write_all(&bytes)
        .map_err(|error| operation_error("write", request.relative_path, &error))?;
    request.phases.record(4, timer.elapsed());
    counters.bytes_written_destination = counters
        .bytes_written_destination
        .saturating_add(bytes.len() as u64);
    post_read_validate(request, source)?;
    let timer = std::time::Instant::now();
    destination
        .finish(
            request.flush,
            request
                .destination_requires_post_write_stamp
                .then_some(request.destination_metadata),
        )
        .map_err(|error| operation_error("flush", request.relative_path, &error))?;
    request.phases.record(5, timer.elapsed());
    Ok(EngineResult {
        bytes: bytes.len() as u64,
        digest: should_hash.then(|| digest_bytes(&bytes)),
        stream_digests: Vec::new(),
        ea_digest: (request.verify && !eas_dropped).then(|| digest_bytes(&[])),
        streams_dropped,
        eas_dropped,
        journal_degraded: false,
        efs_downgraded,
        checkpoint_used: false,
    })
}

/// Opens the direct final-name writer, clearing a read-only attribute first
/// when the classification already saw it on the file being replaced.
fn create_final(request: &EngineRequest<'_>) -> Result<DestinationFinal, OperationError> {
    let encrypted = wants_encryption(request);
    let expected = request
        .replacement_snapshot
        .map(|snapshot| &snapshot.metadata);
    let stamp = request.destination_metadata;
    match DestinationFinal::create(request.destination_path, expected, encrypted, stamp) {
        Ok(value) => Ok(value),
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && request.replacement_snapshot.is_some() =>
        {
            let Some(snapshot) = request
                .replacement_snapshot
                .filter(|snapshot| is_readonly(snapshot.metadata.basic.attributes))
            else {
                return Err(operation_error("create_dst", request.relative_path, &error));
            };
            let mut cleared = snapshot.metadata.basic;
            cleared.attributes = without_readonly(cleared.attributes);
            set_basic_at_checked(
                request.destination_path,
                snapshot.metadata.identity,
                cleared,
            )
            .map_err(|error| operation_error("clear_readonly", request.relative_path, &error))?;
            let mut cleared_snapshot = snapshot.metadata.clone();
            cleared_snapshot.basic = cleared;
            match DestinationFinal::create(
                request.destination_path,
                Some(&cleared_snapshot),
                encrypted,
                stamp,
            ) {
                Ok(value) => Ok(value),
                Err(error) => {
                    // The retry did not take ownership of the file. Restore
                    // the exact object we cleared so a failed copy does not
                    // silently weaken an existing destination attribute.
                    let _ = set_basic_at_checked(
                        request.destination_path,
                        snapshot.metadata.identity,
                        snapshot.metadata.basic,
                    );
                    Err(operation_error("create_dst", request.relative_path, &error))
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(OperationError::from_io_as(
                ErrorCategory::DestinationChanged,
                "create_dst",
                request.relative_path.to_path_buf(),
                &error,
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            Err(OperationError::from_io_as(
                ErrorCategory::DestinationChanged,
                "revalidate_dst",
                request.relative_path.to_path_buf(),
                &error,
            ))
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && request.replacement_snapshot.is_some() =>
        {
            // The classified replacement target vanished before the open —
            // a destination mutation, not a missing-parent create failure.
            Err(OperationError::from_io_as(
                ErrorCategory::DestinationChanged,
                "revalidate_dst",
                request.relative_path.to_path_buf(),
                &error,
            ))
        }
        Err(error) => Err(operation_error("create_dst", request.relative_path, &error)),
    }
}

fn copy_streamed(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut SourceFile,
    streams: &[StreamInfo],
    extended_attributes: Option<&bigcp_win::ExtendedAttributes>,
    source_eas_dropped: bool,
    should_hash: bool,
    mut journal: Option<&mut Journal>,
) -> Result<EngineResult, OperationError> {
    let parent = destination_parent(request)?;
    let path_key = crate::journal::path_key(request.relative_path);
    let checkpoint = journal
        .as_deref()
        .and_then(|value| value.checkpoint_owned(&path_key, ""));
    let resumed = checkpoint
        .as_ref()
        .map(|checkpoint| resume_unnamed(request, counters, source, parent, checkpoint))
        .transpose()?
        .flatten();
    let (mut temp, mut total, mut hasher) = if let Some(state) = resumed {
        state
    } else {
        let temp = DestinationTemp::create(parent, request.run_id, wants_encryption(request))
            .map_err(|error| operation_error("create_dst_temp", request.relative_path, &error))?;
        temp.preallocate(request.source_snapshot.metadata.size)
            .map_err(|error| operation_error("preallocate", request.relative_path, &error))?;
        (temp, 0, should_hash.then(Xxh3::new))
    };
    let efs_downgraded = efs_downgraded_for(request, &temp);
    let mut buffer = vec![0_u8; request.chunk_bytes];
    if buffer.is_empty() {
        return Err(OperationError::semantic(
            ErrorCategory::Internal,
            "allocate_buffer",
            request.relative_path.to_path_buf(),
            "the composed copy profile selected a zero-byte chunk",
        ));
    }
    let checkpoint_eligible =
        request.source_snapshot.metadata.size >= request.checkpoint_threshold && journal.is_some();
    let interval = checkpoint_interval(request.source_snapshot.metadata.size);
    let mut next_checkpoint = checkpoint_eligible
        .then(|| next_checkpoint_after(total, interval, request.source_snapshot.metadata.size));
    let mut journal_degraded = false;
    while total < request.source_snapshot.metadata.size {
        check_cancel(request)?;
        let remaining = request.source_snapshot.metadata.size - total;
        let until_checkpoint = next_checkpoint.map_or(remaining, |boundary| boundary - total);
        let requested = usize::try_from(remaining.min(until_checkpoint).min(buffer.len() as u64))
            .map_err(|_| {
            OperationError::semantic(
                ErrorCategory::Internal,
                "read",
                request.relative_path.to_path_buf(),
                "stream request does not fit address space",
            )
        })?;
        let count = source
            .read(&mut buffer[..requested])
            .map_err(|error| operation_error("read", request.relative_path, &error))?;
        if count == 0 {
            return Err(OperationError::semantic(
                ErrorCategory::SourceChanged,
                "read",
                request.relative_path.to_path_buf(),
                "source ended before its enumerated size",
            ));
        }
        counters.bytes_read_source = counters.bytes_read_source.saturating_add(count as u64);
        total = total.saturating_add(count as u64);
        if let Some(hasher) = &mut hasher {
            hasher.update(&buffer[..count]);
        }
        temp.write_all(&buffer[..count])
            .map_err(|error| operation_error("write", request.relative_path, &error))?;
        counters.bytes_written_destination = counters
            .bytes_written_destination
            .saturating_add(count as u64);
        if next_checkpoint == Some(total) {
            match append_checkpoint(
                journal.as_deref_mut(),
                &mut temp,
                request,
                "",
                request.source_snapshot.metadata.size,
                total,
                hasher.as_ref(),
            )? {
                CheckpointStatus::Written => {
                    next_checkpoint = (total < request.source_snapshot.metadata.size).then(|| {
                        next_checkpoint_after(
                            total,
                            interval,
                            request.source_snapshot.metadata.size,
                        )
                    });
                }
                CheckpointStatus::Disabled => {
                    journal = None;
                    next_checkpoint = None;
                    journal_degraded = true;
                }
                CheckpointStatus::NotConfigured => next_checkpoint = None,
            }
        }
    }
    post_read_validate(request, source)?;
    journal_degraded |= ensure_base_checkpoint_for_named(
        request,
        &mut temp,
        streams,
        total,
        hasher.as_ref(),
        &mut journal,
    )?;
    let named = copy_named_streams(
        request,
        counters,
        &mut temp,
        streams,
        journal.as_deref_mut(),
    )?;
    journal_degraded |= named.journal_degraded;
    copy_eas(request, &temp, extended_attributes)?;
    post_read_validate(request, source)?;
    let dacl = precommit_validate(request)?;
    if let Some(dacl) = &dacl {
        temp.apply_protected_dacl(dacl)
            .map_err(|error| operation_error("preserve_dacl", request.relative_path, &error))?;
    }
    let checkpoint_used = temp.is_persistent();
    temp.commit(
        request.destination_path,
        request.replacement_snapshot.is_some(),
        request.destination_metadata,
        request.flush,
        request.destination_supports_posix_unlink_rename,
    )
    .map_err(|error| operation_error("commit", request.relative_path, &error))?;
    Ok(EngineResult {
        bytes: total.saturating_add(named.bytes),
        digest: hasher.map(|value| format!("xxh3:{:032x}", value.digest128())),
        stream_digests: named.digests,
        ea_digest: (request.verify && !source_eas_dropped)
            .then(|| digest_bytes(extended_attributes.map_or(&[], |value| value.as_bytes()))),
        streams_dropped: named.dropped,
        eas_dropped: source_eas_dropped,
        journal_degraded,
        efs_downgraded,
        checkpoint_used,
    })
}

fn resume_unnamed(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut SourceFile,
    parent: &Path,
    checkpoint: &Checkpoint,
) -> Result<Option<(DestinationTemp, u64, Option<Xxh3>)>, OperationError> {
    let source_matches = checkpoint.source_size == request.source_snapshot.metadata.size
        && checkpoint.source_mtime == request.source_snapshot.metadata.basic.last_write_time
        && checkpoint
            .source_identity
            .as_ref()
            .is_some_and(|identity| identity.matches(request.source_snapshot.metadata.identity))
        && checkpoint.watermark <= checkpoint.source_size;
    let Ok(mut temp) = DestinationTemp::resume(parent, &checkpoint.temp_name) else {
        // Checkpoints are optional hints. A missing, inaccessible, or
        // type-changed candidate must not prevent a clean copy through a new
        // opaque temporary.
        return Ok(None);
    };
    if !checkpoint_matches_temp(checkpoint, &temp) {
        // The handle is intentionally still persistent. Without identity
        // proof, this process has no authority to mutate or delete the object.
        return Ok(None);
    }
    let temp_is_long_enough = temp
        .len()
        .is_ok_and(|length| length >= checkpoint.watermark);
    if !source_matches || !temp_is_long_enough {
        temp.discard().map_err(|error| {
            operation_error("discard_resume_temp", request.relative_path, &error)
        })?;
        return Ok(None);
    }

    temp.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| operation_error("seek_resume_temp", request.relative_path, &error))?;
    let mut hasher = Xxh3::new();
    let mut remaining = checkpoint.watermark;
    let mut buffer = vec![0_u8; request.chunk_bytes.max(64 * 1024)];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            OperationError::semantic(
                ErrorCategory::Internal,
                "verify_resume_temp",
                request.relative_path.to_path_buf(),
                "resume verification request does not fit address space",
            )
        })?;
        let count = temp.read(&mut buffer[..requested]).map_err(|error| {
            operation_error("verify_resume_temp", request.relative_path, &error)
        })?;
        if count == 0 {
            temp.discard().map_err(|error| {
                operation_error("discard_resume_temp", request.relative_path, &error)
            })?;
            return Ok(None);
        }
        hasher.update(&buffer[..count]);
        counters.bytes_verified = counters.bytes_verified.saturating_add(count as u64);
        remaining -= count as u64;
    }
    let digest = format!("xxh3:{:032x}", hasher.digest128());
    if digest != checkpoint.prefix_digest {
        temp.discard().map_err(|error| {
            operation_error("discard_resume_temp", request.relative_path, &error)
        })?;
        return Ok(None);
    }
    temp.set_len(checkpoint.watermark)
        .map_err(|error| operation_error("truncate_resume_temp", request.relative_path, &error))?;
    temp.seek(std::io::SeekFrom::Start(checkpoint.watermark))
        .map_err(|error| operation_error("seek_resume_temp", request.relative_path, &error))?;
    source
        .seek(std::io::SeekFrom::Start(checkpoint.watermark))
        .map_err(|error| operation_error("seek_src", request.relative_path, &error))?;
    Ok(Some((temp, checkpoint.watermark, Some(hasher))))
}

#[allow(clippy::too_many_arguments)]
fn maybe_checkpoint_boundary(
    request: &EngineRequest<'_>,
    temp: &mut DestinationTemp,
    journal: &mut Option<&mut Journal>,
    next_checkpoint: &mut Option<u64>,
    journal_degraded: &mut bool,
    interval: u64,
    logical_size: u64,
    offset: u64,
    hasher: Option<&Xxh3>,
) -> Result<(), OperationError> {
    if *next_checkpoint != Some(offset) {
        return Ok(());
    }
    match append_checkpoint(
        journal.as_deref_mut(),
        temp,
        request,
        "",
        logical_size,
        offset,
        hasher,
    )? {
        CheckpointStatus::Written => {
            *next_checkpoint = (offset < logical_size)
                .then(|| next_checkpoint_after(offset, interval, logical_size));
        }
        CheckpointStatus::Disabled => {
            *journal = None;
            *next_checkpoint = None;
            *journal_degraded = true;
        }
        CheckpointStatus::NotConfigured => *next_checkpoint = None,
    }
    Ok(())
}

fn ensure_base_checkpoint_for_named(
    request: &EngineRequest<'_>,
    temp: &mut DestinationTemp,
    streams: &[StreamInfo],
    base_size: u64,
    base_hasher: Option<&Xxh3>,
    journal: &mut Option<&mut Journal>,
) -> Result<bool, OperationError> {
    if !request.destination_supports_streams {
        return Ok(false);
    }
    let named_is_resumable = streams
        .iter()
        .any(|stream| !stream.is_unnamed() && stream.size >= request.checkpoint_threshold);
    if !named_is_resumable || journal.is_none() || temp.is_persistent() {
        return Ok(false);
    }
    match append_checkpoint(
        journal.as_deref_mut(),
        temp,
        request,
        "",
        request.source_snapshot.metadata.size,
        base_size,
        base_hasher,
    )? {
        CheckpointStatus::Disabled => {
            *journal = None;
            Ok(true)
        }
        CheckpointStatus::Written | CheckpointStatus::NotConfigured => Ok(false),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckpointStatus {
    Written,
    Disabled,
    NotConfigured,
}

fn append_checkpoint(
    journal: Option<&mut Journal>,
    temp: &mut DestinationTemp,
    request: &EngineRequest<'_>,
    stream: &str,
    source_size: u64,
    watermark: u64,
    hasher: Option<&Xxh3>,
) -> Result<CheckpointStatus, OperationError> {
    let Some(journal) = journal else {
        return Ok(CheckpointStatus::NotConfigured);
    };
    let Some(hasher) = hasher else {
        return Err(OperationError::semantic(
            ErrorCategory::Internal,
            "checkpoint",
            request.relative_path.to_path_buf(),
            "checkpoint-eligible stream did not have an in-flight digest",
        ));
    };
    let temp_name = temp
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            OperationError::semantic(
                ErrorCategory::Internal,
                "checkpoint",
                request.relative_path.to_path_buf(),
                "owned temporary name was not valid UTF-8",
            )
        })?
        .to_owned();
    let Ok(temp_identity) = temp.identity() else {
        return Ok(CheckpointStatus::Disabled);
    };
    let checkpoint = Checkpoint {
        relative_path: crate::journal::path_key(request.relative_path),
        stream: stream.to_owned(),
        temp_name,
        temp_identity: Some(CheckpointFileIdentity::from_file(temp_identity)),
        source_identity: Some(CheckpointFileIdentity::from_file(
            request.source_snapshot.metadata.identity,
        )),
        source_size,
        source_mtime: request.source_snapshot.metadata.basic.last_write_time,
        watermark,
        prefix_digest: format!("xxh3:{:032x}", hasher.digest128()),
    };
    if journal
        .append(JournalEvent::Checkpoint(checkpoint))
        .is_err()
    {
        return Ok(CheckpointStatus::Disabled);
    }
    if temp.persist_for_resume().is_err() {
        // Checkpointing is an optional acceleration. The still-delete-pending
        // temp remains safe, and the just-written hint will miss on rerun.
        return Ok(CheckpointStatus::Disabled);
    }
    Ok(CheckpointStatus::Written)
}

fn checkpoint_interval(logical_size: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if logical_size >= 1024 * GIB {
        4 * GIB
    } else if logical_size >= 64 * GIB {
        GIB
    } else {
        256 * MIB
    }
}

fn next_checkpoint_after(offset: u64, interval: u64, size: u64) -> u64 {
    offset
        .checked_div(interval)
        .unwrap_or(0)
        .saturating_add(1)
        .saturating_mul(interval)
        .min(size)
}

fn copy_eas(
    request: &EngineRequest<'_>,
    temp: &DestinationTemp,
    attributes: Option<&bigcp_win::ExtendedAttributes>,
) -> Result<(), OperationError> {
    if let Some(attributes) = attributes {
        temp.write_extended_attributes(attributes)
            .map_err(|error| operation_error("write_ea", request.relative_path, &error))?;
    }
    Ok(())
}

/// Requests EFS on a new temp only when the source is encrypted and the
/// destination volume supports it. Creation is the only reliable moment to
/// apply EFS: an armed delete disposition refuses every later path-based open.
fn wants_encryption(request: &EngineRequest<'_>) -> bool {
    is_encrypted(request.source_snapshot.metadata.basic.attributes)
        && request.destination_supports_encryption
}

/// Reports whether an encrypted source is landing unencrypted.
///
/// Judged from the live temp handle so freshly created and resumed temps get
/// the same answer; the create-time EFS request is best-effort (system policy
/// can disable EFS silently), so the temp's actual attribute is the truth.
fn efs_downgraded_for(request: &EngineRequest<'_>, temp: &DestinationTemp) -> bool {
    if !is_encrypted(request.source_snapshot.metadata.basic.attributes) {
        return false;
    }
    !temp.basic_attributes().is_ok_and(is_encrypted)
}

struct NamedStreamResult {
    digests: Vec<(StreamInfo, String)>,
    dropped: u32,
    journal_degraded: bool,
    bytes: u64,
}

fn copy_named_streams(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    temp: &mut DestinationTemp,
    streams: &[StreamInfo],
    mut journal: Option<&mut Journal>,
) -> Result<NamedStreamResult, OperationError> {
    let named: Vec<&StreamInfo> = streams
        .iter()
        .filter(|stream| !stream.is_unnamed())
        .collect();
    if !request.destination_supports_streams {
        return Ok(NamedStreamResult {
            digests: Vec::new(),
            dropped: u32::try_from(named.len()).unwrap_or(u32::MAX),
            journal_degraded: false,
            bytes: 0,
        });
    }
    let mut digests = Vec::new();
    let mut journal_degraded = false;
    let mut logical_bytes = 0_u64;
    for stream in named {
        let mut source = SourceStream::open(request.source_path, stream)
            .map_err(|error| source_open_error("open_src_stream", request.relative_path, &error))?;
        let stream_identity = source.identity().map_err(|error| {
            source_open_error("identify_src_stream", request.relative_path, &error)
        })?;
        if stream_identity != request.source_snapshot.metadata.identity {
            return Err(OperationError::semantic(
                ErrorCategory::SourceChanged,
                "identify_src_stream",
                request.relative_path.to_path_buf(),
                "named stream belongs to a different source object",
            ));
        }
        let key = crate::journal::stream_key(&stream.name);
        let candidate = journal.as_deref().and_then(|value| {
            value.checkpoint_owned(&crate::journal::path_key(request.relative_path), &key)
        });
        let resumed = candidate
            .as_ref()
            .filter(|checkpoint| {
                temp.path().file_name().and_then(|name| name.to_str())
                    == Some(checkpoint.temp_name.as_str())
            })
            .map(|checkpoint| {
                resume_named(request, counters, temp, stream, &mut source, checkpoint)
            })
            .transpose()?
            .flatten();
        let (mut destination, mut copied, mut hasher) = if let Some(state) = resumed {
            state
        } else {
            (
                temp.create_stream(stream).map_err(|error| {
                    operation_error("create_dst_stream", request.relative_path, &error)
                })?,
                0,
                (request.verify || stream.size >= request.large_threshold).then(Xxh3::new),
            )
        };
        let mut buffer = vec![0_u8; request.chunk_bytes.clamp(64 * 1024, 8 * 1024 * 1024)];
        let checkpoint_eligible = stream.size >= request.checkpoint_threshold && journal.is_some();
        let interval = checkpoint_interval(stream.size);
        let mut next_checkpoint =
            checkpoint_eligible.then(|| next_checkpoint_after(copied, interval, stream.size));
        while copied < stream.size {
            check_cancel(request)?;
            let remaining = stream.size - copied;
            let until_checkpoint = next_checkpoint.map_or(remaining, |boundary| boundary - copied);
            let requested = usize::try_from(
                remaining.min(until_checkpoint).min(buffer.len() as u64),
            )
            .map_err(|_| {
                OperationError::semantic(
                    ErrorCategory::Internal,
                    "read_stream",
                    request.relative_path.to_path_buf(),
                    "named-stream request does not fit address space",
                )
            })?;
            let count = source
                .read(&mut buffer[..requested])
                .map_err(|error| operation_error("read_stream", request.relative_path, &error))?;
            if count == 0 {
                return Err(OperationError::semantic(
                    ErrorCategory::SourceChanged,
                    "read_stream",
                    request.relative_path.to_path_buf(),
                    "named stream ended before its enumerated size",
                ));
            }
            counters.bytes_read_source = counters.bytes_read_source.saturating_add(count as u64);
            copied = copied.saturating_add(count as u64);
            if let Some(hasher) = &mut hasher {
                hasher.update(&buffer[..count]);
            }
            destination
                .write_all(&buffer[..count])
                .map_err(|error| operation_error("write_stream", request.relative_path, &error))?;
            counters.bytes_written_destination = counters
                .bytes_written_destination
                .saturating_add(count as u64);
            if next_checkpoint == Some(copied) {
                match append_checkpoint(
                    journal.as_deref_mut(),
                    temp,
                    request,
                    &key,
                    stream.size,
                    copied,
                    hasher.as_ref(),
                )? {
                    CheckpointStatus::Written => {
                        next_checkpoint = (copied < stream.size)
                            .then(|| next_checkpoint_after(copied, interval, stream.size));
                    }
                    CheckpointStatus::Disabled => {
                        journal = None;
                        next_checkpoint = None;
                        journal_degraded = true;
                    }
                    CheckpointStatus::NotConfigured => next_checkpoint = None,
                }
            }
        }
        destination
            .flush()
            .map_err(|error| operation_error("flush_stream", request.relative_path, &error))?;
        if let Some(hasher) = hasher {
            digests.push((stream.clone(), format!("xxh3:{:032x}", hasher.digest128())));
        }
        logical_bytes = logical_bytes.saturating_add(stream.size);
    }
    digests.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    Ok(NamedStreamResult {
        digests,
        dropped: 0,
        journal_degraded,
        bytes: logical_bytes,
    })
}

fn resume_named(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    temp: &DestinationTemp,
    stream: &StreamInfo,
    source: &mut SourceStream,
    checkpoint: &Checkpoint,
) -> Result<Option<(DestinationStream, u64, Option<Xxh3>)>, OperationError> {
    if checkpoint.source_size != stream.size
        || checkpoint.source_mtime != request.source_snapshot.metadata.basic.last_write_time
        || !checkpoint
            .source_identity
            .as_ref()
            .is_some_and(|identity| identity.matches(request.source_snapshot.metadata.identity))
        || checkpoint.watermark > stream.size
    {
        return Ok(None);
    }
    if !checkpoint_matches_temp(checkpoint, temp) {
        return Ok(None);
    }
    let mut destination = temp
        .resume_stream(stream)
        .map_err(|error| operation_error("open_resume_stream", request.relative_path, &error))?;
    if destination
        .len()
        .map_err(|error| operation_error("stat_resume_stream", request.relative_path, &error))?
        < checkpoint.watermark
    {
        return Ok(None);
    }
    destination
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|error| operation_error("seek_resume_stream", request.relative_path, &error))?;
    let mut hasher = Xxh3::new();
    let mut remaining = checkpoint.watermark;
    let mut buffer = vec![0_u8; request.chunk_bytes.clamp(64 * 1024, 8 * 1024 * 1024)];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            OperationError::semantic(
                ErrorCategory::Internal,
                "verify_resume_stream",
                request.relative_path.to_path_buf(),
                "named resume verification request does not fit address space",
            )
        })?;
        let count = destination
            .read(&mut buffer[..requested])
            .map_err(|error| {
                operation_error("verify_resume_stream", request.relative_path, &error)
            })?;
        if count == 0 {
            return Ok(None);
        }
        hasher.update(&buffer[..count]);
        counters.bytes_verified = counters.bytes_verified.saturating_add(count as u64);
        remaining -= count as u64;
    }
    if format!("xxh3:{:032x}", hasher.digest128()) != checkpoint.prefix_digest {
        return Ok(None);
    }
    destination.set_len(checkpoint.watermark).map_err(|error| {
        operation_error("truncate_resume_stream", request.relative_path, &error)
    })?;
    destination
        .seek(std::io::SeekFrom::Start(checkpoint.watermark))
        .map_err(|error| operation_error("seek_resume_stream", request.relative_path, &error))?;
    source
        .seek(std::io::SeekFrom::Start(checkpoint.watermark))
        .map_err(|error| operation_error("seek_src_stream", request.relative_path, &error))?;
    Ok(Some((destination, checkpoint.watermark, Some(hasher))))
}

fn post_read_validate(
    request: &EngineRequest<'_>,
    source: &SourceFile,
) -> Result<(), OperationError> {
    let current = source
        .current_metadata()
        .map_err(|error| operation_error("revalidate_src", request.relative_path, &error))?;
    ensure_source_unchanged(
        request.source_snapshot,
        &current,
        request.relative_path,
        "revalidate_src",
    )
}

fn ensure_source_unchanged(
    expected: &EntrySnapshot,
    observed: &ObjectMetadata,
    relative_path: &Path,
    operation: &str,
) -> Result<(), OperationError> {
    if expected.metadata.identity != observed.identity
        || expected.metadata.size != observed.size
        || expected.metadata.basic.last_write_time != observed.basic.last_write_time
    {
        return Err(OperationError::semantic(
            ErrorCategory::SourceChanged,
            operation,
            relative_path.to_path_buf(),
            "source identity, size, or last-write time changed after enumeration",
        ));
    }
    Ok(())
}

fn precommit_validate(
    request: &EngineRequest<'_>,
) -> Result<Option<bigcp_win::ProtectedDacl>, OperationError> {
    revalidate_destination(request)?;
    let Some(expected) = request.replacement_snapshot else {
        return Ok(None);
    };
    if !request.destination_supports_persistent_acls {
        return Ok(None);
    }
    read_protected_dacl_checked(request.destination_path, expected.metadata.identity).map_err(
        |error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                OperationError::from_io_as(
                    ErrorCategory::DestinationChanged,
                    "read_dacl",
                    request.relative_path.to_path_buf(),
                    &error,
                )
            } else {
                operation_error("read_dacl", request.relative_path, &error)
            }
        },
    )
}

fn revalidate_destination(request: &EngineRequest<'_>) -> Result<(), OperationError> {
    let Some(expected) = request.replacement_snapshot else {
        match metadata_at(request.destination_path) {
            Ok(_) => {
                return Err(OperationError::semantic(
                    ErrorCategory::DestinationChanged,
                    "revalidate_dst",
                    request.relative_path.to_path_buf(),
                    "a destination object appeared after classification",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(operation_error(
                    "revalidate_dst",
                    request.relative_path,
                    &error,
                ));
            }
        }
        return Ok(());
    };
    let observed = match metadata_at(request.destination_path) {
        Ok(observed) => observed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(OperationError::semantic(
                ErrorCategory::DestinationChanged,
                "revalidate_dst",
                request.relative_path.to_path_buf(),
                "destination disappeared after classification",
            ));
        }
        Err(error) => {
            return Err(operation_error(
                "revalidate_dst",
                request.relative_path,
                &error,
            ));
        }
    };
    if expected.metadata.identity != observed.identity
        || expected.metadata.kind != observed.kind
        || expected.metadata.size != observed.size
        || expected.metadata.basic.last_write_time != observed.basic.last_write_time
        || expected.metadata.basic.attributes != observed.basic.attributes
        || expected.metadata.reparse_tag != observed.reparse_tag
    {
        return Err(OperationError::semantic(
            ErrorCategory::DestinationChanged,
            "revalidate_dst",
            request.relative_path.to_path_buf(),
            "destination identity or metadata changed after classification",
        ));
    }
    Ok(())
}

fn destination_parent<'a>(request: &'a EngineRequest<'_>) -> Result<&'a Path, OperationError> {
    request.destination_path.parent().ok_or_else(|| {
        OperationError::semantic(
            ErrorCategory::Path,
            "create_dst_temp",
            request.relative_path.to_path_buf(),
            "destination file has no parent directory",
        )
    })
}

/// Proves that a resume candidate is still the exact temporary captured by
/// the journal. An identity query failure is a cache miss, never authority to
/// mutate the path named by a checkpoint.
fn checkpoint_matches_temp(checkpoint: &Checkpoint, temp: &DestinationTemp) -> bool {
    checkpoint
        .temp_identity
        .as_ref()
        .is_some_and(|expected| temp.identity().is_ok_and(|actual| expected.matches(actual)))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("xxh3:{:032x}", xxhash_rust::xxh3::xxh3_128(bytes))
}

fn operation_error(
    operation: &str,
    relative_path: &Path,
    error: &std::io::Error,
) -> OperationError {
    OperationError::from_io(operation, PathBuf::from(relative_path), error)
}

/// Operation name marking a graceful mid-file cancellation. The coordinator
/// converts errors carrying it into a not-attempted outcome instead of a
/// failure: a clean cancel is not an error condition.
pub(crate) const CANCELED_MID_FILE: &str = "canceled_mid_file";

/// Operation name marking a worker hand-back of a file whose discovered
/// stream set deserves inline streaming (see `EngineRequest::promote_threshold`).
/// Not a failure: the coordinator reruns the file and records the real outcome.
pub(crate) const PROMOTED_TO_COORDINATOR: &str = "promoted_to_coordinator";

fn check_cancel(request: &EngineRequest<'_>) -> Result<(), OperationError> {
    if (request.cancel)() {
        Err(OperationError::semantic(
            ErrorCategory::Internal,
            CANCELED_MID_FILE,
            request.relative_path.to_path_buf(),
            "graceful cancellation stopped this copy between chunks",
        ))
    } else {
        Ok(())
    }
}

fn source_open_error(
    operation: &str,
    relative_path: &Path,
    error: &std::io::Error,
) -> OperationError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
    ) {
        OperationError::from_io_as(
            ErrorCategory::SourceChanged,
            operation,
            relative_path.to_path_buf(),
            error,
        )
    } else {
        operation_error(operation, relative_path, error)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use bigcp_win::StreamInfo;

    use super::{StreamRouting, route_streams};

    #[test]
    fn dropped_large_ads_does_not_promote_or_checkpoint_plain_data() {
        let streams = [
            StreamInfo::unnamed(4 * 1024),
            StreamInfo {
                name: OsString::from(":large:$DATA"),
                size: 8 * 1024 * 1024,
            },
        ];

        assert_eq!(
            route_streams(&streams, 4 * 1024, false, true, 1024 * 1024),
            StreamRouting {
                largest_representable: 4 * 1024,
                checkpoint_eligible: false,
                named_streams: 1,
                named_streams_dropped: 1,
            }
        );
        assert_eq!(
            route_streams(&streams, 4 * 1024, true, true, 1024 * 1024),
            StreamRouting {
                largest_representable: 8 * 1024 * 1024,
                checkpoint_eligible: true,
                named_streams: 1,
                named_streams_dropped: 0,
            }
        );
    }
}
