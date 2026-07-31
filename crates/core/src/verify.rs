//! The two verification forms: this-run read-back and standalone full-tree.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use bigcp_win::{
    BasicMetadata, ObjectKind, SourceFile, SourceStream, StreamInfo, absolute_extended,
    classify_endpoint, comparison_key, enumerate_directory, final_path, is_same_or_descendant_with,
    list_streams, metadata_at, open_root, probe_volume, read_extended_attributes,
    read_reparse_data,
};
use xxhash_rust::xxh3::Xxh3;

use crate::error::{BigcpError, ErrorCategory, OperationError};
use crate::filesystem::FilesystemPolicy;
use crate::model::Counters;
use crate::options::VerifyOptions;
use crate::report::VerificationSummary;

const MISMATCH_SAMPLE_LIMIT: usize = 100;
type DigestedFile = (u64, String, Vec<(StreamInfo, String)>, String);

#[derive(Clone, Copy)]
struct VerificationFeatures {
    streams: bool,
    eas: bool,
}

/// One file and source digest captured during a copy run.
pub struct VerificationTarget {
    /// Relative path used in mismatch reports.
    pub relative_path: PathBuf,
    /// Absolute destination path.
    pub destination_path: PathBuf,
    /// Digest computed from source buffers during copy.
    pub expected_digest: String,
    /// Exact expected logical size.
    pub expected_size: u64,
    /// Source metadata applied by the atomic finalizer.
    pub expected_metadata: BasicMetadata,
    /// Named stream digests captured from source buffers.
    pub expected_streams: Vec<(StreamInfo, String)>,
    /// Digest of the opaque EA payload copied from the source.
    pub expected_ea_digest: Option<String>,
}

/// Re-reads only files written during this run.
#[must_use]
pub(crate) fn verify_written_targets(
    targets: &[VerificationTarget],
    counters: &mut Counters,
    policy: FilesystemPolicy,
) -> VerificationSummary {
    let mut summary = VerificationSummary {
        mode: "copied".to_owned(),
        destination_filesystem: Some(policy.filesystem().name().to_owned()),
        projected: policy.is_degraded(),
        ..VerificationSummary::default()
    };
    for target in targets {
        match digest_file_streams_and_eas(
            &target.destination_path,
            counters,
            VerificationFeatures {
                streams: policy.supports_streams(),
                eas: policy.supports_eas(),
            },
        ) {
            Ok((size, digest, streams, ea_digest)) => match metadata_at(&target.destination_path) {
                Ok(metadata) => {
                    if !policy.last_access_equal(
                        metadata.basic.last_access_time,
                        target.expected_metadata.last_access_time,
                    ) {
                        summary.last_access_differences =
                            summary.last_access_differences.saturating_add(1);
                    }
                    let metadata_matches = policy.creation_equal(
                        target.expected_metadata.creation_time,
                        metadata.basic.creation_time,
                    ) && policy.last_write_equal(
                        target.expected_metadata.last_write_time,
                        metadata.basic.last_write_time,
                    ) && policy.attributes_equal(
                        target.expected_metadata.attributes,
                        metadata.basic.attributes,
                    );
                    if size == target.expected_size
                        && digest == target.expected_digest
                        && streams == target.expected_streams
                        && target
                            .expected_ea_digest
                            .as_deref()
                            .is_none_or(|expected| expected == ea_digest)
                        && metadata_matches
                    {
                        summary.passed = summary.passed.saturating_add(1);
                    } else {
                        summary.failed = summary.failed.saturating_add(1);
                        push_mismatch(
                            &mut summary,
                            format!(
                                "{}: expected size/hash/streams/ea/meta {}/{}/{:?}/{:?}/{:?}, observed {}/{}/{:?}/{}/{:?}",
                                target.relative_path.display(),
                                target.expected_size,
                                target.expected_digest,
                                target.expected_streams,
                                target.expected_ea_digest,
                                target.expected_metadata,
                                size,
                                digest,
                                streams,
                                ea_digest,
                                metadata.basic
                            ),
                        );
                    }
                }
                Err(error) => {
                    summary.failed = summary.failed.saturating_add(1);
                    push_mismatch(
                        &mut summary,
                        format!(
                            "{}: metadata read-back failed: {error}",
                            target.relative_path.display()
                        ),
                    );
                }
            },
            Err(error) => {
                summary.failed = summary.failed.saturating_add(1);
                push_mismatch(
                    &mut summary,
                    format!(
                        "{}: read-back failed: {error}",
                        target.relative_path.display()
                    ),
                );
            }
        }
    }
    summary
}

