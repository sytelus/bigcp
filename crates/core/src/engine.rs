//! Shared semantic file engine.
//!
//! Plain small files are fully read and source-revalidated before the
//! destination is touched, then written directly to their final name
//! (rerun-repair crash contract, ADR 0030). Files with destination-representable
//! auxiliary streams or EAs and all large files use an opaque temporary so one
//! logical file is published atomically (ADRs 0034/0035).

use std::borrow::Cow;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use bigcp_win::{
    DestinationFinal, DestinationStream, DestinationTemp, EndpointKind, FileIdentity,
    ObjectMetadata, RelativeDirectory, SegmentWriter, SourceFile, SourceStream, StreamInfo,
    is_encrypted, is_readonly, is_sparse, list_streams, metadata_at,
    read_extended_attributes_checked, read_protected_dacl_checked, set_basic_at_checked,
    without_readonly,
};
use xxhash_rust::xxh3::Xxh3;

use crate::error::{ErrorCategory, OperationError};
use crate::filesystem::FilesystemPolicy;
use crate::journal::{Checkpoint, CheckpointFileIdentity, Journal, JournalEvent};
use crate::model::{Counters, EntrySnapshot};
use crate::phase::PhaseTracker;
use crate::transport::{
    BurstBuffer, CancelProbe, PipelinedFailureKind, TransferFailureKind, TransportProfile,
    transfer_pipelined,
};

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

/// Destination capability facts fixed for the whole run.
///
/// Constructed only by [`DestinationCaps::from_policy`] so the projection
/// from [`FilesystemPolicy`] onto engine behavior lives in exactly one place.
#[derive(Clone, Copy)]
pub struct DestinationCaps {
    /// Whether the destination can represent named streams.
    supports_streams: bool,
    /// Whether the destination volume advertises extended-attribute support.
    supports_eas: bool,
    /// Whether the destination advertises EFS support.
    supports_encryption: bool,
    /// Whether local-volume dense allocation hints are appropriate.
    supports_preallocation: bool,
    /// Whether protected destination DACLs can exist and need preservation.
    supports_persistent_acls: bool,
    /// Whether publication can use POSIX unlink/rename semantics.
    supports_posix_unlink_rename: bool,
    /// Whether data writes require a final FAT-family/remote metadata restamp.
    requires_post_write_stamp: bool,
    /// Whether the destination crosses WSL's Plan 9 provider.
    ///
    /// WSL uses this only for provider-specific create hints and to avoid an
    /// initial metadata round trip that the required post-write stamp would
    /// immediately supersede. Other endpoint mechanics remain unchanged.
    is_wsl: bool,
}

impl DestinationCaps {
    /// Projects the run's destination policy onto the engine's capability
    /// facts. This is the only constructor: every field derives from the
    /// policy here, so a capability can never be set independently at a
    /// construction site.
    #[must_use]
    pub fn from_policy(policy: &FilesystemPolicy) -> Self {
        Self {
            supports_streams: policy.supports_streams(),
            supports_eas: policy.supports_eas(),
            supports_encryption: policy.supports_encryption(),
            supports_preallocation: policy.supports_preallocation(),
            supports_persistent_acls: policy.supports_persistent_acls(),
            supports_posix_unlink_rename: policy.supports_posix_unlink_rename(),
            requires_post_write_stamp: policy.requires_post_write_stamp(),
            is_wsl: policy.endpoint() == EndpointKind::Wsl,
        }
    }
}

/// Source-side capability facts fixed for the whole run.
#[derive(Clone, Copy)]
pub struct SourceCaps {
    /// Whether source stream discovery is meaningful on this filesystem.
    pub supports_streams: bool,
    /// Whether source EA reads are meaningful on this filesystem.
    pub supports_eas: bool,
    /// Whether the source crosses WSL's Plan 9 provider.
    ///
    /// Used only to select provider-aware scheduling (small-file striping in
    /// `copy::worker_dispatch` and the segmented large-file strategy's
    /// one-side-is-WSL eligibility); source read semantics are unchanged.
    pub is_wsl: bool,
}

/// Run-constant transfer tuning shared by every engine call.
#[derive(Clone, Copy)]
pub struct EngineTuning {
    /// Composed profile chunk bytes.
    pub chunk_bytes: usize,
    /// Size threshold between pre-read and streamed strategies.
    pub large_threshold: u64,
    /// Minimum per-stream size eligible for partial resume checkpoints.
    pub checkpoint_threshold: u64,
    /// Preflight-selected buffered transport policy.
    pub transport: TransportProfile,
    /// Hash small files for post-copy verification.
    pub verify: bool,
    /// Flush final data and metadata.
    pub flush: bool,
}

/// Run-constant engine parameters constructed once at run setup and shared by
/// the coordinator and every worker.
pub struct EngineSettings {
    /// Unique run identifier for temp ownership.
    pub run_id: String,
    /// Transfer tuning knobs.
    pub tuning: EngineTuning,
    /// Source-side capability facts.
    pub source: SourceCaps,
    /// Destination-side capability facts.
    pub destination: DestinationCaps,
}

/// Parameters shared by the product engine's direct and transactional strategies.
pub struct EngineRequest<'a> {
    /// Absolute source path.
    pub source_path: &'a Path,
    /// Absolute final destination path.
    pub destination_path: &'a Path,
    /// Verified parent capability for the local NTFS plain-small hot path.
    ///
    /// Transactional, remote, degraded-filesystem, and inline callers pass
    /// `None` and retain ordinary absolute-path opens.
    pub relative_destination_parent: Option<&'a RelativeDirectory>,
    /// Relative source identity for errors.
    pub relative_path: &'a Path,
    /// Enumeration snapshot.
    pub source_snapshot: &'a EntrySnapshot,
    /// Replacement snapshot, if any.
    pub replacement_snapshot: Option<&'a EntrySnapshot>,
    /// Run-constant settings shared by the coordinator and every worker.
    pub settings: &'a EngineSettings,
    /// Whether sparse layout can and should be preserved.
    pub preserve_sparse: bool,
    /// Source metadata projected to fields the destination can represent.
    pub destination_metadata: bigcp_win::BasicMetadata,
    /// Graceful-cancel probe checked between chunks so very large files do
    /// not delay a requested stop until they finish (the in-flight temp
    /// self-deletes or resumes from its last verified checkpoint).
    pub cancel: &'a dyn CancelProbe,
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

/// Accessors forwarding to the shared [`EngineSettings`], keeping engine call
/// sites as short as the former per-request fields.
impl EngineRequest<'_> {
    /// Unique run identifier for temp ownership.
    #[inline]
    fn run_id(&self) -> &str {
        &self.settings.run_id
    }

    /// Size threshold between pre-read and streamed strategies.
    #[inline]
    fn large_threshold(&self) -> u64 {
        self.settings.tuning.large_threshold
    }

    /// Hash small files for post-copy verification.
    #[inline]
    fn verify(&self) -> bool {
        self.settings.tuning.verify
    }

    /// Flush final data and metadata.
    #[inline]
    fn flush(&self) -> bool {
        self.settings.tuning.flush
    }

    /// Composed profile chunk bytes.
    #[inline]
    fn chunk_bytes(&self) -> usize {
        self.settings.tuning.chunk_bytes
    }

    /// Preflight-selected buffered transport policy.
    #[inline]
    fn transport(&self) -> TransportProfile {
        self.settings.tuning.transport
    }

    /// Minimum per-stream size eligible for partial resume checkpoints.
    #[inline]
    fn checkpoint_threshold(&self) -> u64 {
        self.settings.tuning.checkpoint_threshold
    }

    /// Whether source stream discovery is meaningful on this filesystem.
    #[inline]
    fn source_supports_streams(&self) -> bool {
        self.settings.source.supports_streams
    }

    /// Whether source EA reads are meaningful on this filesystem.
    #[inline]
    fn source_supports_eas(&self) -> bool {
        self.settings.source.supports_eas
    }

    /// Whether the source crosses WSL's Plan 9 provider (see [`SourceCaps`]).
    #[inline]
    fn source_is_wsl(&self) -> bool {
        self.settings.source.is_wsl
    }

    /// Whether the destination can represent named streams.
    #[inline]
    fn destination_supports_streams(&self) -> bool {
        self.settings.destination.supports_streams
    }

    /// Whether the destination volume advertises extended-attribute support.
    #[inline]
    fn destination_supports_eas(&self) -> bool {
        self.settings.destination.supports_eas
    }

    /// Whether the destination advertises EFS support.
    #[inline]
    fn destination_supports_encryption(&self) -> bool {
        self.settings.destination.supports_encryption
    }

    /// Whether local-volume dense allocation hints are appropriate.
    #[inline]
    fn destination_supports_preallocation(&self) -> bool {
        self.settings.destination.supports_preallocation
    }

    /// Whether protected destination DACLs can exist and need preservation.
    #[inline]
    fn destination_supports_persistent_acls(&self) -> bool {
        self.settings.destination.supports_persistent_acls
    }

    /// Whether publication can use POSIX unlink/rename semantics.
    #[inline]
    fn destination_supports_posix_unlink_rename(&self) -> bool {
        self.settings.destination.supports_posix_unlink_rename
    }

    /// Whether data writes require a final FAT-family/remote metadata restamp.
    #[inline]
    fn destination_requires_post_write_stamp(&self) -> bool {
        self.settings.destination.requires_post_write_stamp
    }

    /// Whether the destination crosses WSL's Plan 9 provider (see
    /// [`DestinationCaps`]).
    #[inline]
    fn destination_is_wsl(&self) -> bool {
        self.settings.destination.is_wsl
    }
}

