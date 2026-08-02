//! Independent, deliberately simple tree oracle.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bigcp_win::{
    COPYABLE_ATTRIBUTES, ObjectKind, SourceFile, SourceStream, enumerate_directory, list_streams,
    metadata_at, ordinal_case_key, read_extended_attributes, read_reparse_data,
};
use serde::Serialize;
use xxhash_rust::xxh3::Xxh3;

use crate::sandbox::SandboxRoot;

const SAMPLE_LIMIT: usize = 1_000;

/// Machine-readable independent comparison result.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CheckReport {
    /// Objects checked.
    pub checked: u64,
    /// Strict mismatches.
    pub mismatches: u64,
    /// Destination extras.
    pub extras: u64,
    /// Informational last-access differences.
    pub last_access_differences: u64,
    /// Bounded detail samples.
    pub samples: Vec<String>,
}

/// Compares two sandbox-confined trees without importing core copy logic.
pub fn check_trees(
    sandbox: &SandboxRoot,
    source_relative: &Path,
    destination_relative: &Path,
) -> Result<CheckReport> {
    let source = sandbox.child(source_relative)?;
    let destination = sandbox.child(destination_relative)?;
    let mut report = CheckReport::default();
    let source_root = metadata_at(&source).context("read source-root metadata")?;
    let destination_root = metadata_at(&destination).context("read destination-root metadata")?;
    report.checked = report.checked.saturating_add(1);
    if source_root.kind != ObjectKind::Directory || destination_root.kind != ObjectKind::Directory {
        mismatch(&mut report, "tree root is not a real directory".to_owned());
        return Ok(report);
    }
    compare_metadata(&mut report, Path::new("."), &source_root, &destination_root);
    compare_auxiliary_data(
        &mut report,
        Path::new("."),
        &source,
        &destination,
        false,
        true,
    )?;
    let mut tasks = vec![(source, destination, PathBuf::new())];
    while let Some((source_dir, destination_dir, relative_dir)) = tasks.pop() {
        let source_entries = enumerate_directory(&source_dir)
            .with_context(|| format!("enumerate {}", source_dir.display()))?;
        let destination_entries = enumerate_directory(&destination_dir)
            .with_context(|| format!("enumerate {}", destination_dir.display()))?;
        let mut destination_map = HashMap::new();
        for entry in destination_entries {
            let key = ordinal_case_key(&entry.name)?;
            if destination_map.insert(key, entry).is_some() {
                mismatch(
                    &mut report,
                    format!("{}: destination name collision", relative_dir.display()),
                );
            }
        }
        let mut source_keys = HashSet::new();
        for source_entry in source_entries {
            report.checked = report.checked.saturating_add(1);
            let relative = relative_dir.join(&source_entry.name);
            let key = ordinal_case_key(&source_entry.name)?;
            if !source_keys.insert(key.clone()) {
                mismatch(
                    &mut report,
                    format!("{}: source name collision", relative.display()),
                );
                continue;
            }
            let Some(destination_entry) = destination_map.remove(&key) else {
                mismatch(&mut report, format!("{}: missing", relative.display()));
                continue;
            };
            if source_entry.metadata.kind != destination_entry.metadata.kind {
                mismatch(&mut report, format!("{}: type differs", relative.display()));
                continue;
            }
            compare_metadata(
                &mut report,
                &relative,
                &source_entry.metadata,
                &destination_entry.metadata,
            );
            match source_entry.metadata.kind {
                ObjectKind::Directory => {
                    compare_auxiliary_data(
                        &mut report,
                        &relative,
                        &source_entry.path,
                        &destination_entry.path,
                        false,
                        true,
                    )?;
                    tasks.push((source_entry.path, destination_entry.path, relative));
                }
                ObjectKind::File => {
                    compare_file(
                        &mut report,
                        &relative,
                        &source_entry.path,
                        &destination_entry.path,
                    )?;
                }
                ObjectKind::Reparse => {
                    let source_data = read_reparse_data(&source_entry.path)?;
                    let destination_data = read_reparse_data(&destination_entry.path)?;
                    if source_data.tag != destination_data.tag
                        || source_data.bytes != destination_data.bytes
                    {
                        mismatch(
                            &mut report,
                            format!("{}: reparse buffer differs", relative.display()),
                        );
                    }
                    compare_auxiliary_data(
                        &mut report,
                        &relative,
                        &source_entry.path,
                        &destination_entry.path,
                        false,
                        true,
                    )?;
                }
            }
        }
        for extra in destination_map.into_values() {
            report.extras = report.extras.saturating_add(1);
            mismatch(
                &mut report,
                format!("{}: extra", relative_dir.join(extra.name).display()),
            );
        }
    }
    Ok(report)
}