/// Fully verifies two trees by reading both sides; no hash cache is trusted.
pub fn run_standalone_verify(options: &VerifyOptions) -> Result<VerificationSummary, BigcpError> {
    let source_input = absolute_extended(&options.source)
        .map_err(|error| BigcpError::io("normalize verify source", error))?;
    let destination_input = absolute_extended(&options.destination)
        .map_err(|error| BigcpError::io("normalize verify destination", error))?;
    let source_pin =
        open_root(&source_input).map_err(|error| BigcpError::io("open verify source", error))?;
    let destination_pin = open_root(&destination_input)
        .map_err(|error| BigcpError::io("open verify destination", error))?;
    let source =
        final_path(&source_pin).map_err(|error| BigcpError::io("resolve verify source", error))?;
    let destination = final_path(&destination_pin)
        .map_err(|error| BigcpError::io("resolve verify destination", error))?;
    let source_endpoint = classify_endpoint(&source);
    let destination_endpoint = classify_endpoint(&destination);
    if is_same_or_descendant_with(
        &source,
        &destination,
        destination_endpoint.names_are_case_sensitive(),
    )
    .map_err(|error| BigcpError::io("compare verify roots", error))?
        || is_same_or_descendant_with(
            &destination,
            &source,
            source_endpoint.names_are_case_sensitive(),
        )
        .map_err(|error| BigcpError::io("compare verify roots", error))?
    {
        return Err(BigcpError::Invalid(
            "verification roots must be distinct, non-nested trees".to_owned(),
        ));
    }
    let source_volume =
        probe_volume(&source).map_err(|error| BigcpError::io("probe verify source", error))?;
    let destination_volume = probe_volume(&destination)
        .map_err(|error| BigcpError::io("probe verify destination", error))?;
    let policy = FilesystemPolicy::from_volumes(&source_volume, &destination_volume);
    let source_features = VerificationFeatures {
        streams: source_volume.capabilities.named_streams,
        eas: source_volume.capabilities.extended_attributes,
    };
    let destination_features = VerificationFeatures {
        streams: destination_volume.capabilities.named_streams,
        eas: destination_volume.capabilities.extended_attributes,
    };

    let mut summary = VerificationSummary {
        mode: "full".to_owned(),
        destination_filesystem: Some(destination_volume.filesystem.name().to_owned()),
        projected: policy.is_degraded(),
        ..VerificationSummary::default()
    };
    let mut counters = Counters::default();
    let source_root_metadata = metadata_at(&source)
        .map_err(|error| BigcpError::io("read verify source root metadata", error))?;
    let destination_root_metadata = metadata_at(&destination)
        .map_err(|error| BigcpError::io("read verify destination root metadata", error))?;
    if source_root_metadata.kind != ObjectKind::Directory
        || destination_root_metadata.kind != ObjectKind::Directory
    {
        return Err(BigcpError::Invalid(
            "verification roots must both resolve to real directories".to_owned(),
        ));
    }
    if !policy.last_access_equal(
        source_root_metadata.basic.last_access_time,
        destination_root_metadata.basic.last_access_time,
    ) {
        summary.last_access_differences = summary.last_access_differences.saturating_add(1);
    }
    let root_metadata_equal = policy.creation_equal(
        source_root_metadata.basic.creation_time,
        destination_root_metadata.basic.creation_time,
    ) && policy.last_write_equal(
        source_root_metadata.basic.last_write_time,
        destination_root_metadata.basic.last_write_time,
    ) && policy.attributes_equal(
        source_root_metadata.basic.attributes,
        destination_root_metadata.basic.attributes,
    );
    let root_aux_equal = directory_aux_equal(
        &source,
        &destination,
        &mut counters,
        true,
        source_features,
        destination_features,
    )
    .map_err(|error| BigcpError::io("verify root streams or EAs", error))?;
    if root_metadata_equal && root_aux_equal {
        summary.passed = summary.passed.saturating_add(1);
    } else {
        summary.failed = summary.failed.saturating_add(1);
        push_mismatch(
            &mut summary,
            ".: root directory metadata, named streams, or EAs differ".to_owned(),
        );
    }
    let mut tasks = vec![(source, destination, PathBuf::new())];
    while let Some((source_dir, destination_dir, relative_dir)) = tasks.pop() {
        let source_entries = enumerate_directory(&source_dir)
            .map_err(|error| BigcpError::io("enumerate verify source", error))?;
        let destination_entries = enumerate_directory(&destination_dir)
            .map_err(|error| BigcpError::io("enumerate verify destination", error))?;
        let mut destination_map = HashMap::new();
        for entry in destination_entries {
            let key = comparison_key(&entry.name, policy.names_are_case_sensitive())
                .map_err(|error| BigcpError::io("match verify destination name", error))?;
            if destination_map.insert(key, entry).is_some() {
                summary.failed = summary.failed.saturating_add(1);
                push_mismatch(
                    &mut summary,
                    format!(
                        "{}: destination has a name collision under its matching semantics",
                        relative_dir.display()
                    ),
                );
            }
        }
        let mut source_keys = HashSet::new();
        for source_entry in source_entries {
            let relative = relative_dir.join(&source_entry.name);
            let key = comparison_key(&source_entry.name, policy.names_are_case_sensitive())
                .map_err(|error| BigcpError::io("match verify source name", error))?;
            if !source_keys.insert(key.clone()) {
                summary.failed = summary.failed.saturating_add(1);
                push_mismatch(
                    &mut summary,
                    format!(
                        "{}: source has a name collision under destination matching semantics",
                        relative.display()
                    ),
                );
                continue;
            }
            let Some(destination_entry) = destination_map.remove(&key) else {
                summary.failed = summary.failed.saturating_add(1);
                push_mismatch(&mut summary, format!("{}: missing", relative.display()));
                continue;
            };
            if source_entry.metadata.kind != destination_entry.metadata.kind {
                summary.failed = summary.failed.saturating_add(1);
                push_mismatch(
                    &mut summary,
                    format!("{}: object type differs", relative.display()),
                );
                continue;
            }
            let metadata_equal = policy.creation_equal(
                source_entry.metadata.basic.creation_time,
                destination_entry.metadata.basic.creation_time,
            ) && policy.last_write_equal(
                source_entry.metadata.basic.last_write_time,
                destination_entry.metadata.basic.last_write_time,
            ) && policy.attributes_equal(
                source_entry.metadata.basic.attributes,
                destination_entry.metadata.basic.attributes,
            );
            if !policy.last_access_equal(
                source_entry.metadata.basic.last_access_time,
                destination_entry.metadata.basic.last_access_time,
            ) {
                summary.last_access_differences = summary.last_access_differences.saturating_add(1);
            }
            match source_entry.metadata.kind {
                ObjectKind::Directory => {
                    // A read failure is reported as unverifiable, never as a
                    // difference: "differ" is a statement about content and
                    // must not be fabricated from an inaccessible object.
                    match directory_aux_equal(
                        &source_entry.path,
                        &destination_entry.path,
                        &mut counters,
                        true,
                        source_features,
                        destination_features,
                    ) {
                        Ok(aux_equal) if metadata_equal && aux_equal => {
                            summary.passed = summary.passed.saturating_add(1);
                        }
                        Ok(_) => {
                            summary.failed = summary.failed.saturating_add(1);
                            push_mismatch(
                                &mut summary,
                                format!(
                                    "{}: directory metadata, named streams, or EAs differ",
                                    relative.display()
                                ),
                            );
                        }
                        Err(error) => {
                            summary.failed = summary.failed.saturating_add(1);
                            push_mismatch(
                                &mut summary,
                                format!(
                                    "{}: could not be verified (read failed: {error})",
                                    relative.display()
                                ),
                            );
                        }
                    }
                    tasks.push((source_entry.path, destination_entry.path, relative));
                }
                ObjectKind::File => {
                    let result = verify_file_pair(
                        &source_entry.path,
                        &destination_entry.path,
                        source_entry.metadata.size,
                        destination_entry.metadata.size,
                        metadata_equal,
                        &mut counters,
                        source_features,
                        destination_features,
                    );
                    match result {
                        Ok(()) => summary.passed = summary.passed.saturating_add(1),
                        Err(message) => {
                            summary.failed = summary.failed.saturating_add(1);
                            push_mismatch(
                                &mut summary,
                                format!("{}: {message}", relative.display()),
                            );
                        }
                    }
                }
                ObjectKind::Reparse => {
                    // As above: unreadable is reported as unverifiable, not
                    // as a fabricated difference.
                    let reparse_state =
                        read_reparse_data(&source_entry.path).and_then(|source_data| {
                            read_reparse_data(&destination_entry.path)
                                .map(|destination_data| source_data == destination_data)
                        });
                    let auxiliary_state = directory_aux_equal(
                        &source_entry.path,
                        &destination_entry.path,
                        &mut counters,
                        true,
                        source_features,
                        destination_features,
                    );
                    match (reparse_state, auxiliary_state) {
                        (Ok(reparse_equal), Ok(auxiliary_equal)) => {
                            if reparse_equal && metadata_equal && auxiliary_equal {
                                summary.passed = summary.passed.saturating_add(1);
                            } else {
                                summary.failed = summary.failed.saturating_add(1);
                                push_mismatch(
                                    &mut summary,
                                    format!(
                                        "{}: reparse payload, named streams, EAs, or metadata differ",
                                        relative.display()
                                    ),
                                );
                            }
                        }
                        (Err(error), _) | (_, Err(error)) => {
                            summary.failed = summary.failed.saturating_add(1);
                            push_mismatch(
                                &mut summary,
                                format!(
                                    "{}: could not be verified (read failed: {error})",
                                    relative.display()
                                ),
                            );
                        }
                    }
                }
            }
        }
        for extra in destination_map.into_values() {
            summary.failed = summary.failed.saturating_add(1);
            push_mismatch(
                &mut summary,
                format!("{}: extra", relative_dir.join(extra.name).display()),
            );
        }
    }
    Ok(summary)
}