/// Copies one ordinary file through the fidelity- and size-appropriate
/// strategy: direct final-name write only for plain small files, otherwise
/// temp+rename.
pub fn copy_file(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    mut journal: Option<&mut Journal>,
) -> Result<EngineResult, OperationError> {
    let OpenedFile {
        mut source,
        streams,
        extended_attributes,
        routing,
        eas_dropped,
        should_hash,
        has_representable_auxiliary_data,
    } = open_file(request, journal.is_some())?;
    if request.preserve_sparse && is_sparse(request.source_snapshot.metadata.basic.attributes) {
        copy_sparse(
            request,
            counters,
            &mut source,
            streams.as_ref(),
            extended_attributes.as_ref(),
            eas_dropped,
            should_hash,
            journal.as_deref_mut(),
        )
    } else if routing.largest_representable < request.large_threshold()
        && !routing.checkpoint_eligible
        && !has_representable_auxiliary_data
    {
        copy_plain_small(
            request,
            counters,
            source,
            should_hash,
            routing.named_streams_dropped,
            eas_dropped,
        )
    } else {
        // A live resume checkpoint keeps the ordered path: resume replays a
        // verified prefix and continues sequentially, which is incompatible
        // with out-of-order segment writes in v1.
        let resume_candidate = journal.as_deref().is_some_and(|journal| {
            journal
                .checkpoint_owned(&crate::journal::path_key(request.relative_path), "")
                .is_some()
        });
        let plan = segment_plan(
            request.transport(),
            request.source_is_wsl(),
            request.destination_is_wsl(),
            // Per-file effective sparse preservation: a file that would take
            // the sparse strategy never reaches this branch, so this is the
            // same `preserve_sparse && is_sparse` predicate the first branch
            // routes on (the run-level `request.preserve_sparse` alone would
            // wrongly veto every file on a sparse-capable destination).
            request.preserve_sparse && is_sparse(request.source_snapshot.metadata.basic.attributes),
            streams.as_ref(),
            request.source_snapshot.metadata.size,
            SEGMENT_THRESHOLD_BYTES,
            request.chunk_bytes(),
            routing.checkpoint_eligible,
            resume_candidate,
        );
        if let Some(plan) = plan {
            copy_streamed_segmented(
                request,
                counters,
                &mut source,
                streams.as_ref(),
                extended_attributes.as_ref(),
                eas_dropped,
                should_hash,
                journal.as_deref_mut(),
                &plan,
            )
        } else {
            copy_streamed(
                request,
                counters,
                &mut source,
                streams.as_ref(),
                extended_attributes.as_ref(),
                eas_dropped,
                should_hash,
                journal.as_deref_mut(),
            )
        }
    }
}

struct OpenedFile<'a> {
    source: SourceFile,
    streams: Cow<'a, [StreamInfo]>,
    extended_attributes: Option<bigcp_win::ExtendedAttributes>,
    routing: StreamRouting,
    eas_dropped: bool,
    should_hash: bool,
    has_representable_auxiliary_data: bool,
}

