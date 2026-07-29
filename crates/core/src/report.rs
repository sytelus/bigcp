//! Self-contained, versioned JSON report model and atomic persistence.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use bigcp_win::publish_audit_temporary;

use crate::REPORT_SCHEMA_VERSION;
use crate::devprofile::CopyProfile;
use crate::error::{BigcpError, ErrorCategory, OperationError};
use crate::model::Counters;
use crate::stats::{AnalysisSummary, TimelinePoint};

/// Run identity and lifecycle facts.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunInfo {
    /// Unique run ID.
    pub id: String,
    /// RFC3339 UTC start.
    pub started: String,
    /// RFC3339 UTC completion.
    pub ended: String,
    /// Wall-clock seconds.
    pub duration_seconds: f64,
    /// Product exit code.
    pub exit: i32,
    /// Whether the run only modeled destination changes.
    pub dry_run: bool,
    /// logical or durable.
    pub durability: String,
    /// ok, degraded, or failed.
    pub audit: String,
    /// Source display path.
    pub source: String,
    /// Destination display path.
    pub destination: String,
    /// JSONL log location.
    pub log_path: PathBuf,
    /// Actual JSON report location.
    pub report_path: PathBuf,
}

/// Aggregated replacement statistics.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplacementSummary {
    /// Number of replacements.
    pub count: u64,
    /// Logical replacement bytes.
    pub bytes: u64,
    /// Replacements where destination was newer.
    pub destination_newer: u64,
    /// Counts by source top-level folder.
    pub by_folder: BTreeMap<String, u64>,
    /// Bounded examples.
    pub samples: Vec<ReplacementSample>,
}

/// One bounded replacement sample.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplacementSample {
    /// Relative path.
    pub relative_path: String,
    /// Previous size.
    pub old_size: u64,
    /// Previous last-write FILETIME.
    pub old_mtime: i64,
    /// Differing classifier fields.
    pub differences: Vec<String>,
}

/// One error category aggregation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorSummary {
    /// Stable category.
    pub category: ErrorCategory,
    /// Count across all raw codes.
    pub count: u64,
    /// Repair guidance.
    pub hint: String,
    /// Counts by top-level folder.
    pub by_folder: BTreeMap<String, u64>,
    /// At most 100 examples.
    pub samples: Vec<OperationError>,
}

/// Destination-only object summary.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExtraSummary {
    /// Total extras.
    pub count: u64,
    /// At most 100 relative paths.
    pub samples: Vec<String>,
}

/// One top-level folder's disjoint file outcomes.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FolderSummary {
    /// Files discovered below this top-level component.
    pub files_discovered: u64,
    /// Newly copied files.
    pub copied_new: u64,
    /// Replaced files.
    pub copied_replaced: u64,
    /// Equality skips.
    pub skipped_same: u64,
    /// Differing files withheld by policy.
    pub skipped_diff: u64,
    /// Metadata-only repairs.
    pub meta_fixed: u64,
    /// Failed files.
    pub failed: u64,
    /// Excluded files.
    pub excluded: u64,
    /// Files not attempted.
    pub not_attempted: u64,
    /// Source unnamed-stream bytes represented by all outcomes.
    pub logical_bytes_discovered: u64,
    /// Source unnamed-stream bytes represented by successful copies.
    pub logical_bytes_copied: u64,
}

/// Confidence-rated application-side bottleneck hypothesis.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BottleneckSummary {
    /// source-bound, destination-bound, balanced, or discovery-bound.
    pub hypothesis: String,
    /// low, medium, or high.
    pub confidence: String,
    /// Signals behind the hypothesis.
    pub evidence: String,
    /// Best sustained rate observed during actual run writes.
    pub observed_peak_mbps: f64,
    /// Whole-run average logical copy rate.
    pub average_mbps: f64,
    /// Average divided by observed peak.
    pub efficiency_vs_observed_peak: f64,
    /// Honest provenance statement.
    pub provenance: String,
}

/// Fastest and slowest active sampling windows from real copy traffic.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PhaseSummary {
    /// Highest destination-write-rate window, if the run wrote data.
    pub fastest: Option<TimelinePoint>,
    /// Lowest destination-write-rate active window, if the run wrote data.
    pub slowest: Option<TimelinePoint>,
}

/// Actionable report hint.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Hint {
    /// Stable hint identifier.
    pub id: String,
    /// Human-readable action.
    pub text: String,
    /// Confidence label.
    pub confidence: String,
}