fn verify_file_pair(
    source: &Path,
    destination: &Path,
    source_size: u64,
    destination_size: u64,
    metadata_equal: bool,
    counters: &mut Counters,
    source_features: VerificationFeatures,
    destination_features: VerificationFeatures,
) -> Result<(), String> {
    if source_size != destination_size {
        return Err(format!(
            "size differs: source={source_size}, destination={destination_size}"
        ));
    }
    if !metadata_equal {
        return Err("copied metadata differs".to_owned());
    }
    let comparison_features = VerificationFeatures {
        streams: destination_features.streams,
        eas: destination_features.eas,
    };
    let (_, source_digest, source_streams, source_eas) = digest_file_streams_and_eas(
        source,
        counters,
        VerificationFeatures {
            streams: comparison_features.streams && source_features.streams,
            eas: comparison_features.eas && source_features.eas,
        },
    )
    .map_err(|error| format!("could not be verified (read failed: {error})"))?;
    let (_, destination_digest, destination_streams, destination_eas) =
        digest_file_streams_and_eas(destination, counters, comparison_features)
            .map_err(|error| format!("could not be verified (read failed: {error})"))?;
    if source_digest != destination_digest
        || source_streams != destination_streams
        || source_eas != destination_eas
    {
        return Err(format!(
            "content, named streams, or EAs differ: source={source_digest}/{source_streams:?}/{source_eas}, destination={destination_digest}/{destination_streams:?}/{destination_eas}"
        ));
    }
    Ok(())
}