fn open_file<'a>(
    request: &EngineRequest<'a>,
    journal_available: bool,
) -> Result<OpenedFile<'a>, OperationError> {
    let timer = std::time::Instant::now();
    let source = SourceFile::open(request.source_path)
        .map_err(|error| source_open_error("open_src", request.relative_path, &error))?;
    ensure_source_unchanged(
        request.source_snapshot,
        source.opened_metadata(),
        request.relative_path,
        "open_src",
    )?;
    request
        .phases
        .record(crate::phase::PHASE_OPEN_SRC, timer.elapsed());

    let streams = if let Some(streams) = request.known_streams {
        Cow::Borrowed(streams)
    } else if !request.source_supports_streams() {
        Cow::Owned(vec![StreamInfo::unnamed(
            request.source_snapshot.metadata.size,
        )])
    } else {
        let timer = std::time::Instant::now();
        let streams = list_streams(request.source_path)
            .map_err(|error| source_open_error("list_streams", request.relative_path, &error))?;
        request
            .phases
            .record(crate::phase::PHASE_LIST_STREAMS, timer.elapsed());
        Cow::Owned(streams)
    };
    let routing = route_streams(
        streams.as_ref(),
        request.source_snapshot.metadata.size,
        request.destination_supports_streams(),
        journal_available,
        request.checkpoint_threshold(),
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
        request.verify() || largest_stream >= request.large_threshold() || checkpoint_eligible;
    let source_has_eas =
        request.source_supports_eas() && request.source_snapshot.metadata.ea_size > 0;
    let eas_dropped = source_has_eas && !request.destination_supports_eas();
    let extended_attributes = (source_has_eas && request.destination_supports_eas())
        .then(|| {
            read_extended_attributes_checked(
                request.source_path,
                request.source_snapshot.metadata.identity,
            )
        })
        .transpose()
        .map_err(|error| source_open_error("read_ea", request.relative_path, &error))?;
    let named_streams = routing.named_streams;
    let has_representable_auxiliary_data = extended_attributes.is_some()
        || (request.destination_supports_streams() && named_streams > 0);
    Ok(OpenedFile {
        source,
        streams,
        extended_attributes,
        routing,
        eas_dropped,
        should_hash,
        has_representable_auxiliary_data,
    })
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
        let temp = DestinationTemp::create(
            parent,
            request.run_id(),
            false,
            request.destination_is_wsl(),
        )
        .map_err(|error| operation_error("create_dst_temp", request.relative_path, &error))?;
        temp.mark_sparse()
            .map_err(|error| operation_error("set_sparse", request.relative_path, &error))?;
        (temp, 0, should_hash.then(Xxh3::new))
    };
    let efs_downgraded = efs_downgraded_for(request, &temp);
    temp.set_len(logical_size)
        .map_err(|error| operation_error("set_eof", request.relative_path, &error))?;

    // Historically this path rejected a zero-byte chunk profile for every
    // transport (the streamed path rejects it only for the standard one), so
    // the check stays unconditional here.
    if request.chunk_bytes() == 0 {
        return Err(zero_chunk_error(request));
    }
    let largest_range = ranges.iter().map(|range| range.length).max().unwrap_or(0);
    let mut buffers = StreamBuffers::new(request, request.chunk_bytes(), largest_range)?;
    let checkpoint_eligible = logical_size >= request.checkpoint_threshold() && journal.is_some();
    let mut cursor = CheckpointCursor::new(
        checkpoint_eligible,
        hash_offset,
        logical_size,
        checkpoint_identity(&temp, checkpoint_eligible),
    );
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
            let count = cursor
                .bound(hash_offset, range_start - hash_offset)
                .min(request.chunk_bytes() as u64);
            hash_zero_count(&mut hasher, count);
            hash_offset += count;
            cursor.advance(
                request,
                &mut temp,
                &mut journal,
                &mut journal_degraded,
                "",
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
            let count = transfer_segment(
                request,
                counters,
                source,
                &mut temp,
                &mut buffers,
                cursor.bound(hash_offset, remaining),
                SPARSE_SEGMENT_OPS,
                &mut hasher,
            )?;
            remaining -= count;
            hash_offset += count;
            cursor.advance(
                request,
                &mut temp,
                &mut journal,
                &mut journal_degraded,
                "",
                hash_offset,
                hasher.as_ref(),
            )?;
        }
    }
    while hash_offset < logical_size {
        let count = cursor
            .bound(hash_offset, logical_size - hash_offset)
            .min(request.chunk_bytes() as u64);
        hash_zero_count(&mut hasher, count);
        hash_offset += count;
        cursor.advance(
            request,
            &mut temp,
            &mut journal,
            &mut journal_degraded,
            "",
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
        request.flush(),
        request.destination_supports_posix_unlink_rename(),
    )
    .map_err(|error| operation_error("commit", request.relative_path, &error))?;
    Ok(EngineResult {
        bytes: logical_size.saturating_add(named.bytes),
        digest: hasher.map(|value| format!("xxh3:{:032x}", value.digest128())),
        stream_digests: named.digests,
        ea_digest: (request.verify() && !source_eas_dropped)
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
    source: SourceFile,
    should_hash: bool,
    streams_dropped: u32,
    eas_dropped: bool,
) -> Result<EngineResult, OperationError> {
    let prepared = read_plain_small(
        request,
        counters,
        source,
        should_hash,
        streams_dropped,
        eas_dropped,
    )?;
    let written = write_plain_small(request, counters, prepared, false)?;
    finish_plain_small(request, written)
}

/// Result of the read-only phase used by the same-spindle small-file batcher.
pub(crate) enum SmallPreparation {
    /// A plain unnamed stream is fully buffered and source-validated.
    Ready(PreparedPlainSmall),
    /// The file needs the regular transactional or sparse engine.
    RequiresRegular,
}

/// Performs only source-side work for a worker-routable small file.
pub(crate) fn prepare_plain_small(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
) -> Result<SmallPreparation, OperationError> {
    let OpenedFile {
        source,
        routing,
        eas_dropped,
        should_hash,
        has_representable_auxiliary_data,
        ..
    } = open_file(request, false)?;
    if (request.preserve_sparse && is_sparse(request.source_snapshot.metadata.basic.attributes))
        || routing.largest_representable >= request.large_threshold()
        || routing.checkpoint_eligible
        || has_representable_auxiliary_data
    {
        return Ok(SmallPreparation::RequiresRegular);
    }
    read_plain_small(
        request,
        counters,
        source,
        should_hash,
        routing.named_streams_dropped,
        eas_dropped,
    )
    .map(SmallPreparation::Ready)
}

pub(crate) struct PreparedPlainSmall {
    source: SourceFile,
    bytes: Vec<u8>,
    digest: Option<String>,
    streams_dropped: u32,
    eas_dropped: bool,
    ea_digest: Option<String>,
}

pub(crate) struct WrittenPlainSmall {
    source_to_revalidate: Option<SourceFile>,
    result: EngineResult,
}

fn read_plain_small(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    mut source: SourceFile,
    should_hash: bool,
    streams_dropped: u32,
    eas_dropped: bool,
) -> Result<PreparedPlainSmall, OperationError> {
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
    request
        .phases
        .record(crate::phase::PHASE_READ, timer.elapsed());
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
    post_read_validate(request, &source)?;

    let digest = should_hash.then(|| digest_bytes(&bytes));
    let ea_digest = (request.verify() && !eas_dropped).then(|| digest_bytes(&[]));
    Ok(PreparedPlainSmall {
        source,
        bytes,
        digest,
        streams_dropped,
        eas_dropped,
        ea_digest,
    })
}

pub(crate) fn write_plain_small(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    prepared: PreparedPlainSmall,
    defer_source_revalidation: bool,
) -> Result<WrittenPlainSmall, OperationError> {
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
    request
        .phases
        .record(crate::phase::PHASE_CREATE_DST, timer.elapsed());
    let efs_downgraded =
        wants_encryption(request) && !destination.basic_attributes().is_ok_and(is_encrypted);
    let mut destination = destination;
    let timer = std::time::Instant::now();
    destination
        .write_all(&prepared.bytes)
        .map_err(|error| operation_error("write", request.relative_path, &error))?;
    request
        .phases
        .record(crate::phase::PHASE_WRITE, timer.elapsed());
    counters.bytes_written_destination = counters
        .bytes_written_destination
        .saturating_add(prepared.bytes.len() as u64);
    // Standard workers preserve the original write→source-check→finish order.
    // The same-spindle batch defers only this metadata check until every
    // destination in the batch is finished, avoiding one mechanical return to
    // the source region per file. No success is returned before that check.
    let source_to_revalidate = if defer_source_revalidation {
        Some(prepared.source)
    } else {
        post_read_validate(request, &prepared.source)?;
        None
    };
    let timer = std::time::Instant::now();
    destination
        .finish(
            request.flush(),
            request
                .destination_requires_post_write_stamp()
                .then_some(request.destination_metadata),
        )
        .map_err(|error| operation_error("flush", request.relative_path, &error))?;
    request
        .phases
        .record(crate::phase::PHASE_SET_META, timer.elapsed());
    Ok(WrittenPlainSmall {
        source_to_revalidate,
        result: EngineResult {
            bytes: prepared.bytes.len() as u64,
            digest: prepared.digest,
            stream_digests: Vec::new(),
            ea_digest: prepared.ea_digest,
            streams_dropped: prepared.streams_dropped,
            eas_dropped: prepared.eas_dropped,
            journal_degraded: false,
            efs_downgraded,
            checkpoint_used: false,
        },
    })
}

/// Completes the source-stability check after a phased destination batch.
pub(crate) fn finish_plain_small(
    request: &EngineRequest<'_>,
    written: WrittenPlainSmall,
) -> Result<EngineResult, OperationError> {
    if let Some(source) = &written.source_to_revalidate {
        post_read_validate(request, source)?;
    }
    Ok(written.result)
}

/// Opens the direct final-name writer, clearing a read-only attribute first
/// when the classification already saw it on the file being replaced.
fn create_final(request: &EngineRequest<'_>) -> Result<DestinationFinal, OperationError> {
    let encrypted = wants_encryption(request);
    let expected = request
        .replacement_snapshot
        .map(|snapshot| &snapshot.metadata);
    let initial_stamp = (!request.destination_is_wsl()).then_some(request.destination_metadata);
    match open_final(request, expected, encrypted, initial_stamp) {
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
            match open_final(request, Some(&cleared_snapshot), encrypted, initial_stamp) {
                Ok(value) => Ok(value),
                Err(create_error) => {
                    // The retry did not take ownership of the file. Restore
                    // the exact object we cleared so a failed copy does not
                    // silently weaken an existing destination attribute.
                    if let Err(restore_error) = set_basic_at_checked(
                        request.destination_path,
                        snapshot.metadata.identity,
                        snapshot.metadata.basic,
                    ) {
                        return Err(readonly_restore_error(
                            request.relative_path,
                            &create_error,
                            &restore_error,
                        ));
                    }
                    Err(operation_error(
                        "create_dst",
                        request.relative_path,
                        &create_error,
                    ))
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

fn open_final(
    request: &EngineRequest<'_>,
    expected: Option<&ObjectMetadata>,
    encrypted: bool,
    initial_stamp: Option<bigcp_win::BasicMetadata>,
) -> std::io::Result<DestinationFinal> {
    if let Some(parent) = request.relative_destination_parent {
        let name = request.destination_path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination file path has no final component",
            )
        })?;
        DestinationFinal::create_relative(
            parent,
            name,
            expected,
            encrypted,
            initial_stamp,
            request.destination_is_wsl(),
        )
    } else {
        DestinationFinal::create(
            request.destination_path,
            expected,
            encrypted,
            initial_stamp,
            request.destination_is_wsl(),
        )
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
        let temp = DestinationTemp::create(
            parent,
            request.run_id(),
            wants_encryption(request),
            request.destination_is_wsl(),
        )
        .map_err(|error| operation_error("create_dst_temp", request.relative_path, &error))?;
        if request.destination_supports_preallocation() {
            temp.preallocate(request.source_snapshot.metadata.size)
                .map_err(|error| operation_error("preallocate", request.relative_path, &error))?;
        }
        (temp, 0, should_hash.then(Xxh3::new))
    };
    let efs_downgraded = efs_downgraded_for(request, &temp);
    let logical_size = request.source_snapshot.metadata.size;
    let checkpoint_eligible = logical_size >= request.checkpoint_threshold() && journal.is_some();
    let mut cursor = CheckpointCursor::new(
        checkpoint_eligible,
        total,
        logical_size,
        checkpoint_identity(&temp, checkpoint_eligible),
    );
    let mut journal_degraded = false;
    let mut buffers = StreamBuffers::new(
        request,
        request.chunk_bytes(),
        logical_size.saturating_sub(total),
    )?;
    while total < logical_size {
        let count = transfer_segment(
            request,
            counters,
            source,
            &mut temp,
            &mut buffers,
            cursor.bound(total, logical_size - total),
            STREAM_SEGMENT_OPS,
            &mut hasher,
        )?;
        total = total.saturating_add(count);
        cursor.advance(
            request,
            &mut temp,
            &mut journal,
            &mut journal_degraded,
            "",
            total,
            hasher.as_ref(),
        )?;
    }
    finish_streamed(
        request,
        counters,
        source,
        streams,
        extended_attributes,
        source_eas_dropped,
        temp,
        total,
        hasher,
        journal,
        journal_degraded,
        efs_downgraded,
    )
}

/// Shared tail of the ordered and segmented streamed strategies: post-transfer
/// source revalidation, named-stream copy, EA copy, pre-commit destination
/// validation, protected-DACL preservation, and atomic publication.
///
/// Extracted mechanically from `copy_streamed` so [`copy_streamed_segmented`]
/// publishes through exactly the same code as the ordered path.
#[allow(clippy::too_many_arguments)]
fn finish_streamed(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &SourceFile,
    streams: &[StreamInfo],
    extended_attributes: Option<&bigcp_win::ExtendedAttributes>,
    source_eas_dropped: bool,
    mut temp: DestinationTemp,
    total: u64,
    hasher: Option<Xxh3>,
    mut journal: Option<&mut Journal>,
    mut journal_degraded: bool,
    efs_downgraded: bool,
) -> Result<EngineResult, OperationError> {
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
        request.flush(),
        request.destination_supports_posix_unlink_rename(),
    )
    .map_err(|error| operation_error("commit", request.relative_path, &error))?;
    Ok(EngineResult {
        bytes: total.saturating_add(named.bytes),
        digest: hasher.map(|value| format!("xxh3:{:032x}", value.digest128())),
        stream_digests: named.digests,
        ea_digest: (request.verify() && !source_eas_dropped)
            .then(|| digest_bytes(extended_attributes.map_or(&[], |value| value.as_bytes()))),
        streams_dropped: named.dropped,
        eas_dropped: source_eas_dropped,
        journal_degraded,
        efs_downgraded,
        checkpoint_used,
    })
}

/// Minimum unnamed-stream size for the segmented parallel WSL strategy.
///
/// Measured 2026-08-02 over `\\wsl.localhost` (BENCHMARKS.md): one 9P handle
/// caps at ~230–290 MB/s while two concurrent handles reach 408 MB/s
/// aggregate, and robocopy reaches 560 MB/s on a single file via parallel
/// I/O — the per-handle ceiling, not the medium, is the bottleneck. Below
/// 64 MiB the extra identity-checked opens and thread startup outweigh that
/// ceiling, so smaller files keep the ordered pipeline.
const SEGMENT_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// Segment-count bounds: `K = clamp(size / SEGMENT_THRESHOLD_BYTES, 2, 8)`.
///
/// Two handles already recover most of the measured aggregate win (408 MB/s
/// vs ~230–290 MB/s on one), and each extra handle adds diminishing returns
/// against the single 9P server while multiplying open/identity round trips
/// and per-thread chunk buffers, so the count is bounded at eight.
const SEGMENT_MIN: u64 = 2;
/// Upper bound of the segment-count clamp (see [`SEGMENT_MIN`]).
const SEGMENT_MAX: u64 = 8;

/// Parallel-segment layout for one eligible large one-sided WSL file.
#[derive(Debug, Eq, PartialEq)]
struct SegmentPlan {
    /// Contiguous `(offset, length)` ranges covering exactly `[0, size)`.
    ///
    /// Every range starts chunk-aligned and is non-empty; the last range
    /// absorbs the unaligned remainder.
    ranges: Vec<(u64, u64)>,
}

/// Decides segmented-parallel eligibility and produces the range layout.
///
/// This is a pure function of preflight facts so the contract is testable in
/// isolation. Every condition is load-bearing:
/// - `transport` must be the WSL Plan 9 transport — the strategy exists only
///   to overlap that provider's per-handle ceiling (see
///   [`SEGMENT_THRESHOLD_BYTES`]).
/// - exactly one endpoint may be WSL (`source_is_wsl` XOR
///   `destination_is_wsl`): the local side hosts the coordinator's whole-file
///   digest pass, and a WSL↔WSL copy has no local side.
/// - `preserve_sparse` files keep the allocated-range strategy.
/// - the discovered stream set must be unnamed-only (the WSL side claims no
///   named streams; requiring it keeps a surprise ADS on the ordered path).
/// - `size` below `threshold` is not worth the extra opens and threads.
/// - `checkpoint_eligible` files keep the ordered path: checkpoints attest a
///   contiguous verified prefix, which out-of-order segment writes violate.
/// - `resume_candidate` files keep the ordered path: resume + segmentation
///   do not mix in v1.
///
/// `threshold` is a parameter (the product call site passes
/// [`SEGMENT_THRESHOLD_BYTES`]) so tests can exercise the planner and the
/// copy mechanics with tiny sandbox files; the copy path itself never
/// depends on the 64 MiB constant.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn segment_plan(
    transport: TransportProfile,
    source_is_wsl: bool,
    destination_is_wsl: bool,
    preserve_sparse: bool,
    streams: &[StreamInfo],
    size: u64,
    threshold: u64,
    chunk_bytes: usize,
    checkpoint_eligible: bool,
    resume_candidate: bool,
) -> Option<SegmentPlan> {
    if !transport.is_wsl()
        || source_is_wsl == destination_is_wsl
        || preserve_sparse
        || streams.iter().any(|stream| !stream.is_unnamed())
        || size < threshold
        || checkpoint_eligible
        || resume_candidate
        || chunk_bytes == 0
    {
        return None;
    }
    let segments = size
        .checked_div(threshold)
        .unwrap_or(0)
        .clamp(SEGMENT_MIN, SEGMENT_MAX);
    let chunk = chunk_bytes as u64;
    // Chunk-aligned base length; the division order keeps every non-final
    // segment a whole number of chunks so no read straddles two writers.
    let base = (size / segments / chunk) * chunk;
    if base == 0 {
        // The file cannot be split into chunk-aligned non-empty segments
        // (only reachable with tiny test thresholds); use the ordered path.
        return None;
    }
    let mut ranges = Vec::with_capacity(usize::try_from(segments).unwrap_or_default());
    for index in 0..segments {
        let offset = index * base;
        let length = if index == segments - 1 {
            size - offset
        } else {
            base
        };
        ranges.push((offset, length));
    }
    Some(SegmentPlan { ranges })
}

/// Segmented parallel transfer for one large file crossing WSL's Plan 9
/// provider exactly once (eligibility and layout in [`segment_plan`]).
///
/// K scoped threads each move one planned range through their own
/// identity-checked source open and their own identity-proven
/// [`SegmentWriter`] over the shared temporary, while the coordinator thread
/// computes the whole-file digest from the local side. Failure semantics are
/// identical to `copy_streamed`: on any error or cancellation the temp's
/// delete-on-close disposition removes it, and success publishes through
/// [`finish_streamed`] — exactly the ordered path's tail. `copy_streamed`
/// itself remains byte-for-byte untouched as the fallback for every other
/// modality (UNC, non-WSL redirectors, checkpointed files, sparse, resume).
#[allow(clippy::too_many_arguments)]
fn copy_streamed_segmented(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut SourceFile,
    streams: &[StreamInfo],
    extended_attributes: Option<&bigcp_win::ExtendedAttributes>,
    source_eas_dropped: bool,
    should_hash: bool,
    journal: Option<&mut Journal>,
    plan: &SegmentPlan,
) -> Result<EngineResult, OperationError> {
    let parent = destination_parent(request)?;
    let mut temp = DestinationTemp::create(
        parent,
        request.run_id(),
        wants_encryption(request),
        request.destination_is_wsl(),
    )
    .map_err(|error| operation_error("create_dst_temp", request.relative_path, &error))?;
    let logical_size = request.source_snapshot.metadata.size;
    if request.destination_supports_preallocation() {
        // Local temp (WSL→Win): the same dense-allocation hint the ordered
        // path issues.
        temp.preallocate(logical_size)
            .map_err(|error| operation_error("preallocate", request.relative_path, &error))?;
    } else {
        // WSL temp (Win→WSL): pin logical EOF before any parallel writer
        // runs, so out-of-order segment writes are in-place writes instead
        // of racing EOF extensions through the Plan 9 provider.
        temp.set_len(logical_size)
            .map_err(|error| operation_error("set_eof", request.relative_path, &error))?;
    }
    let efs_downgraded = efs_downgraded_for(request, &temp);
    if request.chunk_bytes() == 0 {
        return Err(zero_chunk_error(request));
    }
    // Captured once from the live handle: a path reopen without identity
    // proof must never be written through, so every segment writer is
    // validated against this exact identity inside `open_segment_writer`.
    let temp_identity = temp
        .identity()
        .map_err(|error| operation_error("identify_dst_temp", request.relative_path, &error))?;
    // All segment writers are opened by the coordinator before any thread
    // starts: each open clears and re-arms the shared handle's delete-on-close
    // disposition, which is only safe serialized on one thread.
    let mut writers = Vec::with_capacity(plan.ranges.len());
    for _ in &plan.ranges {
        writers.push(
            temp.open_segment_writer(temp_identity).map_err(|error| {
                operation_error("open_dst_segment", request.relative_path, &error)
            })?,
        );
    }
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<OperationError>> = Mutex::new(None);
    let mut hasher = should_hash.then(Xxh3::new);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(plan.ranges.len());
        for ((offset, length), writer) in plan.ranges.iter().copied().zip(writers) {
            let stop = &stop;
            let first_error = &first_error;
            handles.push(
                scope.spawn(move || {
                    copy_segment(request, writer, offset, length, stop, first_error)
                }),
            );
        }
        // Win→WSL: while the segments run, the coordinator hashes the whole
        // file through its own already-open local source handle (its file
        // position is independent of the segment threads' separate opens),
        // overlapping the nearly-free local reads with the 9P writes. These
        // digest reads are NOT copy reads — the segment threads' reads are
        // the copy's actual I/O — so they must not touch `counters`
        // (double-counting `bytes_read_source` would corrupt the report's
        // actual-I/O accounting).
        if request.destination_is_wsl()
            && let Some(hasher) = hasher.as_mut()
            && let Err(error) = hash_local_source(request, source, logical_size, &stop, hasher)
        {
            record_segment_error(&first_error, &stop, error);
        }
        for handle in handles {
            match handle.join() {
                Ok(local) => {
                    counters.bytes_read_source = counters
                        .bytes_read_source
                        .saturating_add(local.bytes_read_source);
                    counters.bytes_written_destination = counters
                        .bytes_written_destination
                        .saturating_add(local.bytes_written_destination);
                }
                Err(_) => record_segment_error(
                    &first_error,
                    &stop,
                    OperationError::semantic(
                        ErrorCategory::Internal,
                        "segment_panic",
                        request.relative_path.to_path_buf(),
                        "segment thread panicked; this is a bigcp bug — please file the log",
                    ),
                ),
            }
        }
    });
    if let Some(error) = first_error
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
    {
        // Identical to a copy_streamed failure: returning drops `temp`, whose
        // delete-on-close disposition removes the partial temporary.
        return Err(error);
    }
    if !request.destination_is_wsl()
        && let Some(hasher) = hasher.as_mut()
    {
        // WSL→Win: after every segment joined successfully, hash the
        // just-written local temp bytes (page-cache-hot). This digests
        // exactly the bytes that will be published — a strictly stronger
        // attestation than hashing the source stream in flight. Like the
        // Win→WSL pass above, this is neither copy I/O nor verification
        // I/O: no counter changes.
        hash_local_temp(request, &mut temp, logical_size, hasher)?;
    }
    finish_streamed(
        request,
        counters,
        source,
        streams,
        extended_attributes,
        source_eas_dropped,
        temp,
        logical_size,
        hasher,
        journal,
        false,
        efs_downgraded,
    )
}

