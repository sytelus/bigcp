//! Shared work-item, outcome, counter, and UI snapshot types.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bigcp_win::{COPYABLE_ATTRIBUTES, ObjectKind, ObjectMetadata};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorCategory, OperationError};

/// Source or destination enumeration snapshot used by the classifier.
#[derive(Clone, Debug)]
pub struct EntrySnapshot {
    /// Source-root-relative path.
    pub relative_path: PathBuf,
    /// Handle-derived Windows metadata.
    pub metadata: ObjectMetadata,
}

impl EntrySnapshot {
    /// Returns attributes that participate in copy semantics.
    #[must_use]
    pub const fn copyable_attributes(&self) -> u32 {
        self.metadata.basic.attributes & COPYABLE_ATTRIBUTES
    }

    /// Returns the non-following object class.
    #[must_use]
    pub const fn kind(&self) -> ObjectKind {
        self.metadata.kind
    }
}

/// Destination state recorded at classification time.
#[derive(Clone, Debug)]
pub enum DestinationState {
    /// No matching destination name existed.
    New,
    /// A matching object existed and must be revalidated before commit.
    Replace(EntrySnapshot),
}

/// Exact classifier result from PLAN.md section 4.1.
#[derive(Clone, Debug)]
pub enum Classification {
    /// Destination has no matching object.
    New,
    /// Data heuristic and examined metadata match.
    Same,
    /// Data heuristic matches but cheap metadata differs.
    MetadataDiff(Vec<&'static str>),
    /// Data differs and replacement is authorized.
    Replace {
        /// Differing fields used in audit output.
        fields: Vec<&'static str>,
        /// Whether the destination mtime is newer.
        destination_newer: bool,
    },
    /// Data differs but replacement was disabled.
    SkipDifferent {
        /// Differing fields used in audit output.
        fields: Vec<&'static str>,
        /// Whether the destination mtime is newer.
        destination_newer: bool,
    },
    /// Source and destination object classes differ.
    TypeConflict,
}

/// File terminal outcomes. Variants are deliberately disjoint.
#[derive(Clone, Debug)]
pub enum FileOutcome {
    /// Newly published file.
    CopiedNew {
        /// Logical bytes copied.
        bytes: u64,
        /// Optional xxh3-128 digest.
        digest: Option<String>,
    },
    /// Replacement of a differing file through the selected completion path.
    CopiedReplaced {
        /// Logical bytes copied.
        bytes: u64,
        /// Optional xxh3-128 digest.
        digest: Option<String>,
        /// Whether the previous destination was newer.
        destination_newer: bool,
        /// Previous destination logical size captured at classification.
        old_size: u64,
        /// Previous destination last-write time in FILETIME ticks.
        old_mtime: i64,
        /// Previous destination raw attributes.
        old_attributes: u32,
    },
    /// Metadata equality skip.
    SkippedSame {
        /// Logical bytes not rewritten.
        bytes: u64,
    },
    /// Differing file withheld by replace=false.
    ///
    /// Carries the same destination snapshot detail as a replacement so the
    /// withheld decision is fully analyzable later (F13/F20).
    SkippedDifferent {
        /// Logical bytes withheld.
        bytes: u64,
        /// Whether the existing destination was newer than the source.
        destination_newer: bool,
        /// Existing destination logical size captured at classification.
        old_size: u64,
        /// Existing destination last-write time in FILETIME ticks.
        old_mtime: i64,
        /// Existing destination raw attributes.
        old_attributes: u32,
    },
    /// Metadata repaired without unnamed-stream I/O.
    MetadataFixed {
        /// Logical file bytes represented by this outcome.
        bytes: u64,
    },
    /// Copy or metadata operation failed.
    Failed {
        /// Logical source bytes represented by this outcome.
        bytes: u64,
        /// Rich operation failure.
        error: OperationError,
    },
    /// Policy exclusion.
    Excluded {
        /// Logical source bytes excluded.
        bytes: u64,
        /// Stable exclusion reason.
        reason: String,
    },
    /// Discovered but not dispatched after a breaker or parent failure.
    NotAttempted {
        /// Logical source bytes represented by this outcome.
        bytes: u64,
        /// Stable reason.
        reason: String,
    },
}

/// Run lifecycle exposed to both terminal interfaces.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Startup and safety validation.
    Preflight,
    /// Source discovery and copy overlap.
    Copying,
    /// Post-copy verification.
    Verifying,
    /// Graceful cancellation is draining.
    Canceling,
    /// Run completed and audit artifacts are final.
    Complete,
    /// Fatal startup or invariant failure.
    Failed,
}