/// Full verification summary.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct VerificationSummary {
    /// copied for post-copy or full for standalone.
    pub mode: String,
    /// Objects passing every strict field.
    pub passed: u64,
    /// Objects with a serious mismatch.
    pub failed: u64,
    /// Bounded mismatch details.
    pub mismatches: Vec<String>,
    /// Last-access differences, informational only.
    pub last_access_differences: u64,
}

/// Versioned report persisted by copy and verify runs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunReport {
    /// Schema version.
    pub v: u32,
    /// Run lifecycle.
    pub run: RunInfo,
    /// Semantic command configuration needed to interpret the run later.
    pub config: serde_json::Value,
    /// Query-derived device facts translated into deterministic static settings.
    pub devices: CopyProfile,
    /// Exact reconciled counters.
    pub counters: Counters,
    /// Replacement aggregate.
    pub replacements: ReplacementSummary,
    /// Error aggregates.
    pub errors: Vec<ErrorSummary>,
    /// Warning counts.
    pub warnings: BTreeMap<String, u64>,
    /// Destination-only entries.
    pub extras: ExtraSummary,
    /// Disjoint file outcomes grouped by source top-level component.
    pub folders: BTreeMap<String, FolderSummary>,
    /// Downsampled real-traffic samples.
    pub timeline: Vec<TimelinePoint>,
    /// Fastest and slowest active portions derived from the timeline.
    pub phases: PhaseSummary,
    /// Bottleneck analysis.
    pub bottleneck: BottleneckSummary,
    /// Actionable hints.
    pub hints: Vec<Hint>,
    /// Bounded `--analyze` insight; absent unless the flag was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<AnalysisSummary>,
    /// Optional verification result.
    pub verify: Option<VerificationSummary>,
    /// ok or failed counter-integrity state.
    pub integrity: String,
}

impl RunReport {
    /// Writes a report as temp, flush, atomic rename.
    pub fn write_atomic(&self, path: &Path) -> Result<(), BigcpError> {
        let parent = path
            .parent()
            .ok_or_else(|| BigcpError::Invalid("report path has no parent".to_owned()))?;
        fs::create_dir_all(parent)
            .map_err(|error| BigcpError::io("create report parent", error))?;
        let temp = parent.join(format!(".bigcp-report-{}.part", Uuid::new_v4().simple()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| BigcpError::io("create report temp", error))?;
        let mut writer = BufWriter::new(file);
        if let Err(error) = serde_json::to_writer_pretty(&mut writer, self) {
            drop(writer);
            let _ = fs::remove_file(&temp);
            return Err(BigcpError::Format(format!("serialize report: {error}")));
        }
        if let Err(error) = writer.flush().and_then(|()| writer.get_ref().sync_all()) {
            drop(writer);
            let _ = fs::remove_file(&temp);
            return Err(BigcpError::io("flush report temp", error));
        }
        drop(writer);
        if let Err(error) = publish_audit_temporary(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(BigcpError::io("publish report", error));
        }
        Ok(())
    }
}

/// Loads and version-checks a saved report.
pub fn load_report(path: &Path) -> Result<RunReport, BigcpError> {
    let file = File::open(path).map_err(|error| BigcpError::io("open report", error))?;
    let report: RunReport = serde_json::from_reader(BufReader::new(file))
        .map_err(|error| BigcpError::Format(format!("parse report: {error}")))?;
    if report.v != REPORT_SCHEMA_VERSION {
        return Err(BigcpError::Format(format!(
            "unsupported report schema {}; this build supports {}",
            report.v, REPORT_SCHEMA_VERSION
        )));
    }
    Ok(report)
}

/// Returns the top-level component used for grouping.
#[must_use]
pub fn top_level(path: &Path) -> String {
    path.components().next().map_or_else(
        || ".".to_owned(),
        |component| component.as_os_str().to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn public_schemas_are_valid_json_with_v1_identity() {
        for schema in [
            include_str!("../../../docs/schemas/log-v1.schema.json"),
            include_str!("../../../docs/schemas/report-v1.schema.json"),
        ] {
            let parsed = serde_json::from_str::<serde_json::Value>(schema);
            assert!(parsed.is_ok());
            assert!(parsed.ok().is_some_and(|value| {
                value.get("$schema").is_some() && value.get("$id").is_some()
            }));
        }
    }
}