/// Runs one planned segment on its own thread and returns its actual-I/O
/// counters — always, so partial reads and writes stay exactly accounted even
/// when the segment fails. Errors land in the shared first-error slot
/// (first error wins) and trip the stop flag for the other segments.
fn copy_segment(
    request: &EngineRequest<'_>,
    mut writer: SegmentWriter,
    offset: u64,
    length: u64,
    stop: &AtomicBool,
    first_error: &Mutex<Option<OperationError>>,
) -> Counters {
    let mut local = Counters::default();
    if let Err(error) = copy_segment_range(request, &mut writer, offset, length, stop, &mut local) {
        record_segment_error(first_error, stop, error);
    }
    local
}

/// Moves exactly `[offset, offset + length)` from a fresh identity-checked
/// source open into the identity-proven segment writer, chunk by chunk,
/// polling cancellation and the shared stop flag between chunks.
fn copy_segment_range(
    request: &EngineRequest<'_>,
    writer: &mut SegmentWriter,
    offset: u64,
    length: u64,
    stop: &AtomicBool,
    counters: &mut Counters,
) -> Result<(), OperationError> {
    // Each segment proves its own source object: the fresh open is validated
    // against the enumeration snapshot exactly like the ordered path's open
    // (identity, size, and last-write time), so no thread can ever read a
    // file swapped in at the source path.
    let mut source = SourceFile::open(request.source_path)
        .map_err(|error| source_open_error("open_src", request.relative_path, &error))?;
    ensure_source_unchanged(
        request.source_snapshot,
        source.opened_metadata(),
        request.relative_path,
        "open_src",
    )?;
    source
        .seek(std::io::SeekFrom::Start(offset))
        .map_err(|error| operation_error("seek_src", request.relative_path, &error))?;
    writer
        .seek(std::io::SeekFrom::Start(offset))
        .map_err(|error| operation_error("seek_dst", request.relative_path, &error))?;
    let mut buffer = vec![0_u8; bounded_buffer_len(request.chunk_bytes(), length)];
    let mut remaining = length;
    while remaining > 0 {
        if stop.load(Ordering::Acquire) {
            // Another segment already failed or canceled; its error is
            // authoritative and this segment just stops issuing I/O.
            return Ok(());
        }
        if request.cancel.is_canceled() {
            return Err(canceled_error(request));
        }
        let requested = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            address_space_error(
                request,
                STREAM_SEGMENT_OPS.read_op,
                STREAM_SEGMENT_OPS.subject,
            )
        })?;
        let count = source.read(&mut buffer[..requested]).map_err(|error| {
            operation_error(STREAM_SEGMENT_OPS.read_op, request.relative_path, &error)
        })?;
        if count == 0 {
            return Err(OperationError::semantic(
                ErrorCategory::SourceChanged,
                STREAM_SEGMENT_OPS.read_op,
                request.relative_path.to_path_buf(),
                STREAM_SEGMENT_OPS.truncated,
            ));
        }
        counters.bytes_read_source = counters.bytes_read_source.saturating_add(count as u64);
        writer.write_all(&buffer[..count]).map_err(|error| {
            operation_error(STREAM_SEGMENT_OPS.write_op, request.relative_path, &error)
        })?;
        counters.bytes_written_destination = counters
            .bytes_written_destination
            .saturating_add(count as u64);
        remaining -= count as u64;
    }
    writer.flush().map_err(|error| {
        operation_error(STREAM_SEGMENT_OPS.write_op, request.relative_path, &error)
    })
}