fn compare_metadata(
    report: &mut CheckReport,
    relative: &Path,
    source: &bigcp_win::ObjectMetadata,
    destination: &bigcp_win::ObjectMetadata,
) {
    if source.basic.creation_time != destination.basic.creation_time
        || source.basic.last_write_time != destination.basic.last_write_time
        || source.basic.attributes & COPYABLE_ATTRIBUTES
            != destination.basic.attributes & COPYABLE_ATTRIBUTES
    {
        mismatch(
            report,
            format!(
                "{}: copied metadata differs (source ctime/mtime/attrs={}/{}/{:#x}, destination={}/{}/{:#x})",
                relative.display(),
                source.basic.creation_time,
                source.basic.last_write_time,
                source.basic.attributes & COPYABLE_ATTRIBUTES,
                destination.basic.creation_time,
                destination.basic.last_write_time,
                destination.basic.attributes & COPYABLE_ATTRIBUTES
            ),
        );
    }
    if source.basic.last_access_time != destination.basic.last_access_time {
        report.last_access_differences = report.last_access_differences.saturating_add(1);
    }
}

fn compare_file(
    report: &mut CheckReport,
    relative: &Path,
    source: &Path,
    destination: &Path,
) -> Result<()> {
    let source_streams = hash_streams(source)?;
    let destination_streams = hash_streams(destination)?;
    if source_streams != destination_streams {
        mismatch(
            report,
            format!("{}: data stream set or content differs", relative.display()),
        );
    }
    compare_eas(report, relative, source, destination)?;
    Ok(())
}

fn compare_auxiliary_data(
    report: &mut CheckReport,
    relative: &Path,
    source: &Path,
    destination: &Path,
    include_unnamed: bool,
    open_reparse: bool,
) -> Result<()> {
    let source_streams = hash_selected_streams(source, include_unnamed, open_reparse)?;
    let destination_streams = hash_selected_streams(destination, include_unnamed, open_reparse)?;
    if source_streams != destination_streams {
        mismatch(
            report,
            format!(
                "{}: named stream set or content differs",
                relative.display()
            ),
        );
    }
    compare_eas(report, relative, source, destination)
}

fn compare_eas(
    report: &mut CheckReport,
    relative: &Path,
    source: &Path,
    destination: &Path,
) -> Result<()> {
    let source_eas = read_extended_attributes(source)
        .with_context(|| format!("read source EAs for {}", relative.display()))?;
    let destination_eas = read_extended_attributes(destination)
        .with_context(|| format!("read destination EAs for {}", relative.display()))?;
    if source_eas != destination_eas {
        mismatch(report, format!("{}: EA blob differs", relative.display()));
    }
    Ok(())
}

fn hash_streams(path: &Path) -> Result<Vec<(Vec<u16>, u64, u128)>> {
    hash_selected_streams(path, true, false)
}

fn hash_selected_streams(
    path: &Path,
    include_unnamed: bool,
    open_reparse: bool,
) -> Result<Vec<(Vec<u16>, u64, u128)>> {
    use std::os::windows::ffi::OsStrExt;

    let mut result = Vec::new();
    for stream in list_streams(path)? {
        if stream.is_unnamed() && !include_unnamed {
            continue;
        }
        let mut hasher = Xxh3::new();
        let mut total = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        if stream.is_unnamed() {
            let mut source = SourceFile::open(path)?;
            loop {
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                total = total.saturating_add(count as u64);
                hasher.update(&buffer[..count]);
            }
        } else {
            let mut source = if open_reparse {
                SourceStream::open_reparse(path, &stream)?
            } else {
                SourceStream::open(path, &stream)?
            };
            loop {
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                total = total.saturating_add(count as u64);
                hasher.update(&buffer[..count]);
            }
        }
        result.push((
            stream.name.encode_wide().collect::<Vec<_>>(),
            total,
            hasher.digest128(),
        ));
    }
    result.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(result)
}

