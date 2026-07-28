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