/// Exact accounting counters persisted in reports.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Counters {
    /// Plain files discovered.
    pub files_discovered: u64,
    /// New files copied.
    pub copied_new: u64,
    /// Existing files replaced through either completion protocol.
    pub copied_replaced: u64,
    /// Files skipped by the equality heuristic.
    pub skipped_same: u64,
    /// Differing files withheld by replace=false.
    pub skipped_diff: u64,
    /// Files repaired without data rewrite.
    pub meta_fixed: u64,
    /// Failed files.
    pub failed: u64,
    /// Excluded files.
    pub excluded: u64,
    /// Discovered files not attempted.
    pub not_attempted: u64,
    /// New files that a dry-run predicts would be copied.
    pub would_copy_new: u64,
    /// Existing files that a dry-run predicts would be replaced.
    pub would_copy_replaced: u64,
    /// Files that a dry-run predicts would receive metadata repair.
    pub would_meta_fix: u64,
    /// Destination-only entries.
    pub extra: u64,
    /// Directories discovered.
    pub dirs_discovered: u64,
    /// Directories whose final metadata completed.
    pub dir_done: u64,
    /// Directories whose metadata finalization failed.
    pub dirs_meta_failed: u64,
    /// Directories that could not be created or traversed.
    pub dirs_failed: u64,
    /// Excluded directories.
    pub dirs_excluded: u64,
    /// Directories whose finalization was modeled by a dry-run.
    pub dirs_planned: u64,
    /// Reparse entries discovered.
    pub links_discovered: u64,
    /// Links copied.
    pub links_copied: u64,
    /// Links that failed.
    pub links_failed: u64,
    /// Links excluded.
    pub links_excluded: u64,
    /// Links not attempted.
    pub links_not_attempted: u64,
    /// Equal reparse objects skipped without a write.
    pub links_skipped: u64,
    /// Reparse writes modeled by a dry-run.
    pub links_planned: u64,
    /// Unnamed-stream bytes seen at enumeration time; the basis for live
    /// remaining-work estimates (named-stream bytes join the logical totals
    /// only once a file is opened, so this can trail the logical figures).
    #[serde(default)]
    pub bytes_enumerated: u64,
    /// Source logical bytes represented by terminal outcomes; named streams
    /// join the total for files whose stream set was opened.
    pub bytes_logical_discovered: u64,
    /// Logical bytes represented by copied outcomes, including copied named
    /// streams.
    pub bytes_logical_copied: u64,
    /// Actual source bytes read by the copy engine.
    pub bytes_read_source: u64,
    /// Actual destination bytes written by the copy engine.
    pub bytes_written_destination: u64,
    /// Bytes read during verification.
    pub bytes_verified: u64,
}

impl Counters {
    /// Notes one file entry at *discovery* time (enumeration/subtree walks).
    ///
    /// Discovery and terminal outcomes are counted at different call sites on
    /// purpose: if any code path ever drops a discovered file without an
    /// outcome, `reconcile` detects it. Counting both from the same call would
    /// make the I6 equation a tautology.
    pub fn note_file_discovered(&mut self, enumerated_bytes: u64) {
        self.files_discovered = self.files_discovered.saturating_add(1);
        self.bytes_enumerated = self.bytes_enumerated.saturating_add(enumerated_bytes);
    }