fn mismatch(report: &mut CheckReport, message: String) {
    report.mismatches = report.mismatches.saturating_add(1);
    if report.samples.len() < SAMPLE_LIMIT {
        report.samples.push(message);
    }
}

#[cfg(test)]
mod tests {
    use super::{CheckReport, check_trees};
    use crate::sandbox::SandboxRoot;
    use bigcp_win::{DestinationStream, StreamInfo};
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    // Windows requires this create flag to obtain a directory handle.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    // Creates one sandbox holding empty `source` and `destination` trees.
    fn tree_pair(label: &str) -> Option<SandboxRoot> {
        let Ok(sandbox) = SandboxRoot::create_system_temp(label) else {
            return None;
        };
        assert!(fs::create_dir(sandbox.path().join("source")).is_ok());
        assert!(fs::create_dir(sandbox.path().join("destination")).is_ok());
        Some(sandbox)
    }

    // Runs the oracle over the fixture pair and returns its report.
    fn compared(sandbox: &SandboxRoot) -> Option<CheckReport> {
        let report = check_trees(sandbox, Path::new("source"), Path::new("destination"));
        assert!(report.is_ok(), "check_trees failed: {report:?}");
        report.ok()
    }

    // Every negative fixture must be flagged with a sample naming the finding;
    // a regression that silently reports zero mismatches would otherwise make
    // each oracle-backed end-to-end assertion pass vacuously.
    fn assert_flagged(report: &CheckReport, needle: &str) {
        assert!(
            report.mismatches > 0,
            "oracle reported a known-bad pair as clean: {report:?}"
        );
        assert!(
            report.samples.iter().any(|sample| sample.contains(needle)),
            "no sample contains {needle:?}: {:?}",
            report.samples
        );
    }