fn digest_file(path: &Path, counters: &mut Counters) -> Result<(u64, String), OperationError> {
    let mut file = SourceFile::open(path)
        .map_err(|error| OperationError::from_io("verify_open", path.to_path_buf(), &error))?;
    let opened = file.opened_metadata().clone();
    let mut hasher = Xxh3::new();
    let mut buffer = vec![0_u8; 8 * 1024 * 1024];
    let mut total = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| OperationError::from_io("verify_read", path.to_path_buf(), &error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        total = total.saturating_add(count as u64);
        counters.bytes_verified = counters.bytes_verified.saturating_add(count as u64);
    }
    let observed = file.current_metadata().map_err(|error| {
        OperationError::from_io("verify_revalidate", path.to_path_buf(), &error)
    })?;
    if observed.identity != opened.identity
        || observed.size != opened.size
        || observed.basic.last_write_time != opened.basic.last_write_time
        || observed.size != total
    {
        return Err(OperationError::semantic(
            ErrorCategory::SourceChanged,
            "verify_revalidate",
            path.to_path_buf(),
            "file size changed during verification",
        ));
    }
    Ok((total, format!("xxh3:{:032x}", hasher.digest128())))
}

fn digest_file_streams_and_eas(
    path: &Path,
    counters: &mut Counters,
    features: VerificationFeatures,
) -> Result<DigestedFile, OperationError> {
    let before = metadata_at(path)
        .map_err(|error| OperationError::from_io("verify_stat", path.to_path_buf(), &error))?;
    let (size, digest) = digest_file(path, counters)?;
    let named = if features.streams {
        digest_named_streams(path, counters, false)?
    } else {
        Vec::new()
    };
    let attributes = if features.eas {
        read_extended_attributes(path)
            .map_err(|error| OperationError::from_io("verify_ea", path.to_path_buf(), &error))?
    } else {
        bigcp_win::ExtendedAttributes::empty()
    };
    let ea_digest = format!(
        "xxh3:{:032x}",
        xxhash_rust::xxh3::xxh3_128(attributes.as_bytes())
    );
    let after = metadata_at(path).map_err(|error| {
        OperationError::from_io("verify_revalidate", path.to_path_buf(), &error)
    })?;
    if before.identity != after.identity
        || before.size != after.size
        || before.basic.last_write_time != after.basic.last_write_time
    {
        return Err(OperationError::semantic(
            ErrorCategory::SourceChanged,
            "verify_revalidate",
            path.to_path_buf(),
            "file identity, size, or last-write time changed during stream verification",
        ));
    }
    Ok((size, digest, named, ea_digest))
}

