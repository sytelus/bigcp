//! Deterministic bounded scenario generator.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::sandbox::SandboxRoot;

const HARD_WRITE_LIMIT: u64 = 1024 * 1024 * 1024;
/// VISION.md forbids large-scale tests outright (creating on the order of
/// hundreds of thousands of files or more). The generator enforces that
/// prohibition structurally: entry count is capped far below it, so no
/// scenario file — however written — can turn the testkit into a scale test.
const HARD_ENTRY_LIMIT: usize = 10_000;
/// Deep `a/b/c/…` chains stress nothing the path tests don't already cover
/// and can only slow runs down; scenarios stay shallow by construction.
const HARD_DEPTH_LIMIT: usize = 32;
/// Routine correctness scenarios stay small so the default suite runs in
/// seconds and is gentle on the drive. Anything above these limits is a
/// heavy scenario and requires the explicit [`HEAVY_TESTS_ENV`] opt-in;
/// the hard caps above remain absolute either way.
const ROUTINE_ENTRY_LIMIT: usize = 1_000;
const ROUTINE_WRITE_LIMIT: u64 = 64 * 1024 * 1024;

/// Environment variable that opts a run into heavy (thousands-of-files or
/// performance/stress) scenarios. It must be set to `1` by the operator on
/// the command line — code in this repository never sets it.
pub const HEAVY_TESTS_ENV: &str = "BIGCP_ALLOW_HEAVY_TESTS";

/// True when the operator explicitly opted this process into heavy scenarios.
#[must_use]
pub fn heavy_tests_enabled() -> bool {
    std::env::var_os(HEAVY_TESTS_ENV).is_some_and(|value| value == "1")
}

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
    let entries = scenario
        .directories
        .len()
        .saturating_add(scenario.files.len());
    if entries > HARD_ENTRY_LIMIT {
        bail!(
            "scenario declares {entries} entries; the generator caps at {HARD_ENTRY_LIMIT} \
             (VISION.md prohibits large-scale test trees)"
        );
    }
    if !heavy_tests_enabled() {
        if entries > ROUTINE_ENTRY_LIMIT {
            bail!(
                "scenario declares {entries} entries; routine runs cap at {ROUTINE_ENTRY_LIMIT}. \
                 Heavy scenarios require {HEAVY_TESTS_ENV}=1 set explicitly by the operator"
            );
        }
        if scenario.write_budget_bytes > ROUTINE_WRITE_LIMIT {
            bail!(
                "scenario declares a {} byte budget; routine runs cap at {ROUTINE_WRITE_LIMIT}. \
                 Heavy scenarios require {HEAVY_TESTS_ENV}=1 set explicitly by the operator",
                scenario.write_budget_bytes
            );
        }
    }
    for relative in scenario
        .directories
        .iter()
        .chain(scenario.files.iter().map(|file| &file.path))
    {
        let depth = relative.components().count();
        if depth > HARD_DEPTH_LIMIT {
            bail!(
                "scenario path {} has {depth} components; the generator caps depth at {HARD_DEPTH_LIMIT}",
                relative.display()
            );
        }
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

#[cfg(test)]
mod tests {
    use super::{FileSpec, Scenario, generate, heavy_tests_enabled};
    use crate::sandbox::SandboxRoot;
    use std::path::{Path, PathBuf};

    fn refusal_message(scenario: &Scenario) -> Option<String> {
        let Ok(sandbox) = SandboxRoot::create_system_temp("generator-caps") else {
            return None;
        };
        generate(&sandbox, Path::new("root"), scenario)
            .err()
            .map(|error| format!("{error:#}"))
    }

    fn declared_files(count: usize) -> Vec<FileSpec> {
        (0..count)
            .map(|index| FileSpec {
                path: PathBuf::from(format!("file-{index}")),
                size: 0,
                pattern: 0,
            })
            .collect()
    }

    #[test]
    fn routine_runs_refuse_thousands_of_entries_before_writing() {
        if heavy_tests_enabled() {
            return;
        }
        let scenario = Scenario {
            directories: Vec::new(),
            files: declared_files(1001),
            write_budget_bytes: 0,
        };
        let message = refusal_message(&scenario);
        assert!(
            message
                .as_deref()
                .is_some_and(|text| text.contains("BIGCP_ALLOW_HEAVY_TESTS")),
            "routine cap refusal must name the opt-in: {message:?}"
        );
    }

    #[test]
    fn routine_runs_refuse_large_write_budgets_before_writing() {
        if heavy_tests_enabled() {
            return;
        }
        let scenario = Scenario {
            directories: Vec::new(),
            files: Vec::new(),
            write_budget_bytes: 65 * 1024 * 1024,
        };
        let message = refusal_message(&scenario);
        assert!(
            message
                .as_deref()
                .is_some_and(|text| text.contains("BIGCP_ALLOW_HEAVY_TESTS")),
            "routine budget refusal must name the opt-in: {message:?}"
        );
    }

    #[test]
    fn absolute_entry_cap_holds_regardless_of_opt_in() {
        let scenario = Scenario {
            directories: Vec::new(),
            files: declared_files(10_001),
            write_budget_bytes: 0,
        };
        let message = refusal_message(&scenario);
        assert!(
            message
                .as_deref()
                .is_some_and(|text| text.contains("VISION.md")),
            "hard cap refusal must cite the prohibition: {message:?}"
        );
    }
}