    // Pins every compared timestamp to one fixed value so independently
    // created fixtures can be metadata-identical (or deliberately different)
    // without racing the clock.
    fn stamp(path: &Path, directory: bool, seconds: u64) -> std::io::Result<()> {
        use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
        let mut options = fs::OpenOptions::new();
        options.write(true);
        if directory {
            options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        options.open(path)?.set_times(
            fs::FileTimes::new()
                .set_created(fixed)
                .set_modified(fixed)
                .set_accessed(fixed),
        )
    }

    #[test]
    fn identical_trees_report_zero_mismatches() {
        let Some(sandbox) = tree_pair("oracle-identical") else {
            return;
        };
        for root in ["source", "destination"] {
            let base = sandbox.path().join(root);
            assert!(fs::create_dir(base.join("nested")).is_ok());
            let file = base.join("nested").join("file.txt");
            assert!(fs::write(&file, b"identical tiny fixture").is_ok());
            // Every object needs pinned times because the oracle compares
            // creation and last-write values (stamping a child never touches
            // its parent, so order is irrelevant once creation is done).
            assert!(stamp(&file, false, 1_700_000_000).is_ok());
            assert!(stamp(&base.join("nested"), true, 1_700_000_000).is_ok());
            assert!(stamp(&base, true, 1_700_000_000).is_ok());
        }
        let Some(report) = compared(&sandbox) else {
            return;
        };
        assert_eq!(report.mismatches, 0, "samples: {:?}", report.samples);
        assert_eq!(report.extras, 0);
        assert_eq!(report.checked, 3, "root pair, nested, and one file");
    }

    #[test]
    fn differing_content_at_equal_size_is_flagged() {
        let Some(sandbox) = tree_pair("oracle-content") else {
            return;
        };
        assert!(fs::write(sandbox.path().join("source").join("same.bin"), b"content-A").is_ok());
        assert!(
            fs::write(
                sandbox.path().join("destination").join("same.bin"),
                b"content-B"
            )
            .is_ok()
        );
        let Some(report) = compared(&sandbox) else {
            return;
        };
        assert_flagged(&report, "same.bin: data stream set or content differs");
    }

    #[test]
    fn differing_file_size_is_flagged() {
        let Some(sandbox) = tree_pair("oracle-size") else {
            return;
        };
        assert!(fs::write(sandbox.path().join("source").join("grew.bin"), b"short").is_ok());
        assert!(
            fs::write(
                sandbox.path().join("destination").join("grew.bin"),
                b"deliberately longer"
            )
            .is_ok()
        );
        let Some(report) = compared(&sandbox) else {
            return;
        };
        assert_flagged(&report, "grew.bin: data stream set or content differs");
    }

    #[test]
    fn missing_destination_file_is_flagged() {
        let Some(sandbox) = tree_pair("oracle-missing") else {
            return;
        };
        assert!(fs::write(sandbox.path().join("source").join("gone.txt"), b"present").is_ok());
        let Some(report) = compared(&sandbox) else {
            return;
        };
        assert_flagged(&report, "gone.txt: missing");
    }

    #[test]
    fn extra_destination_file_is_flagged() {
        let Some(sandbox) = tree_pair("oracle-extra") else {
            return;
        };
        assert!(
            fs::write(
                sandbox.path().join("destination").join("surplus.txt"),
                b"unexpected"
            )
            .is_ok()
        );
        let Some(report) = compared(&sandbox) else {
            return;
        };
        assert_flagged(&report, "surplus.txt: extra");
        assert!(report.extras > 0, "extras counter: {report:?}");
    }

    #[test]
    fn file_versus_directory_type_conflict_is_flagged() {
        let Some(sandbox) = tree_pair("oracle-type") else {
            return;
        };
        assert!(fs::write(sandbox.path().join("source").join("node"), b"a file here").is_ok());
        assert!(fs::create_dir(sandbox.path().join("destination").join("node")).is_ok());
        let Some(report) = compared(&sandbox) else {
            return;
        };
        assert_flagged(&report, "node: type differs");
    }

    #[test]
    fn named_stream_difference_is_flagged() {
        let Some(sandbox) = tree_pair("oracle-ads") else {
            return;
        };
        let destination_host = sandbox.path().join("destination").join("host.txt");
        assert!(fs::write(sandbox.path().join("source").join("host.txt"), b"same base").is_ok());
        assert!(fs::write(&destination_host, b"same base").is_ok());
        let stream = StreamInfo {
            name: OsString::from(":oracle-only:$DATA"),
            size: 9,
        };
        let created = DestinationStream::create(&destination_host, &stream, true);
        assert!(created.is_ok(), "creating the fixture stream failed");
        let Some(mut alternate) = created.ok() else {
            return;
        };
        assert!(alternate.write_all(b"ads bytes").is_ok());
        assert!(alternate.flush().is_ok());
        drop(alternate);
        let Some(report) = compared(&sandbox) else {
            return;
        };
        // File stream sets are hashed together with the unnamed content, so
        // a destination-only ADS must surface as a stream-set mismatch.
        assert_flagged(&report, "host.txt: data stream set or content differs");
    }

    #[test]
    fn timestamp_difference_is_flagged() {
        let Some(sandbox) = tree_pair("oracle-times") else {
            return;
        };
        let source_file = sandbox.path().join("source").join("stamped.txt");
        let destination_file = sandbox.path().join("destination").join("stamped.txt");
        assert!(fs::write(&source_file, b"same bytes").is_ok());
        assert!(fs::write(&destination_file, b"same bytes").is_ok());
        assert!(stamp(&source_file, false, 1_700_000_000).is_ok());
        assert!(stamp(&destination_file, false, 1_700_000_600).is_ok());
        let Some(report) = compared(&sandbox) else {
            return;
        };
        assert_flagged(&report, "stamped.txt: copied metadata differs");
    }
}
