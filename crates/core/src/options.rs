//! Validated, UI-independent option types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Static device classes from PLAN.md section 8.2.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceClass {
    /// Automatic, capability-based selection.
    Auto,
    /// Internal NVMe-class storage.
    Nvme,
    /// SATA solid-state storage.
    SataSsd,
    /// UASP USB solid-state storage.
    UsbSsd,
    /// Rotational storage.
    Hdd,
    /// Conservative fallback profile.
    Unknown,
}

/// Advanced performance overrides exposed through the single tune hatch.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TuneOptions {
    /// Streaming chunk bytes.
    pub chunk_bytes: Option<usize>,
    /// Concurrent large-file streams.
    pub streams: Option<usize>,
    /// Small-file worker count.
    pub threads: Option<usize>,
    /// Buffer-pool memory budget.
    pub memory_bytes: Option<usize>,
    /// Small/large engine threshold.
    pub large_threshold: Option<u64>,
    /// Partial checkpoint eligibility threshold.
    pub checkpoint_threshold: Option<u64>,
}

/// Complete copy-run configuration.
#[derive(Clone, Debug)]
pub struct CopyOptions {
    /// Source tree root.
    pub source: PathBuf,
    /// Destination tree root.
    pub destination: PathBuf,
    /// Enumerate and classify without destination-tree writes.
    pub dry_run: bool,
    /// Verify files written by this run after copying.
    pub verify: bool,
    /// Include root-level operating-system artifacts.
    pub include_system: bool,
    /// Exclude cloud placeholders instead of hydrating them.
    pub skip_cloud: bool,
    /// Replace differing destination files.
    pub replace: bool,
    /// Request per-file durable flushing.
    pub flush: bool,
    /// Disable sparse-layout preservation.
    pub no_sparse: bool,
    /// Collect bounded live-run insight (size-class timings, slowest files,
    /// finer stat cadence) for later analysis — the VISION analysis flag.
    pub analyze: bool,
    /// Permit unknown reparse buffers to be copied verbatim.
    pub raw_reparse: bool,
    /// Ignore prior journal checkpoints.
    pub fresh: bool,
    /// Optional audit state directory.
    pub state_dir: Option<PathBuf>,
    /// Optional JSONL log path.
    pub log_path: Option<PathBuf>,
    /// Optional JSON report path.
    pub report_path: Option<PathBuf>,
    /// Requested source static profile.
    pub source_profile: DeviceClass,
    /// Requested destination static profile.
    pub destination_profile: DeviceClass,
    /// Advanced bounded overrides.
    pub tune: TuneOptions,
}

impl CopyOptions {
    /// Constructs conservative product defaults for one source/destination pair.
    #[must_use]
    pub fn new(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source,
            destination,
            dry_run: false,
            verify: false,
            include_system: false,
            skip_cloud: false,
            replace: true,
            flush: false,
            no_sparse: false,
            analyze: false,
            raw_reparse: false,
            fresh: false,
            state_dir: None,
            log_path: None,
            report_path: None,
            source_profile: DeviceClass::Auto,
            destination_profile: DeviceClass::Auto,
            tune: TuneOptions::default(),
        }
    }

    /// Returns the configured large-file threshold.
    #[must_use]
    pub fn large_threshold(&self) -> u64 {
        self.tune.large_threshold.unwrap_or(4 * 1024 * 1024)
    }

    /// Returns the configured checkpoint threshold.
    #[must_use]
    pub fn checkpoint_threshold(&self) -> u64 {
        self.tune
            .checkpoint_threshold
            .unwrap_or(16 * 1024 * 1024 * 1024)
    }
}

/// Standalone full-tree verification configuration.
///
/// There is deliberately no persisted-report option: standalone verification
/// prints its complete machine-readable summary to stdout and sets the exit
/// code, which covers scripting without a second report format to maintain.
#[derive(Clone, Debug)]
pub struct VerifyOptions {
    /// Source tree root.
    pub source: PathBuf,
    /// Destination tree root.
    pub destination: PathBuf,
}
