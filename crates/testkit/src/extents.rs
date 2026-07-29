//! Read-only extent-count evidence over a sandboxed tree.
//!
//! Benchmark runs record this report in `BENCHMARKS.md` to prove that
//! parallel copies did not fragment destination files (the preallocation
//! counter-measure, DESIGN.md). Measurement opens files read-only, never
//! follows reparse points, and changes nothing.

use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context, Result, bail};
use bigcp_win::{ObjectKind, extent_count, metadata_at};
use serde::Serialize;

use crate::sandbox::SandboxRoot;

/// Fragmentation evidence for one measured tree.
#[derive(Clone, Debug, Serialize)]
pub struct ExtentReport {
    /// Regular files measured.
    pub files: u64,
    /// Sum of physical extents across all measured files.
    pub total_extents: u64,
    /// Largest single-file extent count observed.
    pub max_extents: u64,
    /// Files backed by more than one physical extent.
    pub fragmented_files: u64,
}

/// Measures every regular file beneath a validated sandbox child.
///
/// The relative root may also name a single file. Directories are walked
/// without following reparse points; reparse objects are skipped.
pub fn measure_extents(sandbox: &SandboxRoot, relative: &Path) -> Result<ExtentReport> {
    let root = sandbox.child(relative)?;
    let mut report = ExtentReport {
        files: 0,
        total_extents: 0,
        max_extents: 0,
        fragmented_files: 0,
    };
    let root_kind = metadata_at(&root).context("inspect extent-measurement root")?;
    match root_kind.kind {
        ObjectKind::File => {
            measure_file(&root, &mut report)?;
            return Ok(report);
        }
        ObjectKind::Directory => {}
        ObjectKind::Reparse => bail!("extent measurement root must be a plain file or directory"),
    }
    let mut stack = vec![root];
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory)
            .with_context(|| format!("enumerate {}", directory.display()))?;
        for entry in entries {
            let path = entry.context("read directory entry")?.path();
            let metadata =
                metadata_at(&path).with_context(|| format!("inspect {}", path.display()))?;
            match metadata.kind {
                ObjectKind::Directory => stack.push(path),
                ObjectKind::File => measure_file(&path, &mut report)?,
                // Reparse points are never followed; links own no data runs
                // this measurement should attribute to the tree.
                ObjectKind::Reparse => {}
            }
        }
    }
    Ok(report)
}

fn measure_file(path: &Path, report: &mut ExtentReport) -> Result<()> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let extents =
        extent_count(&file).with_context(|| format!("count extents of {}", path.display()))?;
    report.files = report.files.saturating_add(1);
    report.total_extents = report.total_extents.saturating_add(extents);
    report.max_extents = report.max_extents.max(extents);
    if extents > 1 {
        report.fragmented_files = report.fragmented_files.saturating_add(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::measure_extents;
    use crate::sandbox::SandboxRoot;
    use std::fs;
    use std::path::Path;

    #[test]
    fn measures_only_regular_files_inside_the_sandbox() {
        let Ok(sandbox) = SandboxRoot::create_system_temp("extent-report") else {
            return;
        };
        let tree = sandbox.path().join("tree");
        assert!(fs::create_dir(&tree).is_ok());
        assert!(fs::create_dir(tree.join("nested")).is_ok());
        assert!(fs::write(tree.join("small.bin"), b"resident").is_ok());
        assert!(fs::write(tree.join("nested/data.bin"), vec![0x5A_u8; 128 * 1024]).is_ok());

        let report = measure_extents(&sandbox, Path::new("tree"));
        assert!(report.is_ok(), "{report:?}");
        let Some(report) = report.ok() else {
            return;
        };
        assert_eq!(report.files, 2);
        // 128 KiB spans at most 32 clusters; totals stay within that bound.
        assert!(report.total_extents <= 33, "{report:?}");
        assert!(report.max_extents <= 32, "{report:?}");
    }
}