/// Records the first segment failure (later failures are dropped — the first
/// error wins) and trips the shared stop flag so sibling segments exit at
/// their next chunk boundary.
fn record_segment_error(
    first_error: &Mutex<Option<OperationError>>,
    stop: &AtomicBool,
    error: OperationError,
) {
    let mut slot = first_error.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(error);
    }
    drop(slot);
    stop.store(true, Ordering::Release);
}

/// Coordinator digest pass over the local source while segments write to a
/// WSL destination. Deliberately touches no counters (see the call site).
fn hash_local_source(
    request: &EngineRequest<'_>,
    source: &mut SourceFile,
    logical_size: u64,
    stop: &AtomicBool,
    hasher: &mut Xxh3,
) -> Result<(), OperationError> {
    source
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|error| operation_error("seek_src", request.relative_path, &error))?;
    let mut buffer = vec![0_u8; bounded_buffer_len(request.chunk_bytes(), logical_size)];
    let mut remaining = logical_size;
    while remaining > 0 {
        if stop.load(Ordering::Acquire) {
            // A segment already failed; its error is authoritative and this
            // partial digest is discarded with the run.
            return Ok(());
        }
        if request.cancel.is_canceled() {
            return Err(canceled_error(request));
        }
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| address_space_error(request, STREAM_SEGMENT_OPS.read_op, "digest"))?;
        let count = source.read(&mut buffer[..requested]).map_err(|error| {
            operation_error(STREAM_SEGMENT_OPS.read_op, request.relative_path, &error)
        })?;
        if count == 0 {
            return Err(OperationError::semantic(
                ErrorCategory::SourceChanged,
                STREAM_SEGMENT_OPS.read_op,
                request.relative_path.to_path_buf(),
                STREAM_SEGMENT_OPS.truncated,
            ));
        }
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    Ok(())
}

/// Coordinator digest pass over the just-written local temp after every
/// segment joined (WSL→Win). Deliberately touches no counters (see the call
/// site).
fn hash_local_temp(
    request: &EngineRequest<'_>,
    temp: &mut DestinationTemp,
    logical_size: u64,
    hasher: &mut Xxh3,
) -> Result<(), OperationError> {
    temp.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| operation_error("seek_dst_temp", request.relative_path, &error))?;
    let mut buffer = vec![0_u8; bounded_buffer_len(request.chunk_bytes(), logical_size)];
    let mut remaining = logical_size;
    while remaining > 0 {
        if request.cancel.is_canceled() {
            return Err(canceled_error(request));
        }
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| address_space_error(request, "read_dst_temp", "digest"))?;
        let count = temp
            .read(&mut buffer[..requested])
            .map_err(|error| operation_error("read_dst_temp", request.relative_path, &error))?;
        if count == 0 {
            // Every segment reported success and EOF was pinned up front, so
            // a short temp is an engine invariant breach, not a source event.
            return Err(OperationError::semantic(
                ErrorCategory::Internal,
                "read_dst_temp",
                request.relative_path.to_path_buf(),
                "segment-written temporary ended before its pinned size",
            ));
        }
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    Ok(())
}

/// Chunk-bounded transfer/digest buffer length, never zero and never larger
/// than the bytes remaining.
fn bounded_buffer_len(chunk_bytes: usize, remaining: u64) -> usize {
    let chunk = chunk_bytes.max(1);
    usize::try_from(remaining.min(chunk as u64).max(1)).unwrap_or(chunk)
}

fn allocate_burst_buffer(
    request: &EngineRequest<'_>,
    remaining: u64,
) -> Result<BurstBuffer, OperationError> {
    BurstBuffer::new(request.transport(), request.chunk_bytes(), remaining).map_err(|error| {
        OperationError::semantic(
            ErrorCategory::Internal,
            "allocate_burst_buffer",
            request.relative_path.to_path_buf(),
            format!(
                "could not reserve the bounded {}-byte same-spindle buffer: {error}",
                request.transport().burst_bytes
            ),
        )
    })
}

fn zero_chunk_error(request: &EngineRequest<'_>) -> OperationError {
    OperationError::semantic(
        ErrorCategory::Internal,
        "allocate_buffer",
        request.relative_path.to_path_buf(),
        "the composed copy profile selected a zero-byte chunk",
    )
}

fn address_space_error(
    request: &EngineRequest<'_>,
    operation: &str,
    subject: &str,
) -> OperationError {
    OperationError::semantic(
        ErrorCategory::Internal,
        operation,
        request.relative_path.to_path_buf(),
        format!("{subject} request does not fit address space"),
    )
}

/// Per-stream transport state allocated once before a stream's transfer loop.
enum StreamBuffers {
    /// Request-at-a-time chunk buffer for the standard transport.
    Standard(Vec<u8>),
    /// Bounded redirector pipeline; the two request buffers live inside
    /// [`transfer_pipelined`], so only the per-request size is retained.
    Redirector {
        /// Bytes per pipeline request.
        request_bytes: usize,
    },
    /// Phased staging buffer reused by every same-spindle burst of the stream.
    SameSpindle(BurstBuffer),
}

impl StreamBuffers {
    /// Builds the run transport's state for one stream with `remaining` bytes
    /// left to move. `request_bytes` sizes the standard chunk buffer (a
    /// zero-byte composed profile is rejected) and the redirector's requests;
    /// the same-spindle staging buffer keeps its own composed request size.
    fn new(
        request: &EngineRequest<'_>,
        request_bytes: usize,
        remaining: u64,
    ) -> Result<Self, OperationError> {
        if request.transport().is_same_spindle() {
            Ok(Self::SameSpindle(allocate_burst_buffer(
                request, remaining,
            )?))
        } else if request.transport().is_redirector() {
            Ok(Self::Redirector { request_bytes })
        } else if request_bytes == 0 {
            Err(zero_chunk_error(request))
        } else {
            Ok(Self::Standard(vec![0_u8; request_bytes]))
        }
    }
}

/// Static identity of one transfer site: the operation names and messages
/// its errors carry, plus site accounting preserved from the original loops.
#[derive(Clone, Copy)]
struct SegmentOps {
    /// Operation name attached to read-side failures.
    read_op: &'static str,
    /// Operation name attached to write-side failures.
    write_op: &'static str,
    /// `SourceChanged` message when the source ends before the segment bound.
    truncated: &'static str,
    /// Subject noun for address-space overflow errors.
    subject: &'static str,
    /// Historical sparse-range accounting: the sparse loop counted a chunk's
    /// read into `bytes_read_source` only after its write succeeded, while
    /// the streamed and named-stream loops count the read immediately.
    /// Preserved per-site so failure-path counters stay identical.
    count_read_after_write: bool,
}

/// Sparse allocated-range transfer identity.
const SPARSE_SEGMENT_OPS: SegmentOps = SegmentOps {
    read_op: "read_sparse",
    write_op: "write_sparse",
    truncated: "source ended inside an allocated range",
    subject: "sparse",
    count_read_after_write: true,
};

/// Unnamed-stream transfer identity.
const STREAM_SEGMENT_OPS: SegmentOps = SegmentOps {
    read_op: "read",
    write_op: "write",
    truncated: "source ended before its enumerated size",
    subject: "stream",
    count_read_after_write: false,
};

/// Named-stream transfer identity.
const NAMED_SEGMENT_OPS: SegmentOps = SegmentOps {
    read_op: "read_stream",
    write_op: "write_stream",
    truncated: "named stream ended before its enumerated size",
    subject: "named-stream",
    count_read_after_write: false,
};

