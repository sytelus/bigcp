//! Deterministic bounded scenario generator.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::sandbox::SandboxRoot;

const HARD_WRITE_LIMIT: u64 = 1024 * 1024 * 1024;

/// One generated file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileSpec {
    /// Relative path beneath the generated tree.
    pub path: PathBuf,
    /// File bytes.
    pub size: u64,
    /// Deterministic byte pattern seed.
    #[serde(default)]
    pub pattern: u64,
}

/// Harmless deterministic scenario.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Scenario {
    /// Directory paths created before files.
    #[serde(default)]
    pub directories: Vec<PathBuf>,
    /// File definitions.
    #[serde(default)]
    pub files: Vec<FileSpec>,
    /// Declared maximum bytes this scenario may write.
    pub write_budget_bytes: u64,
}

/// Exact generator accounting.
#[derive(Clone, Debug, Serialize)]
pub struct GenerationReport {
    /// Generated root.
    pub root: PathBuf,
    /// Directories created.
    pub directories: u64,
    /// Files created.
    pub files: u64,
    /// Bytes written.
    pub bytes_written: u64,
}

/// Generates a scenario only beneath a validated sandbox child.
pub fn generate(
    sandbox: &SandboxRoot,
    relative_root: &Path,
    scenario: &Scenario,
) -> Result<GenerationReport> {
    let declared = scenario
        .files
        .iter()
        .try_fold(0_u64, |sum, file| sum.checked_add(file.size))
        .context("scenario byte sum overflow")?;
    if declared > scenario.write_budget_bytes {
        bail!("scenario exceeds its declared write budget");
    }
    if scenario.write_budget_bytes > HARD_WRITE_LIMIT {
        bail!("ordinary generator hard limit is 1 GiB");
    }
    let root = sandbox.child(relative_root)?;
    fs::create_dir(&root).context("create new scenario root")?;
    let mut directories = 1_u64;
    for relative in &scenario.directories {
        let path = sandbox.child(&relative_root.join(relative))?;
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        directories = directories.saturating_add(1);
    }
    let mut files = 0_u64;
    let mut bytes_written = 0_u64;
    for file in &scenario.files {
        let path = sandbox.child(&relative_root.join(&file.path))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("create generated file parent")?;
        }
        let handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        let mut writer = BufWriter::new(handle);
        write_pattern(&mut writer, file.size, file.pattern)?;
        writer.flush().context("flush generated file")?;
        files = files.saturating_add(1);
        bytes_written = bytes_written.saturating_add(file.size);
    }
    Ok(GenerationReport {
        root,
        directories,
        files,
        bytes_written,
    })
}

fn write_pattern(writer: &mut impl Write, size: u64, seed: u64) -> Result<()> {
    let mut remaining = size;
    let mut state = seed | 1;
    let mut buffer = vec![0_u8; 64 * 1024];
    while remaining > 0 {
        for byte in &mut buffer {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state.to_le_bytes()[0];
        }
        let count = usize::try_from(remaining.min(buffer.len() as u64))
            .context("pattern chunk size overflow")?;
        writer.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    Ok(())
}