    /// Applies exactly one terminal file outcome to the disjoint counters.
    pub fn apply_file(&mut self, outcome: &FileOutcome) {
        // `files_discovered` is deliberately NOT incremented here — see
        // `note_file_discovered`. Logical byte totals stay outcome-time
        // because the true all-streams size is only known once the file has
        // been opened (PLAN section 5.4: totals adjust as discovery refines).
        let bytes = match outcome {
            FileOutcome::CopiedNew { bytes, .. } => {
                self.copied_new = self.copied_new.saturating_add(1);
                self.bytes_logical_copied = self.bytes_logical_copied.saturating_add(*bytes);
                *bytes
            }
            FileOutcome::CopiedReplaced { bytes, .. } => {
                self.copied_replaced = self.copied_replaced.saturating_add(1);
                self.bytes_logical_copied = self.bytes_logical_copied.saturating_add(*bytes);
                *bytes
            }
            FileOutcome::SkippedSame { bytes } => {
                self.skipped_same = self.skipped_same.saturating_add(1);
                *bytes
            }
            FileOutcome::SkippedDifferent { bytes, .. } => {
                self.skipped_diff = self.skipped_diff.saturating_add(1);
                *bytes
            }
            FileOutcome::MetadataFixed { bytes } => {
                self.meta_fixed = self.meta_fixed.saturating_add(1);
                *bytes
            }
            FileOutcome::Failed { bytes, .. } => {
                self.failed = self.failed.saturating_add(1);
                *bytes
            }
            FileOutcome::Excluded { bytes, .. } => {
                self.excluded = self.excluded.saturating_add(1);
                *bytes
            }
            FileOutcome::NotAttempted { bytes, .. } => {
                self.not_attempted = self.not_attempted.saturating_add(1);
                *bytes
            }
        };
        self.bytes_logical_discovered = self.bytes_logical_discovered.saturating_add(bytes);
    }

    /// Verifies the disjoint file, directory, and link equations.
    pub fn reconcile(&self) -> Result<(), String> {
        let files = self
            .copied_new
            .saturating_add(self.copied_replaced)
            .saturating_add(self.skipped_same)
            .saturating_add(self.skipped_diff)
            .saturating_add(self.meta_fixed)
            .saturating_add(self.failed)
            .saturating_add(self.excluded)
            .saturating_add(self.not_attempted);
        if files != self.files_discovered {
            return Err(format!(
                "file counter mismatch: discovered={}, terminal={files}",
                self.files_discovered
            ));
        }
        let directories = self
            .dir_done
            .saturating_add(self.dirs_meta_failed)
            .saturating_add(self.dirs_failed)
            .saturating_add(self.dirs_excluded)
            .saturating_add(self.dirs_planned);
        if directories != self.dirs_discovered {
            return Err(format!(
                "directory counter mismatch: discovered={}, terminal={directories}",
                self.dirs_discovered
            ));
        }
        let links = self
            .links_copied
            .saturating_add(self.links_failed)
            .saturating_add(self.links_excluded)
            .saturating_add(self.links_not_attempted)
            .saturating_add(self.links_skipped)
            .saturating_add(self.links_planned);
        if links != self.links_discovered {
            return Err(format!(
                "link counter mismatch: discovered={}, terminal={links}",
                self.links_discovered
            ));
        }
        if self.bytes_logical_copied > self.bytes_logical_discovered {
            return Err(format!(
                "copied logical bytes exceed discovered source bytes: copied={}, discovered={}",
                self.bytes_logical_copied, self.bytes_logical_discovered
            ));
        }
        Ok(())
    }
}

/// Immutable, inexpensive state sent to an observer.
#[derive(Clone, Debug, Serialize)]
pub struct RunSnapshot {
    /// Current lifecycle state.
    pub state: RunState,
    /// Current exact counters.
    pub counters: Counters,
    /// Current read rate.
    pub read_bytes_per_second: f64,
    /// Current write rate.
    pub write_bytes_per_second: f64,
    /// Running failures by category.
    pub failures_by_category: BTreeMap<ErrorCategory, u64>,
    /// Most recently active relative paths.
    pub active_paths: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::{Counters, FileOutcome};

    #[test]
    fn file_outcomes_reconcile_exactly() {
        let mut counters = Counters::default();
        counters.note_file_discovered(4);
        counters.apply_file(&FileOutcome::CopiedNew {
            bytes: 4,
            digest: None,
        });
        counters.note_file_discovered(5);
        counters.apply_file(&FileOutcome::SkippedSame { bytes: 5 });
        assert!(counters.reconcile().is_ok());
        assert_eq!(counters.files_discovered, 2);
        assert_eq!(counters.bytes_enumerated, 9);
        assert_eq!(counters.bytes_logical_discovered, 9);
    }

    #[test]
    fn dropped_outcome_is_loud() {
        // Discovery without a terminal outcome must fail reconciliation —
        // this is the direction the old outcome-time counting could not see.
        let mut counters = Counters::default();
        counters.note_file_discovered(1);
        assert!(counters.reconcile().is_err());
    }

    #[test]
    fn outcome_without_discovery_is_loud() {
        let mut counters = Counters::default();
        counters.apply_file(&FileOutcome::SkippedSame { bytes: 1 });
        assert!(counters.reconcile().is_err());
    }
}