/// Moves up to `bound` bytes through the run's transport — one staged burst
/// for same-spindle, one pipelined segment for the redirector, or
/// cancel-checked chunks up to the bound for the standard transport —
/// updating the actual-I/O counters and the optional in-flight hasher.
/// Returns the bytes moved: never 0, exactly `bound` for the redirector and
/// standard transports, and possibly less for a same-spindle burst capped by
/// its staging capacity, so callers must loop until their position reaches
/// the stream end and re-derive each bound from their checkpoint cursor.
#[allow(clippy::too_many_arguments)]
fn transfer_segment<R: Read + Send, W: Write>(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut R,
    destination: &mut W,
    buffers: &mut StreamBuffers,
    bound: u64,
    ops: SegmentOps,
    hasher: &mut Option<Xxh3>,
) -> Result<u64, OperationError> {
    match buffers {
        StreamBuffers::SameSpindle(staging) => {
            let requested = usize::try_from(bound.min(staging.capacity() as u64))
                .map_err(|_| address_space_error(request, ops.read_op, ops.subject))?;
            transfer_same_spindle_burst(
                request,
                counters,
                source,
                destination,
                staging,
                requested,
                ops.read_op,
                ops.write_op,
                ops.truncated,
                hasher,
            )
            .map(|count| count as u64)
        }
        StreamBuffers::Redirector { request_bytes } => transfer_redirector_segment(
            request,
            counters,
            source,
            destination,
            bound,
            *request_bytes,
            ops.read_op,
            ops.write_op,
            ops.truncated,
            hasher,
        ),
        StreamBuffers::Standard(buffer) => {
            let mut moved = 0_u64;
            while moved < bound {
                check_cancel(request)?;
                let requested = usize::try_from((bound - moved).min(buffer.len() as u64))
                    .map_err(|_| address_space_error(request, ops.read_op, ops.subject))?;
                let count = source
                    .read(&mut buffer[..requested])
                    .map_err(|error| operation_error(ops.read_op, request.relative_path, &error))?;
                if count == 0 {
                    return Err(OperationError::semantic(
                        ErrorCategory::SourceChanged,
                        ops.read_op,
                        request.relative_path.to_path_buf(),
                        ops.truncated,
                    ));
                }
                if !ops.count_read_after_write {
                    counters.bytes_read_source =
                        counters.bytes_read_source.saturating_add(count as u64);
                }
                if let Some(hasher) = hasher.as_mut() {
                    hasher.update(&buffer[..count]);
                }
                destination.write_all(&buffer[..count]).map_err(|error| {
                    operation_error(ops.write_op, request.relative_path, &error)
                })?;
                if ops.count_read_after_write {
                    counters.bytes_read_source =
                        counters.bytes_read_source.saturating_add(count as u64);
                }
                counters.bytes_written_destination = counters
                    .bytes_written_destination
                    .saturating_add(count as u64);
                moved = moved.saturating_add(count as u64);
            }
            Ok(moved)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn transfer_same_spindle_burst<R: Read, W: Write>(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut R,
    destination: &mut W,
    buffer: &mut BurstBuffer,
    requested: usize,
    read_operation: &str,
    write_operation: &str,
    unexpected_end: &str,
    hasher: &mut Option<Xxh3>,
) -> Result<usize, OperationError> {
    let count = match buffer.read_from(source, requested, request.cancel) {
        Ok(count) => count,
        Err(failure) => {
            counters.bytes_read_source = counters
                .bytes_read_source
                .saturating_add(failure.transferred as u64);
            return Err(match failure.kind {
                TransferFailureKind::Canceled => canceled_error(request),
                TransferFailureKind::Io(error) => {
                    operation_error(read_operation, request.relative_path, &error)
                }
            });
        }
    };
    counters.bytes_read_source = counters.bytes_read_source.saturating_add(count as u64);
    if count != requested {
        return Err(OperationError::semantic(
            ErrorCategory::SourceChanged,
            read_operation,
            request.relative_path.to_path_buf(),
            unexpected_end,
        ));
    }
    if let Some(hasher) = hasher {
        hasher.update(buffer.prefix(count));
    }
    match buffer.write_to(destination, count, request.cancel) {
        Ok(written) => {
            counters.bytes_written_destination = counters
                .bytes_written_destination
                .saturating_add(written as u64);
            Ok(written)
        }
        Err(failure) => {
            counters.bytes_written_destination = counters
                .bytes_written_destination
                .saturating_add(failure.transferred as u64);
            Err(match failure.kind {
                TransferFailureKind::Canceled => canceled_error(request),
                TransferFailureKind::Io(error) => {
                    operation_error(write_operation, request.relative_path, &error)
                }
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn transfer_redirector_segment<R: Read + Send, W: Write>(
    request: &EngineRequest<'_>,
    counters: &mut Counters,
    source: &mut R,
    destination: &mut W,
    requested: u64,
    request_bytes: usize,
    read_operation: &str,
    write_operation: &str,
    unexpected_end: &str,
    hasher: &mut Option<Xxh3>,
) -> Result<u64, OperationError> {
    let result = transfer_pipelined(
        source,
        destination,
        requested,
        request_bytes,
        request.cancel,
        |bytes| {
            if let Some(hasher) = hasher {
                hasher.update(bytes);
            }
        },
    );
    let transfer = match &result {
        Ok(transfer) => *transfer,
        Err(failure) => failure.transfer,
    };
    counters.bytes_read_source = counters
        .bytes_read_source
        .saturating_add(transfer.bytes_read);
    counters.bytes_written_destination = counters
        .bytes_written_destination
        .saturating_add(transfer.bytes_written);
    match result {
        Ok(transfer) => Ok(transfer.bytes_written),
        Err(failure) => Err(match failure.kind {
            PipelinedFailureKind::Allocate(error) => OperationError::semantic(
                ErrorCategory::Internal,
                "allocate_redirector_pipeline",
                request.relative_path.to_path_buf(),
                format!(
                    "could not reserve two bounded {request_bytes}-byte redirector buffers: {error}"
                ),
            ),
            PipelinedFailureKind::Canceled => canceled_error(request),
            PipelinedFailureKind::Read(error) => {
                operation_error(read_operation, request.relative_path, &error)
            }
            PipelinedFailureKind::Write(error) => {
                operation_error(write_operation, request.relative_path, &error)
            }
            PipelinedFailureKind::UnexpectedEof => OperationError::semantic(
                ErrorCategory::SourceChanged,
                read_operation,
                request.relative_path.to_path_buf(),
                unexpected_end,
            ),
            PipelinedFailureKind::ReaderPanicked => OperationError::semantic(
                ErrorCategory::Internal,
                "redirector_reader_panic",
                request.relative_path.to_path_buf(),
                "redirector reader thread panicked; this is a bigcp bug",
            ),
            PipelinedFailureKind::Internal(message) => OperationError::semantic(
                ErrorCategory::Internal,
                "redirector_pipeline",
                request.relative_path.to_path_buf(),
                message,
            ),
        }),
    }
}

/// Per-stream checkpoint-boundary bookkeeping shared by the sparse, streamed,
/// and named-stream loops.
///
/// [`CheckpointCursor::advance`] no-ops unless the position sits exactly on
/// the pending boundary, so callers invoke it unconditionally after every
/// position advance — transfers and sparse zero-gap hashing alike (sparse
/// boundaries can therefore land inside holes, where positions advance with
/// no transfer).
struct CheckpointCursor {
    /// Next boundary at which a checkpoint should be appended, when eligible.
    next: Option<u64>,
    /// Distance between checkpoint boundaries for this stream.
    interval: u64,
    /// Logical size of the stream being checkpointed.
    total_size: u64,
    /// Temp identity captured once at cursor construction and reused by
    /// every append: a filesystem identity is invariant for the lifetime of
    /// an open handle, so re-querying it per checkpoint paid one metadata
    /// round trip per append without proving anything new. `None` while
    /// eligible means the single capture failed, which disables
    /// checkpointing exactly as a per-append query failure used to.
    temp_identity: Option<FileIdentity>,
}

impl CheckpointCursor {
    /// Schedules boundaries for one `total_size`-byte stream whose copy
    /// starts at `position` (non-zero after a verified resume).
    fn new(
        eligible: bool,
        position: u64,
        total_size: u64,
        temp_identity: Option<FileIdentity>,
    ) -> Self {
        let interval = checkpoint_interval(total_size);
        Self {
            next: eligible.then(|| next_checkpoint_after(position, interval, total_size)),
            interval,
            total_size,
            temp_identity,
        }
    }

    /// Bytes the caller may move from `position` before the pending boundary,
    /// capped at `remaining`.
    fn bound(&self, position: u64, remaining: u64) -> u64 {
        remaining.min(self.next.map_or(remaining, |boundary| boundary - position))
    }

    /// Appends a checkpoint attesting the stream prefix up to `position` when
    /// it sits exactly on the pending boundary. A written checkpoint
    /// schedules the next boundary; a disabled journal (append or temp
    /// persist failed) stops checkpointing for the rest of this file and
    /// records the degradation; an unconfigured journal just stops scheduling
    /// boundaries.
    ///
    /// Invariant (see [`transfer_pipelined`]): the in-flight digest may run
    /// ahead of the durable destination prefix mid-segment, so this must only
    /// be called after the transfer that reached `position` returned `Ok` —
    /// never from inside a segment.
    #[allow(clippy::too_many_arguments)]
    fn advance(
        &mut self,
        request: &EngineRequest<'_>,
        temp: &mut DestinationTemp,
        journal: &mut Option<&mut Journal>,
        journal_degraded: &mut bool,
        stream: &str,
        position: u64,
        hasher: Option<&Xxh3>,
    ) -> Result<(), OperationError> {
        if self.next != Some(position) {
            return Ok(());
        }
        match append_checkpoint(
            journal.as_deref_mut(),
            temp,
            request,
            stream,
            self.total_size,
            position,
            hasher,
            self.temp_identity,
        )? {
            CheckpointStatus::Written => {
                self.next = (position < self.total_size)
                    .then(|| next_checkpoint_after(position, self.interval, self.total_size));
            }
            CheckpointStatus::Disabled => {
                *journal = None;
                self.next = None;
                *journal_degraded = true;
            }
            CheckpointStatus::NotConfigured => self.next = None,
        }
        Ok(())
    }
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
    let Ok(mut temp) =
        DestinationTemp::resume(parent, &checkpoint.temp_name, request.destination_is_wsl())
    else {
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
    let mut buffer = vec![0_u8; request.chunk_bytes().max(64 * 1024)];
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

fn ensure_base_checkpoint_for_named(
    request: &EngineRequest<'_>,
    temp: &mut DestinationTemp,
    streams: &[StreamInfo],
    base_size: u64,
    base_hasher: Option<&Xxh3>,
    journal: &mut Option<&mut Journal>,
) -> Result<bool, OperationError> {
    if !request.destination_supports_streams() {
        return Ok(false);
    }
    let named_is_resumable = streams
        .iter()
        .any(|stream| !stream.is_unnamed() && stream.size >= request.checkpoint_threshold());
    if !named_is_resumable || journal.is_none() || temp.is_persistent() {
        return Ok(false);
    }
    // This runs at most once per file, so capturing the identity here keeps
    // the invariant that appends never re-query an already-proven handle.
    let temp_identity = temp.identity().ok();
    match append_checkpoint(
        journal.as_deref_mut(),
        temp,
        request,
        "",
        request.source_snapshot.metadata.size,
        base_size,
        base_hasher,
        temp_identity,
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

#[allow(clippy::too_many_arguments)]
fn append_checkpoint(
    journal: Option<&mut Journal>,
    temp: &mut DestinationTemp,
    request: &EngineRequest<'_>,
    stream: &str,
    source_size: u64,
    watermark: u64,
    hasher: Option<&Xxh3>,
    temp_identity: Option<FileIdentity>,
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
    // The identity was captured once when the temporary's cursor (or the
    // named-stream base checkpoint) was set up: it is invariant for an open
    // handle, so the former per-append `temp.identity()` query was one
    // metadata round trip per checkpoint for no new proof. A failed capture
    // arrives here as `None` and disables checkpointing exactly as the
    // per-append query failure used to.
    let Some(temp_identity) = temp_identity else {
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
        && request.destination_supports_encryption()
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
    if !request.destination_supports_streams() {
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
        let checkpoint_eligible =
            stream.size >= request.checkpoint_threshold() && journal.is_some();
        let (mut destination, mut copied, mut hasher) = if let Some(state) = resumed {
            state
        } else {
            (
                temp.create_stream(stream).map_err(|error| {
                    operation_error("create_dst_stream", request.relative_path, &error)
                })?,
                0,
                // Invariant: every stream that can reach append_checkpoint
                // must carry a live hasher — append_checkpoint hard-errors
                // without one. checkpoint_eligible must therefore be part of
                // this condition: `--tune checkpoint-threshold` below
                // `large-threshold` is legal, so size >= large_threshold
                // alone does not imply it.
                (request.verify()
                    || stream.size >= request.large_threshold()
                    || checkpoint_eligible)
                    .then(Xxh3::new),
            )
        };
        let request_bytes = request.chunk_bytes().clamp(64 * 1024, 8 * 1024 * 1024);
        let mut buffers =
            StreamBuffers::new(request, request_bytes, stream.size.saturating_sub(copied))?;
        let mut cursor = CheckpointCursor::new(
            checkpoint_eligible,
            copied,
            stream.size,
            checkpoint_identity(temp, checkpoint_eligible),
        );
        while copied < stream.size {
            check_cancel(request)?;
            let count = transfer_segment(
                request,
                counters,
                &mut source,
                &mut destination,
                &mut buffers,
                cursor.bound(copied, stream.size - copied),
                NAMED_SEGMENT_OPS,
                &mut hasher,
            )?;
            copied = copied.saturating_add(count);
            cursor.advance(
                request,
                temp,
                &mut journal,
                &mut journal_degraded,
                &key,
                copied,
                hasher.as_ref(),
            )?;
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
    let mut buffer = vec![0_u8; request.chunk_bytes().clamp(64 * 1024, 8 * 1024 * 1024)];
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
    if !request.destination_supports_persistent_acls() {
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
        // New files skip the pre-commit path probe: without a replacement
        // snapshot, publication uses a non-replacing rename (`commit` with
        // `replace = false`), which atomically fails on a concurrently
        // appeared name — pinned by bigcp-win's
        // `nonreplacing_commit_preserves_a_dangling_link_collision` test —
        // so a separate `metadata_at` here cost one destination round trip
        // per streamed new file while proving strictly less than the rename
        // itself (the probe-then-rename window stayed open regardless).
        // Replacements below keep the probe: their replacing rename cannot
        // distinguish "expected object" from "mutated object" on its own.
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

/// Captures a stream cursor's temp identity exactly once, and only when
/// checkpointing is possible for the stream — files that will never append a
/// checkpoint must not pay the metadata query. The identity is invariant for
/// the lifetime of the open temp handle (see [`CheckpointCursor`]).
fn checkpoint_identity(temp: &DestinationTemp, eligible: bool) -> Option<FileIdentity> {
    if eligible { temp.identity().ok() } else { None }
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

/// Reports the rollback failure as the primary error because it may have left
/// an existing destination with READONLY cleared. The failed create remains in
/// the message so the operator can diagnose both halves of the operation.
fn readonly_restore_error(
    relative_path: &Path,
    create_error: &std::io::Error,
    restore_error: &std::io::Error,
) -> OperationError {
    let mut error = if restore_error.kind() == std::io::ErrorKind::InvalidData {
        OperationError::from_io_as(
            ErrorCategory::DestinationChanged,
            "restore_dst_metadata",
            relative_path.to_path_buf(),
            restore_error,
        )
    } else {
        OperationError::from_io(
            "restore_dst_metadata",
            relative_path.to_path_buf(),
            restore_error,
        )
    };
    error.message = format!(
        "destination create retry failed after READONLY was cleared ({create_error}); restoring the original metadata also failed: {restore_error}"
    );
    error
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
    if request.cancel.is_canceled() {
        Err(canceled_error(request))
    } else {
        Ok(())
    }
}

fn canceled_error(request: &EngineRequest<'_>) -> OperationError {
    OperationError::semantic(
        ErrorCategory::Internal,
        CANCELED_MID_FILE,
        request.relative_path.to_path_buf(),
        "graceful cancellation stopped this copy between chunks",
    )
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

    use bigcp_win::{FileSystem, StreamInfo, VolumeCapabilities};

    use super::{
        DestinationCaps, FilesystemPolicy, StreamRouting, readonly_restore_error, route_streams,
    };

    fn capabilities(value: bool) -> VolumeCapabilities {
        VolumeCapabilities {
            named_streams: value,
            extended_attributes: value,
            sparse_files: value,
            encryption: value,
            reparse_points: value,
            block_refcounting: value,
            persistent_acls: value,
            posix_unlink_rename: value,
        }
    }

    /// Pins `DestinationCaps::from_policy` field-by-field against the
    /// corresponding `FilesystemPolicy` projection so a constructor edit that
    /// transposes two capabilities fails loudly.
    #[test]
    fn destination_caps_mirror_the_filesystem_policy_projection() {
        // Strict local NTFS: every capability advertised, no restamp, no WSL.
        let ntfs = FilesystemPolicy::new(FileSystem::Ntfs, capabilities(true));
        let caps = DestinationCaps::from_policy(&ntfs);
        assert_eq!(caps.supports_streams, ntfs.supports_streams());
        assert_eq!(caps.supports_eas, ntfs.supports_eas());
        assert_eq!(caps.supports_encryption, ntfs.supports_encryption());
        assert_eq!(caps.supports_preallocation, ntfs.supports_preallocation());
        assert_eq!(
            caps.supports_persistent_acls,
            ntfs.supports_persistent_acls()
        );
        assert_eq!(
            caps.supports_posix_unlink_rename,
            ntfs.supports_posix_unlink_rename()
        );
        assert_eq!(
            caps.requires_post_write_stamp,
            ntfs.requires_post_write_stamp()
        );
        assert!(caps.supports_streams);
        assert!(caps.supports_eas);
        assert!(caps.supports_encryption);
        assert!(caps.supports_preallocation);
        assert!(caps.supports_persistent_acls);
        assert!(caps.supports_posix_unlink_rename);
        assert!(!caps.requires_post_write_stamp);
        assert!(!caps.is_wsl);

        // FAT: no capabilities, but local preallocation stays appropriate and
        // the direct write path needs the FAT-family post-write restamp.
        let fat = FilesystemPolicy::new(FileSystem::Fat, capabilities(false));
        let caps = DestinationCaps::from_policy(&fat);
        assert_eq!(caps.supports_streams, fat.supports_streams());
        assert_eq!(caps.supports_eas, fat.supports_eas());
        assert_eq!(caps.supports_encryption, fat.supports_encryption());
        assert_eq!(caps.supports_preallocation, fat.supports_preallocation());
        assert_eq!(
            caps.supports_persistent_acls,
            fat.supports_persistent_acls()
        );
        assert_eq!(
            caps.supports_posix_unlink_rename,
            fat.supports_posix_unlink_rename()
        );
        assert_eq!(
            caps.requires_post_write_stamp,
            fat.requires_post_write_stamp()
        );
        assert!(!caps.supports_streams);
        assert!(!caps.supports_eas);
        assert!(!caps.supports_encryption);
        assert!(caps.supports_preallocation);
        assert!(!caps.supports_persistent_acls);
        assert!(!caps.supports_posix_unlink_rename);
        assert!(caps.requires_post_write_stamp);
        assert!(!caps.is_wsl);

        // Alternating volume capabilities: any transposition of two adjacent
        // capability-derived fields flips at least one of these assertions.
        let alternating = FilesystemPolicy::new(
            FileSystem::Ntfs,
            VolumeCapabilities {
                named_streams: true,
                extended_attributes: false,
                sparse_files: false,
                encryption: true,
                reparse_points: false,
                block_refcounting: false,
                persistent_acls: false,
                posix_unlink_rename: true,
            },
        );
        let caps = DestinationCaps::from_policy(&alternating);
        assert!(caps.supports_streams);
        assert!(!caps.supports_eas);
        assert!(caps.supports_encryption);
        assert!(!caps.supports_persistent_acls);
        assert!(caps.supports_posix_unlink_rename);
    }

    #[test]
    fn readonly_rollback_failure_preserves_both_failure_contexts() {
        let create_error = std::io::Error::from_raw_os_error(5);
        let restore_error = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "object identity changed before metadata update",
        );
        let error =
            readonly_restore_error(Path::new("existing.txt"), &create_error, &restore_error);

        assert_eq!(
            error.category,
            crate::error::ErrorCategory::DestinationChanged
        );
        assert_eq!(error.operation, "restore_dst_metadata");
        assert!(error.message.contains("READONLY was cleared"));
        assert!(error.message.contains(&create_error.to_string()));
        assert!(error.message.contains("object identity changed"));
    }

    #[test]
    fn readonly_rollback_io_failure_is_the_primary_error() {
        let create_error = std::io::Error::from_raw_os_error(32);
        let restore_error = std::io::Error::from_raw_os_error(5);
        let error =
            readonly_restore_error(Path::new("existing.txt"), &create_error, &restore_error);

        assert_eq!(error.category, crate::error::ErrorCategory::Permissions);
        assert_eq!(error.code, Some(5));
        assert!(error.message.contains(&create_error.to_string()));
        assert!(error.message.contains(&restore_error.to_string()));
    }

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

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bigcp_win::{EndpointKind, SourceFile, VolumeInfo, metadata_at};

    use super::{
        CANCELED_MID_FILE, EngineRequest, EngineSettings, EngineTuning, SourceCaps,
        copy_streamed_segmented, segment_plan,
    };
    use crate::model::{Counters, EntrySnapshot};
    use crate::phase::PhaseTracker;
    use crate::transport::TransportProfile;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    /// Test planner threshold: `segment_plan` takes the threshold as a
    /// parameter precisely so the planner and copy mechanics can be exercised
    /// with tiny sandbox files instead of 64 MiB fixtures.
    const TEST_THRESHOLD: u64 = 512 * KIB;
    const TEST_CHUNK: usize = 64 * 1024;

    fn plan_for(size: u64) -> Option<super::SegmentPlan> {
        segment_plan(
            TransportProfile::wsl(TEST_CHUNK),
            true,
            false,
            false,
            &[StreamInfo::unnamed(size)],
            size,
            TEST_THRESHOLD,
            TEST_CHUNK,
            false,
            false,
        )
    }

    #[test]
    fn segment_plan_rejects_every_ineligible_modality() {
        let size = 4 * MIB;
        let streams = [StreamInfo::unnamed(size)];
        let wsl = TransportProfile::wsl(TEST_CHUNK);
        let eligible = |transport, source_wsl, destination_wsl| {
            segment_plan(
                transport,
                source_wsl,
                destination_wsl,
                false,
                &streams,
                size,
                TEST_THRESHOLD,
                TEST_CHUNK,
                false,
                false,
            )
        };
        // Baseline sanity: the eligible shape produces a plan at all.
        assert!(eligible(wsl, true, false).is_some());
        assert!(eligible(wsl, false, true).is_some());
        // Only the WSL transport qualifies.
        assert!(eligible(TransportProfile::redirector(TEST_CHUNK), true, false).is_none());
        assert!(eligible(TransportProfile::standard(TEST_CHUNK), true, false).is_none());
        // Exactly one endpoint may be WSL.
        assert!(eligible(wsl, true, true).is_none());
        assert!(eligible(wsl, false, false).is_none());
        // Sparse preservation keeps the allocated-range strategy.
        assert!(
            segment_plan(
                wsl,
                true,
                false,
                true,
                &streams,
                size,
                TEST_THRESHOLD,
                TEST_CHUNK,
                false,
                false,
            )
            .is_none()
        );
        // Named streams keep the ordered path.
        let with_named = [
            StreamInfo::unnamed(size),
            StreamInfo {
                name: OsString::from(":ads:$DATA"),
                size: 4,
            },
        ];
        assert!(
            segment_plan(
                wsl,
                true,
                false,
                false,
                &with_named,
                size,
                TEST_THRESHOLD,
                TEST_CHUNK,
                false,
                false,
            )
            .is_none()
        );
        // Below the threshold is not worth extra opens and threads.
        assert!(plan_for(TEST_THRESHOLD - 1).is_none());
        // Checkpoint-eligible files keep the ordered prefix contract.
        assert!(
            segment_plan(
                wsl,
                true,
                false,
                false,
                &streams,
                size,
                TEST_THRESHOLD,
                TEST_CHUNK,
                true,
                false,
            )
            .is_none()
        );
        // A live resume candidate keeps the ordered path (v1).
        assert!(
            segment_plan(
                wsl,
                true,
                false,
                false,
                &streams,
                size,
                TEST_THRESHOLD,
                TEST_CHUNK,
                false,
                true,
            )
            .is_none()
        );
        // A zero-byte chunk cannot produce aligned segments.
        assert!(
            segment_plan(
                wsl,
                true,
                false,
                false,
                &streams,
                size,
                TEST_THRESHOLD,
                0,
                false,
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn segment_plan_clamps_counts_and_covers_exactly() {
        // K = clamp(size / threshold, 2, 8), exact contiguous coverage,
        // chunk-aligned starts, remainder on the last segment.
        for (size, expected_segments) in [
            (TEST_THRESHOLD, 2),         // 1 → clamped up to 2
            (2 * TEST_THRESHOLD + 3, 2), // unaligned remainder
            (4 * TEST_THRESHOLD, 4),
            (8 * TEST_THRESHOLD, 8),
            (64 * TEST_THRESHOLD, 8), // clamped down to 8
        ] {
            let plan = plan_for(size);
            assert!(
                plan.as_ref()
                    .is_some_and(|plan| plan.ranges.len() == expected_segments),
                "unexpected segment count for size {size}: {plan:?}"
            );
            let Some(plan) = plan else {
                return;
            };
            let chunk = TEST_CHUNK as u64;
            let mut cursor = 0_u64;
            for (index, (offset, length)) in plan.ranges.iter().copied().enumerate() {
                assert_eq!(offset, cursor, "range {index} is not contiguous");
                assert_eq!(
                    offset % chunk,
                    0,
                    "range {index} start is not chunk-aligned"
                );
                assert!(length > 0, "range {index} is empty");
                if index + 1 < plan.ranges.len() {
                    assert_eq!(
                        length % chunk,
                        0,
                        "non-final range {index} is not whole chunks"
                    );
                }
                cursor += length;
            }
            assert_eq!(cursor, size, "ranges do not cover size {size} exactly");
        }
    }

    /// Byte pattern that makes any segment offset mix-up visible.
    fn patterned(size: usize) -> Vec<u8> {
        (0..size)
            .map(|index| {
                let low = u8::try_from(index & 0xFF).unwrap_or_default();
                let high = u8::try_from((index >> 8) & 0xFF).unwrap_or_default();
                low.wrapping_mul(31).wrapping_add(high)
            })
            .collect()
    }

    fn wsl_policy_settings(source_is_wsl: bool, chunk: usize) -> EngineSettings {
        let destination = if source_is_wsl {
            // WSL→Win: strict local NTFS destination.
            DestinationCaps::from_policy(&FilesystemPolicy::new(
                FileSystem::Ntfs,
                capabilities(true),
            ))
        } else {
            // Win→WSL: a Plan 9 destination policy built from raw volume
            // facts (endpoint drives the engine's WSL mechanics; the actual
            // test files stay in a local sandbox).
            let volume = |endpoint, filesystem| VolumeInfo {
                root: PathBuf::from(r"\\?\C:\"),
                endpoint,
                filesystem,
                serial: 0,
                maximum_component_length: 255,
                bytes_per_sector: 512,
                cluster_size: 4096,
                free_bytes_available: 0,
                total_bytes: 0,
                capabilities: capabilities(false),
                remote_query_latency: None,
            };
            DestinationCaps::from_policy(&FilesystemPolicy::from_volumes(
                &volume(EndpointKind::Local, FileSystem::Ntfs),
                &volume(EndpointKind::Wsl, FileSystem::Wsl),
            ))
        };
        EngineSettings {
            run_id: "segmented-test".to_owned(),
            tuning: EngineTuning {
                chunk_bytes: chunk,
                large_threshold: MIB,
                checkpoint_threshold: u64::MAX,
                transport: TransportProfile::wsl(chunk),
                verify: false,
                flush: false,
            },
            source: SourceCaps {
                supports_streams: true,
                supports_eas: false,
                is_wsl: source_is_wsl,
            },
            destination,
        }
    }

    /// Runs the segmented strategy end to end inside one sandbox and returns
    /// the engine result, counters, and the still-alive sandbox (dropping it
    /// removes everything). `None` only when sandbox setup itself failed.
    fn run_segmented(
        source_is_wsl: bool,
        data: &[u8],
        cancel: &dyn crate::transport::CancelProbe,
    ) -> Option<(
        Result<super::EngineResult, crate::error::OperationError>,
        Counters,
        tempfile::TempDir,
    )> {
        let sandbox = tempfile::tempdir().ok()?;
        let source_dir = sandbox.path().join("src");
        let destination_dir = sandbox.path().join("dst");
        std::fs::create_dir(&source_dir).ok()?;
        std::fs::create_dir(&destination_dir).ok()?;
        let source_path = source_dir.join("big.bin");
        std::fs::write(&source_path, data).ok()?;
        let destination_path = destination_dir.join("big.bin");
        let snapshot = EntrySnapshot {
            relative_path: PathBuf::from("big.bin"),
            metadata: metadata_at(&source_path).ok()?,
        };
        let settings = wsl_policy_settings(source_is_wsl, TEST_CHUNK);
        let phases = PhaseTracker::new();
        let request = EngineRequest {
            source_path: &source_path,
            destination_path: &destination_path,
            relative_destination_parent: None,
            relative_path: Path::new("big.bin"),
            source_snapshot: &snapshot,
            replacement_snapshot: None,
            settings: &settings,
            preserve_sparse: false,
            destination_metadata: snapshot.metadata.basic,
            cancel,
            promote_threshold: None,
            known_streams: None,
            phases: &phases,
        };
        let streams = [StreamInfo::unnamed(snapshot.metadata.size)];
        let plan = segment_plan(
            TransportProfile::wsl(TEST_CHUNK),
            source_is_wsl,
            !source_is_wsl,
            false,
            &streams,
            snapshot.metadata.size,
            TEST_THRESHOLD,
            TEST_CHUNK,
            false,
            false,
        )?;
        let mut counters = Counters::default();
        let mut source = SourceFile::open(&source_path).ok()?;
        let result = copy_streamed_segmented(
            &request,
            &mut counters,
            &mut source,
            &streams,
            None,
            false,
            true,
            None,
            &plan,
        );
        Some((result, counters, sandbox))
    }

    #[test]
    fn segmented_copy_publishes_identical_content_and_digest_both_directions() {
        let data = patterned(2 * 1024 * 1024 + 7);
        let expected_digest = format!("xxh3:{:032x}", xxhash_rust::xxh3::xxh3_128(&data));
        for source_is_wsl in [true, false] {
            let cancel = || false;
            let run = run_segmented(source_is_wsl, &data, &cancel);
            assert!(run.is_some(), "sandbox setup or planning failed");
            let Some((result, counters, sandbox)) = run else {
                return;
            };
            assert!(
                result.is_ok(),
                "segmented copy failed (source_is_wsl={source_is_wsl}): {:?}",
                result.err()
            );
            let Some(result) = result.ok() else {
                return;
            };
            assert_eq!(result.bytes, data.len() as u64);
            assert_eq!(result.digest.as_deref(), Some(expected_digest.as_str()));
            assert_eq!(result.streams_dropped, 0);
            assert_eq!(
                std::fs::read(sandbox.path().join("dst").join("big.bin")).ok(),
                Some(data.clone()),
                "published bytes differ (source_is_wsl={source_is_wsl})"
            );
            assert_eq!(counters.bytes_read_source, data.len() as u64);
            assert_eq!(counters.bytes_written_destination, data.len() as u64);
        }
    }

    #[test]
    fn segmented_copy_cancel_leaves_no_destination_object() {
        let data = patterned(2 * 1024 * 1024);
        let polls = AtomicUsize::new(0);
        let cancel = move || polls.fetch_add(1, Ordering::SeqCst) >= 3;
        let run = run_segmented(true, &data, &cancel);
        assert!(run.is_some(), "sandbox setup or planning failed");
        let Some((result, _counters, sandbox)) = run else {
            return;
        };
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.operation == CANCELED_MID_FILE),
            "expected graceful mid-file cancellation, got {:?}",
            result.as_ref().err().map(|error| &error.operation)
        );
        // No final-name object and no leftover temp: the delete-on-close
        // disposition removed the partial temporary on the error return.
        let destination_dir = sandbox.path().join("dst");
        assert!(!destination_dir.join("big.bin").exists());
        assert_eq!(
            std::fs::read_dir(&destination_dir)
                .ok()
                .map(Iterator::count),
            Some(0)
        );
    }
}