fn digest_named_streams(
    path: &Path,
    counters: &mut Counters,
    open_reparse: bool,
) -> Result<Vec<(StreamInfo, String)>, OperationError> {
    let streams = list_streams(path)
        .map_err(|error| OperationError::from_io("verify_streams", path.to_path_buf(), &error))?;
    let mut named = Vec::new();
    for stream in streams.into_iter().filter(|stream| !stream.is_unnamed()) {
        let opened = if open_reparse {
            SourceStream::open_reparse(path, &stream)
        } else {
            SourceStream::open(path, &stream)
        };
        let mut source = opened.map_err(|error| {
            OperationError::from_io("verify_open_stream", path.to_path_buf(), &error)
        })?;
        let mut hasher = Xxh3::new();
        let mut total = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let count = source.read(&mut buffer).map_err(|error| {
                OperationError::from_io("verify_read_stream", path.to_path_buf(), &error)
            })?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            total = total.saturating_add(count as u64);
            counters.bytes_verified = counters.bytes_verified.saturating_add(count as u64);
        }
        if total != stream.size {
            return Err(OperationError::semantic(
                ErrorCategory::SourceChanged,
                "verify_read_stream",
                path.to_path_buf(),
                "named stream size changed during verification",
            ));
        }
        named.push((stream, format!("xxh3:{:032x}", hasher.digest128())));
    }
    named.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    Ok(named)
}

fn directory_aux_equal(
    source: &Path,
    destination: &Path,
    counters: &mut Counters,
    open_reparse: bool,
    source_features: VerificationFeatures,
    destination_features: VerificationFeatures,
) -> std::io::Result<bool> {
    let source_streams = if destination_features.streams && source_features.streams {
        digest_named_streams(source, counters, open_reparse)
            .map_err(|error| std::io::Error::other(error.to_string()))?
    } else {
        Vec::new()
    };
    let destination_streams = if destination_features.streams {
        digest_named_streams(destination, counters, open_reparse)
            .map_err(|error| std::io::Error::other(error.to_string()))?
    } else {
        Vec::new()
    };
    let source_eas = if destination_features.eas && source_features.eas {
        read_extended_attributes(source)?
    } else {
        bigcp_win::ExtendedAttributes::empty()
    };
    let destination_eas = if destination_features.eas {
        read_extended_attributes(destination)?
    } else {
        bigcp_win::ExtendedAttributes::empty()
    };
    Ok(source_streams == destination_streams && source_eas == destination_eas)
}

fn push_mismatch(summary: &mut VerificationSummary, message: String) {
    if summary.mismatches.len() < MISMATCH_SAMPLE_LIMIT {
        summary.mismatches.push(message);
    }
}
