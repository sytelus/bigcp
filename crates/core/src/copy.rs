//! End-to-end copy orchestration and iterative directory join.
//!
//! Pre-flight resolves and pins both roots, enforces remote/degraded-filesystem
//! acceptance, rejects unsupported local volumes, validates audit-path
//! containment, and acquires the exact-root machine-wide lock before
//! enumeration begins.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bigcp_win::{
    DestinationLock, DestinationStream, DirectoryEntry, FileSystem, ObjectKind, ObjectMetadata,
    SourceStream, VolumeInfo, absolute_extended, classify_endpoint,
    clear_extended_attributes_checked, comparison_key, copy_reparse, create_directory,
    display_path, enumerate_directory, final_path, is_cloud_placeholder, is_compressed,
    is_same_or_descendant_with, is_sparse, list_streams, metadata_at, open_root, probe_volume,
    read_extended_attributes_checked, set_basic_at_checked, write_extended_attributes_checked,
};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::REPORT_SCHEMA_VERSION;
use crate::audit::{AuditEvent, AuditPath, AuditWriter, ReplacementEvent, option_summary};
use crate::classify::classify;
use crate::devprofile::{CopyProfile, select_copy_profile};
use crate::engine::{EngineRequest, copy_file};
use crate::error::{BigcpError, ErrorCategory, OperationError};
use crate::filesystem::FilesystemPolicy;
use crate::journal::{Journal, JournalEvent, path_key};
use crate::model::{Classification, Counters, EntrySnapshot, FileOutcome, RunSnapshot, RunState};
use crate::options::CopyOptions;
use crate::phase::PhaseTracker;
use crate::report::{
    BottleneckSummary, ErrorSummary, ExtraSummary, FolderSummary, Hint, PhaseSummary,
    ReplacementSample, ReplacementSummary, RunInfo, RunReport, top_level,
};
use crate::stats::{AnalysisCollector, StatsTracker};
use crate::transport::TransportProfile;
use crate::verify::{VerificationTarget, verify_written_targets};
use crate::worker::{CompletedCopy, FileCopyJob, FileWorkers, ReplacementWork};

const REPORT_SAMPLE_LIMIT: usize = 100;
/// Consecutive device-gone/disk-full failures (with no success in between)
/// that trip the early-stop breaker. Low enough to stop a disconnected-drive
/// failure storm quickly, high enough that a few isolated bad objects on a
/// healthy device never end the run.
const BREAKER_THRESHOLD: u32 = 5;
/// Journal job signature. Checkpoints self-validate (temp identity + source
/// size/mtime + prefix digest), so the signature pins only the resume
/// protocol version — never user options, whose changes must not wipe
/// another run's checkpoints.
const RESUME_PROTOCOL_SIGNATURE: &str = "resume-protocol-v1";
const SYSTEM_EXCLUSIONS: [&str; 6] = [
    "$RECYCLE.BIN",
    "System Volume Information",
    "pagefile.sys",
    "swapfile.sys",
    "hiberfil.sys",
    "DumpStack.log.tmp",
];

/// Read-only observer implemented by terminal front ends.
pub trait RunObserver: Send + Sync {
    /// Receives immutable state snapshots.
    fn on_snapshot(&self, snapshot: &RunSnapshot);

    /// Receives important human-readable status changes.
    fn on_message(&self, message: &str);

    /// Returns true after the front end requests a graceful stop.
    fn cancellation_requested(&self) -> bool {
        false
    }
}

/// Executes one copy run and returns the exact persisted report model.
pub fn run_copy(
    options: &CopyOptions,
    observer: &dyn RunObserver,
) -> Result<RunReport, BigcpError> {
    observer.on_message("pre-flight: resolving and pinning roots");
    let preflight = preflight(options)?;
    let destination_policy =
        FilesystemPolicy::from_volumes(&preflight.source_volume, &preflight.destination_volume);
    let run_id = Uuid::new_v4().simple().to_string();
    let started_at = OffsetDateTime::now_utc();
    let started = format_time(started_at);

    let fallback_log = preflight
        .state_dir
        .join(format!("run-{run_id}-fallback.log.jsonl"));
    let mut audit = AuditWriter::create(preflight.log_path.clone(), fallback_log)?;
    audit.emit(&AuditEvent::RunStart {
        v: crate::LOG_SCHEMA_VERSION,
        run_id: run_id.clone(),
        source: AuditPath::from_path(&preflight.source),
        destination: AuditPath::from_path(&preflight.destination),
        dry_run: options.dry_run,
        verify: options.verify,
        replace: options.replace,
    })?;
    audit.emit(&AuditEvent::Profile {
        source_class: format!("{:?}", preflight.profile.source.class),
        destination_class: format!("{:?}", preflight.profile.destination.class),
        source_endpoint: preflight.profile.source.endpoint,
        destination_endpoint: preflight.profile.destination.endpoint,
        chunk_bytes: preflight.profile.chunk_bytes,
        workers: preflight.profile.workers,
        same_physical_disk: preflight.profile.same_physical_disk,
        transport: preflight.profile.transport.kind,
        burst_bytes: preflight.profile.transport.burst_bytes,
    })?;
    if preflight.profile.transport.is_same_spindle() {
        observer.on_message(&format!(
            "same-spindle HDD topology: using serialized small-file batches and {} MiB phased read/write bursts",
            preflight.profile.transport.burst_bytes / (1024 * 1024)
        ));
    }
    if preflight.source_volume.endpoint.is_remote()
        || preflight.destination_volume.endpoint.is_remote()
    {
        let message = if preflight.profile.transport.is_wsl() {
            format!(
                "WSL Plan 9 topology: source={} destination={}; using bounded two-buffer overlap, {} MiB requests, and {} static worker(s){}",
                preflight.source_volume.endpoint.name(),
                preflight.destination_volume.endpoint.name(),
                preflight.profile.chunk_bytes / (1024 * 1024),
                preflight.profile.workers,
                if preflight.destination_volume.endpoint == bigcp_win::EndpointKind::Wsl {
                    " with striped destination creates"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "remote topology: source={} destination={}; using bounded two-buffer read/write overlap and {} static redirector worker(s)",
                preflight.source_volume.endpoint.name(),
                preflight.destination_volume.endpoint.name(),
                preflight.profile.workers,
            )
        };
        observer.on_message(&message);
        audit.emit(&AuditEvent::Warning {
            kind: "remote_endpoint".to_owned(),
            rel: None,
            message,
        })?;
    }
    if preflight.destination_write_cache_disabled {
        observer.on_message(
            "destination write caching is off (Quick-removal policy): small-file copies run \
             several times slower; see the report's hints for the measured fix",
        );
        audit.emit(&AuditEvent::Warning {
            kind: "quick_removal_destination".to_owned(),
            rel: None,
            message: "destination device reports write caching disabled (Quick-removal policy); \
                      metadata-heavy workloads measured ~3.4x slower under it"
                .to_owned(),
        })?;
    }
    if destination_policy.is_degraded() {
        let message = if destination_policy.filesystem().is_fat_family() {
            format!(
                "{} destination: timestamps and attributes are projected to a coarse, range-limited representation; named streams, EAs, sparse layout, EFS, ACLs, and reparse points cannot be preserved{}",
                destination_policy.filesystem().name(),
                if destination_policy.maximum_file_size().is_some() {
                    "; files larger than 4 GiB minus 1 byte fail"
                } else {
                    ""
                }
            )
        } else if matches!(
            destination_policy.filesystem(),
            FileSystem::Wsl | FileSystem::Remote
        ) {
            format!(
                "remote semantic projection with {} destination: content and last-write time are preserved; Windows creation/access times and attributes are not claimed, and optional features remain capability-gated",
                destination_policy.filesystem().name()
            )
        } else {
            format!(
                "remote source semantic projection onto {}: content and last-write time are preserved; source creation/access times and Windows attributes are not claimed, while optional features remain capability-gated",
                destination_policy.filesystem().name()
            )
        };
        observer.on_message(&message);
        audit.emit(&AuditEvent::Warning {
            kind: "degraded_filesystem".to_owned(),
            rel: None,
            message,
        })?;
    }

    let journal = if options.dry_run {
        // A dry-run must not disturb resume state: appending a Job record
        // could invalidate another run's checkpoints, silently destroying a
        // multi-hundred-GB resume the user was about to continue. The
        // zero-destination-write promise extends to resume hints.
        None
    } else {
        match Journal::open(preflight.state_dir.join("journal.jsonl"), options.fresh) {
            Ok(mut journal) => {
                if let Err(error) = journal.begin_job(
                    run_id.clone(),
                    path_key(&preflight.source),
                    path_key(&preflight.destination),
                    // Checkpoints self-validate (temp identity + source
                    // size/mtime + prefix digest), so no user option can make
                    // a stale checkpoint unsafe. The signature pins only the
                    // resume protocol itself; hashing the full option summary
                    // here used to wipe checkpoints whenever any flag (even
                    // --verify) changed between runs.
                    RESUME_PROTOCOL_SIGNATURE.to_owned(),
                    started.clone(),
                ) {
                    audit.emit(&AuditEvent::Warning {
                        kind: "checkpointing_disabled".to_owned(),
                        rel: None,
                        message: format!("journal job header could not be written: {error}"),
                    })?;
                    None
                } else {
                    if journal.skipped_records() > 0 {
                        audit.emit(&AuditEvent::Warning {
                            kind: "journal_records_skipped".to_owned(),
                            rel: None,
                            message: format!(
                                "{} interior journal record(s) failed validation and were ignored \
                                 (not truncated); affected resume hints fall back safely",
                                journal.skipped_records()
                            ),
                        })?;
                    }
                    Some(journal)
                }
            }
            Err(error) => {
                audit.emit(&AuditEvent::Warning {
                    kind: "checkpointing_disabled".to_owned(),
                    rel: None,
                    message: format!("journal could not be opened: {error}"),
                })?;
                None
            }
        }
    };

    let root_metadata = metadata_at(&preflight.source)
        .map_err(|error| BigcpError::io("read source root metadata", error))?;
    if root_metadata.kind != ObjectKind::Directory {
        return Err(BigcpError::Invalid(
            "source must resolve to a real directory, not a file or reparse point".to_owned(),
        ));
    }

    let worker_cancel = Arc::new(AtomicBool::new(false));
    let file_workers = FileWorkers::new(
        preflight.profile.workers,
        preflight
            .profile
            .transport
            .is_same_spindle()
            .then_some(preflight.profile.transport.burst_bytes),
    )?;
    let mut runner = Runner {
        options,
        observer,
        run_id: &run_id,
        source_root: &preflight.source,
        destination_root: &preflight.destination,
        source_supports_streams: preflight.source_volume.capabilities.named_streams,
        source_supports_eas: preflight.source_volume.capabilities.extended_attributes,
        destination_policy,
        chunk_bytes: preflight.profile.chunk_bytes,
        transport: preflight.profile.transport,
        source_is_volume_root: same_path(&preflight.source, &preflight.source_volume.root)?,
        counters: Counters::default(),
        audit,
        stats: StatsTracker::new(),
        phases: Arc::new(PhaseTracker::new()),
        errors: BTreeMap::new(),
        warnings: BTreeMap::new(),
        replacements: ReplacementSummary::default(),
        extras: ExtraSummary::default(),
        folders: BTreeMap::new(),
        verification_targets: Vec::new(),
        journal,
        analysis: options.analyze.then(AnalysisCollector::default),
        canceled: false,
        breaker: None,
        breaker_streak: 0,
        breaker_announced: false,
        dir_outstanding: HashMap::new(),
        file_workers,
        file_jobs_outstanding: 0,
        next_independent_shard: 0,
        worker_cancel,
        last_snapshot: Instant::now(),
    };
    if destination_policy.is_degraded() {
        runner.increment_warning("degraded_filesystem");
    }
    runner.publish(RunState::Copying);
    runner.copy_tree(root_metadata, preflight.destination_exists)?;
    if let Some(point) = runner.stats.maybe_roll(Duration::ZERO) {
        runner.audit.emit(&AuditEvent::Stat {
            counters: runner.counters.clone(),
            read_mbps: point.read_mbps,
            write_mbps: point.write_mbps,
        })?;
    }

    let verify = if options.verify && !options.dry_run {
        runner.publish(RunState::Verifying);
        Some(verify_written_targets(
            &runner.verification_targets,
            &mut runner.counters,
            destination_policy,
        ))
    } else {
        None
    };
    let analysis = runner.analysis.take().map(AnalysisCollector::finish);
    if let Some(summary) = &analysis {
        runner.audit.emit(&AuditEvent::Analysis {
            analysis: summary.clone(),
        })?;
        // Engine phase-time breakdown — the debugging aid behind the
        // meet-or-exceed-robocopy gate (§8.7): where each worker-second went.
        runner.audit.emit(&AuditEvent::Warning {
            kind: "phase_timing".to_owned(),
            rel: None,
            message: runner.phases.summary(),
        })?;
        runner.observer.on_message(&runner.phases.summary());
    }

    // An invariant breach must not abort before the report exists: the report
    // and log carry `integrity: failed` so the user is told to trust the log
    // over the summary (PLAN section 7.3), and the run still exits 6.
    let (integrity, invariant_failure) = match runner
        .counters
        .reconcile()
        .and_then(|()| runner.reconcile_folders())
    {
        Ok(()) => ("ok".to_owned(), None),
        Err(message) => {
            runner.audit.emit(&AuditEvent::Warning {
                kind: "counter_invariant".to_owned(),
                rel: None,
                message: message.clone(),
            })?;
            ("failed".to_owned(), Some(message))
        }
    };
    let exit = if invariant_failure.is_some() {
        6
    } else {
        completed_exit(
            runner.canceled,
            runner.breaker.is_some(),
            &runner.counters,
            verify.as_ref(),
        )
    };
    let journal_end_failed = runner.journal.as_mut().is_some_and(|journal| {
        journal
            .append(JournalEvent::End {
                run_id: run_id.clone(),
            })
            .is_err()
    });
    if journal_end_failed {
        runner.increment_warning("checkpointing_disabled");
        runner.audit.emit(&AuditEvent::Warning {
            kind: "checkpointing_disabled".to_owned(),
            rel: None,
            message: "journal run-end marker could not be written".to_owned(),
        })?;
        runner.journal = None;
    } else if let Some(journal) = runner.journal.as_mut()
        && let Err(error) = journal.compact()
    {
        // Compaction is hygiene (bounded replay, section 5.12), never a
        // correctness requirement — a failure is reported and the run stays
        // successful.
        runner.increment_warning("journal_compaction_failed");
        runner.audit.emit(&AuditEvent::Warning {
            kind: "journal_compaction_failed".to_owned(),
            rel: None,
            message: format!("journal could not be compacted after run end: {error}"),
        })?;
    }
    let audit_status = if runner.audit.degraded() {
        "degraded"
    } else {
        "ok"
    };

    let ended_at = OffsetDateTime::now_utc();
    let duration = runner.stats.elapsed().as_secs_f64();
    let average = if duration > 0.0 {
        runner.counters.bytes_logical_copied as f64 / duration
    } else {
        0.0
    };
    let peak = runner.stats.best_write_bytes_per_second();
    let final_hypothesis = runner
        .stats
        .timeline()
        .last()
        .map_or("balanced", |point| point.hypothesis.as_str())
        .to_owned();
    let bottleneck = BottleneckSummary {
        hypothesis: final_hypothesis,
        confidence: "low".to_owned(),
        evidence: "application-side read/write completion rates; not physical device utilization"
            .to_owned(),
        observed_peak_mbps: peak / 1_000_000.0,
        average_mbps: average / 1_000_000.0,
        efficiency_vs_observed_peak: if peak > 0.0 { average / peak } else { 0.0 },
        provenance: "best sustained window observed during this run; no probe I/O".to_owned(),
    };
    let hints = derive_hints(&runner, &preflight);
    let phases = summarize_phases(runner.stats.timeline());
    runner.observer.on_message("copy run complete");
    runner.publish(RunState::Complete);
    let mut report = RunReport {
        v: REPORT_SCHEMA_VERSION,
        run: RunInfo {
            id: run_id.clone(),
            started,
            ended: format_time(ended_at),
            duration_seconds: duration,
            exit,
            dry_run: options.dry_run,
            durability: if options.flush {
                "durable".to_owned()
            } else {
                "logical".to_owned()
            },
            audit: audit_status.to_owned(),
            source: display_path(&preflight.source)
                .to_string_lossy()
                .into_owned(),
            destination: display_path(&preflight.destination)
                .to_string_lossy()
                .into_owned(),
            source_filesystem: preflight.source_volume.filesystem.name().to_owned(),
            destination_filesystem: preflight.destination_volume.filesystem.name().to_owned(),
            log_path: runner.audit.path().to_path_buf(),
            report_path: preflight.report_path.clone(),
        },
        config: option_summary(options),
        devices: preflight.profile.clone(),
        counters: runner.counters.clone(),
        replacements: runner.replacements,
        errors: runner.errors.into_values().collect(),
        warnings: runner.warnings,
        extras: runner.extras,
        folders: runner.folders,
        timeline: runner.stats.timeline().to_vec(),
        phases,
        bottleneck,
        hints,
        analysis,
        verify,
        integrity,
    };

    let report_path = preflight.report_path;
    if let Err(primary_error) = report.write_atomic(&report_path) {
        let fallback = preflight
            .state_dir
            .join(format!("run-{}-fallback.report.json", report.run.id));
        "degraded".clone_into(&mut report.run.audit);
        report.run.report_path.clone_from(&fallback);
        let warning = report
            .warnings
            .entry("report_fallback".to_owned())
            .or_insert(0);
        *warning = warning.saturating_add(1);
        let fallback_message = format!(
            "primary report {} failed ({primary_error}); using {}",
            report_path.display(),
            fallback.display()
        );
        runner.observer.on_message(&fallback_message);
        runner.audit.emit(&AuditEvent::Warning {
            kind: "report_fallback".to_owned(),
            rel: None,
            message: fallback_message,
        })?;
        report.run.log_path = runner.audit.path().to_path_buf();
        runner.audit.flush()?;
        report.write_atomic(&fallback).map_err(|fallback_error| {
            BigcpError::Audit(format!(
                "report failed at {} ({primary_error}) and fallback {} failed ({fallback_error})",
                report_path.display(),
                fallback.display()
            ))
        })?;
    }
    runner.audit.emit(&AuditEvent::RunEnd {
        counters: report.counters.clone(),
        durability: report.run.durability.clone(),
        audit: report.run.audit.clone(),
        integrity: report.integrity.clone(),
        exit: report.run.exit,
    })?;
    runner.audit.finish()?;
    if let Some(message) = invariant_failure {
        return Err(BigcpError::Invariant(message));
    }
    Ok(report)
}

struct Runner<'a> {
    options: &'a CopyOptions,
    observer: &'a dyn RunObserver,
    run_id: &'a str,
    source_root: &'a Path,
    destination_root: &'a Path,
    source_supports_streams: bool,
    source_supports_eas: bool,
    destination_policy: FilesystemPolicy,
    chunk_bytes: usize,
    transport: TransportProfile,
    source_is_volume_root: bool,
    counters: Counters,
    audit: AuditWriter,
    stats: StatsTracker,
    phases: Arc<PhaseTracker>,
    errors: BTreeMap<ErrorCategory, ErrorSummary>,
    warnings: BTreeMap<String, u64>,
    replacements: ReplacementSummary,
    extras: ExtraSummary,
    folders: BTreeMap<String, FolderSummary>,
    verification_targets: Vec<VerificationTarget>,
    journal: Option<Journal>,
    analysis: Option<AnalysisCollector>,
    canceled: bool,
    /// Tripped circuit breaker: repeated device-gone or disk-full failures
    /// stop the run early and resumably instead of grinding through every
    /// remaining object (exit 4, PLAN §5.5).
    breaker: Option<ErrorCategory>,
    breaker_streak: u32,
    breaker_announced: bool,
    /// Outstanding worker jobs per parent directory, so a directory's exit
    /// waits only for its own files while sibling directories keep copying.
    dir_outstanding: HashMap<PathBuf, usize>,
    file_workers: FileWorkers,
    file_jobs_outstanding: usize,
    /// Round-robin shard for independently streamed redirector files and WSL
    /// destination creates. Other small creates use directory affinity.
    next_independent_shard: usize,
    /// Coordinator-owned cancellation state shared with streamed file jobs.
    worker_cancel: Arc<AtomicBool>,
    last_snapshot: Instant,
}

enum DirectoryTask {
    Enter {
        source: PathBuf,
        destination: PathBuf,
        relative: PathBuf,
        source_metadata: ObjectMetadata,
        destination_exists: bool,
        destination_metadata: Option<ObjectMetadata>,
    },
    Exit {
        source: PathBuf,
        destination: PathBuf,
        relative: PathBuf,
        source_metadata: ObjectMetadata,
        destination_exists: bool,
        destination_metadata: Option<ObjectMetadata>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerDispatch {
    promote_threshold: Option<u64>,
    directory_affine: bool,
}

/// Selects only files whose correctness does not require coordinator-owned
/// journal access. Local behavior remains small-file-only; redirectors may
/// additionally stream independent files through the bounded worker pool.
fn worker_dispatch(
    transport: TransportProfile,
    destination_is_wsl: bool,
    journal_available: bool,
    unnamed_size: u64,
    large_threshold: u64,
    checkpoint_threshold: u64,
    preserve_sparse: bool,
) -> Option<WorkerDispatch> {
    let checkpoint_eligible = journal_available && unnamed_size >= checkpoint_threshold;
    let redirector_stream =
        transport.is_redirector() && (!journal_available || unnamed_size < checkpoint_threshold);
    if preserve_sparse
        || checkpoint_eligible
        || (unnamed_size >= large_threshold && !redirector_stream)
    {
        return None;
    }
    let promote_threshold = if transport.is_redirector() {
        journal_available.then_some(checkpoint_threshold)
    } else if journal_available {
        Some(large_threshold.min(checkpoint_threshold))
    } else {
        Some(large_threshold)
    };
    Some(WorkerDispatch {
        promote_threshold,
        // NTFS and generic redirectors retain the measured same-directory
        // create serialization. WSL's Plan 9 destination instead stripes
        // small creates: serial affinity there merely exposes one provider
        // round trip at a time and protects no local NTFS directory index.
        directory_affine: unnamed_size < large_threshold && !destination_is_wsl,
    })
}

impl Runner<'_> {
    fn copy_tree(
        &mut self,
        root_metadata: ObjectMetadata,
        destination_exists: bool,
    ) -> Result<(), BigcpError> {
        let destination_metadata = destination_exists
            .then(|| metadata_at(self.destination_root))
            .transpose()
            .map_err(|error| BigcpError::io("read destination root metadata", error))?;
        let mut tasks = std::collections::VecDeque::new();
        tasks.push_back(DirectoryTask::Enter {
            source: self.source_root.to_path_buf(),
            destination: self.destination_root.to_path_buf(),
            relative: PathBuf::new(),
            source_metadata: root_metadata,
            destination_exists,
            destination_metadata,
        });
        while let Some(task) = tasks.pop_back() {
            self.observe_cancellation()?;
            if let Some(category) = self.breaker
                && !self.breaker_announced
            {
                self.breaker_announced = true;
                self.observer.on_message(
                    "stopping early: repeated device or disk-space failures tripped the breaker; \
                     resolve the cause and re-run to resume",
                );
                self.publish(RunState::Canceling);
                self.audit.emit(&AuditEvent::Warning {
                    kind: "breaker".to_owned(),
                    rel: None,
                    message: format!(
                        "{BREAKER_THRESHOLD} consecutive {category:?}-class failures; \
                         no further objects will be attempted (exit 4, rerun resumes)"
                    ),
                })?;
            }
            match task {
                DirectoryTask::Enter {
                    source,
                    destination,
                    relative,
                    source_metadata,
                    destination_exists,
                    destination_metadata,
                } => {
                    if !self.canceled && self.breaker.is_none() {
                        self.enter_directory(
                            source,
                            destination,
                            relative,
                            source_metadata,
                            destination_exists,
                            destination_metadata,
                            &mut tasks,
                        )?;
                    } else {
                        self.counters.dirs_discovered =
                            self.counters.dirs_discovered.saturating_add(1);
                        self.counters.dirs_failed = self.counters.dirs_failed.saturating_add(1);
                        // A clean stop is not an error condition: surface the
                        // untraversed subtree as a warning so exit-3/exit-4
                        // runs keep meaningful error summaries (the breaker's
                        // cause is already recorded by the tripping failures).
                        let (kind, message) = if self.breaker.is_some() {
                            (
                                "breaker_subtree",
                                "directory was discovered but not traversed after the breaker tripped",
                            )
                        } else {
                            (
                                "canceled_subtree",
                                "directory was discovered but not traversed after cancellation",
                            )
                        };
                        self.increment_warning(kind);
                        self.audit.emit(&AuditEvent::Warning {
                            kind: kind.to_owned(),
                            rel: Some(AuditPath::from_path(&relative)),
                            message: message.to_owned(),
                        })?;
                    }
                }
                DirectoryTask::Exit {
                    source,
                    destination,
                    relative,
                    source_metadata,
                    destination_exists,
                    destination_metadata,
                } => {
                    // A still-busy directory yields: drain one completion
                    // (guaranteed progress) and revisit this exit after the
                    // remaining tasks, so sibling directories keep feeding
                    // their affine workers instead of serializing dir-by-dir.
                    if self.dir_outstanding.contains_key(&relative) && !tasks.is_empty() {
                        self.receive_file_job()?;
                        tasks.push_front(DirectoryTask::Exit {
                            source,
                            destination,
                            relative,
                            source_metadata,
                            destination_exists,
                            destination_metadata,
                        });
                    } else {
                        self.exit_directory(
                            &source,
                            &destination,
                            &relative,
                            &source_metadata,
                            destination_exists,
                            destination_metadata.as_ref(),
                        )?;
                    }
                }
            }
        }
        // Safety net: every directory exit drained its own jobs; this
        // catches anything a stopped (canceled/breaker) walk left in flight.
        self.drain_file_workers()?;
        Ok(())
    }

    fn observe_cancellation(&mut self) -> Result<(), BigcpError> {
        let requested = self.observer.cancellation_requested();
        if requested {
            self.worker_cancel.store(true, Ordering::Release);
        }
        if !self.canceled && requested {
            self.canceled = true;
            self.observer
                .on_message("cancellation requested: no new directories will be dispatched");
            self.publish(RunState::Canceling);
            self.audit.emit(&AuditEvent::Warning {
                kind: "canceled".to_owned(),
                rel: None,
                message: "user requested a graceful stop".to_owned(),
            })?;
        }
        Ok(())
    }

    fn enter_directory(
        &mut self,
        source: PathBuf,
        destination: PathBuf,
        relative: PathBuf,
        source_metadata: ObjectMetadata,
        destination_exists: bool,
        destination_metadata: Option<ObjectMetadata>,
        tasks: &mut std::collections::VecDeque<DirectoryTask>,
    ) -> Result<(), BigcpError> {
        self.counters.dirs_discovered = self.counters.dirs_discovered.saturating_add(1);
        let phase_timer = Instant::now();
        let source_entries = match enumerate_directory(&source) {
            Ok(entries) => entries,
            Err(error) => {
                self.counters.dirs_failed = self.counters.dirs_failed.saturating_add(1);
                self.record_error(OperationError::from_io(
                    "enumerate_src",
                    relative.clone(),
                    &error,
                ))?;
                return Ok(());
            }
        };
        let mut destination_map = if destination_exists {
            match destination_join(&destination, self.destination_policy) {
                Ok(map) => map,
                Err(error) => {
                    self.counters.dirs_failed = self.counters.dirs_failed.saturating_add(1);
                    self.record_error(OperationError::from_io(
                        "enumerate_dst",
                        relative.clone(),
                        &error,
                    ))?;
                    self.account_children_not_attempted(&source_entries, &relative)?;
                    return Ok(());
                }
            }
        } else {
            HashMap::new()
        };
        self.phases.record(6, phase_timer.elapsed());
        let mut seen_source = HashSet::new();
        let mut child_directories = Vec::new();

        for source_entry in source_entries {
            let phase_timer = Instant::now();
            let child_relative = relative.join(&source_entry.name);
            if source_entry.metadata.kind == ObjectKind::File {
                // Discovery is counted here, once per seen entry, so the I6
                // reconciliation can detect any later dropped outcome.
                self.counters
                    .note_file_discovered(source_entry.metadata.size);
            }
            if self.breaker.is_some() {
                // A tripped breaker stops mid-directory too: a failure storm
                // inside one huge directory must not keep dispatching doomed
                // work until the directory is exhausted.
                match source_entry.metadata.kind {
                    ObjectKind::File => {
                        self.record_file(
                            &child_relative,
                            FileOutcome::NotAttempted {
                                bytes: source_entry.metadata.size,
                                reason: "breaker".to_owned(),
                            },
                            None,
                        )?;
                    }
                    ObjectKind::Directory => {
                        self.counters.dirs_discovered =
                            self.counters.dirs_discovered.saturating_add(1);
                        self.counters.dirs_failed = self.counters.dirs_failed.saturating_add(1);
                        self.increment_warning("breaker_subtree");
                        self.audit.emit(&AuditEvent::Warning {
                            kind: "breaker_subtree".to_owned(),
                            rel: Some(AuditPath::from_path(&child_relative)),
                            message: "directory was discovered but not traversed after the breaker tripped".to_owned(),
                        })?;
                    }
                    ObjectKind::Reparse => {
                        self.counters.links_discovered =
                            self.counters.links_discovered.saturating_add(1);
                        self.counters.links_not_attempted =
                            self.counters.links_not_attempted.saturating_add(1);
                        self.record_link_event(
                            &child_relative,
                            "not_attempted_link",
                            Some("breaker".to_owned()),
                            None,
                            None,
                        )?;
                    }
                }
                continue;
            }
            let key = match comparison_key(
                &source_entry.name,
                self.destination_policy.names_are_case_sensitive(),
            ) {
                Ok(key) => key,
                Err(error) => {
                    self.record_entry_failure(
                        &source_entry,
                        child_relative,
                        OperationError::from_io("casefold", PathBuf::new(), &error),
                    )?;
                    continue;
                }
            };
            if !seen_source.insert(key.clone()) {
                self.record_entry_failure(
                    &source_entry,
                    child_relative,
                    OperationError::semantic(
                        ErrorCategory::TypeConflict,
                        "join",
                        PathBuf::new(),
                        "source directory contains names that collide under the destination's name-matching semantics",
                    ),
                )?;
                continue;
            }
            let destination_entry = destination_map.remove(&key);
            if self.is_system_candidate(&source_entry, &relative) && self.options.include_system {
                self.increment_warning("system_candidate_included");
                self.audit.emit(&AuditEvent::Warning {
                    kind: "system_candidate_included".to_owned(),
                    rel: Some(AuditPath::from_path(&child_relative)),
                    message: "root-level operating-system artifact included by --include-system"
                        .to_owned(),
                })?;
            }
            if let Some(reason) = self.exclusion_reason(&source_entry, &relative) {
                self.record_exclusion(&source_entry, child_relative, reason)?;
                continue;
            }
            match source_entry.metadata.kind {
                ObjectKind::File => {
                    self.handle_file(source_entry, destination_entry, child_relative)?;
                }
                ObjectKind::Directory => {
                    child_directories.push((source_entry, destination_entry, child_relative));
                }
                ObjectKind::Reparse => {
                    self.handle_reparse(source_entry, destination_entry, child_relative)?;
                }
            }
            // Includes any completions drained under backpressure — the
            // coordinator's whole per-entry serial cost is the number to beat.
            self.phases.record(7, phase_timer.elapsed());
        }
        // No drain here: keeping worker jobs in flight across sibling
        // directories is where cross-directory pipelining comes from (the
        // per-directory barrier measurably serialized small-file runs). The
        // drain happens at directory exit, before timestamps are stamped.
        for extra in destination_map.into_values() {
            let rel = relative.join(extra.name);
            self.counters.extra = self.counters.extra.saturating_add(1);
            self.extras.count = self.extras.count.saturating_add(1);
            if self.extras.samples.len() < REPORT_SAMPLE_LIMIT {
                self.extras.samples.push(rel.to_string_lossy().into_owned());
            }
            self.audit.emit(&AuditEvent::Extra {
                rel: AuditPath::from_path(&rel),
            })?;
        }

        tasks.push_back(DirectoryTask::Exit {
            source: source.clone(),
            destination: destination.clone(),
            relative: relative.clone(),
            source_metadata,
            destination_exists: destination_exists || self.options.dry_run,
            destination_metadata,
        });
        for (source_entry, destination_entry, child_relative) in child_directories.into_iter().rev()
        {
            let child_destination = destination.join(&source_entry.name);
            let (child_exists, child_destination_metadata) = match destination_entry {
                None => {
                    if !self.options.dry_run {
                        if let Err(error) = create_directory(&child_destination) {
                            self.account_failed_subtree(
                                &source_entry.path,
                                &child_relative,
                                OperationError::from_io(
                                    "create_dir",
                                    child_relative.clone(),
                                    &error,
                                ),
                            )?;
                            continue;
                        }
                        self.audit.emit(&AuditEvent::Directory {
                            action: "created".to_owned(),
                            rel: AuditPath::from_path(&child_relative),
                        })?;
                        match metadata_at(&child_destination) {
                            Ok(metadata) if metadata.kind == ObjectKind::Directory => {
                                (true, Some(metadata))
                            }
                            Ok(_) => {
                                self.account_failed_subtree(
                                    &source_entry.path,
                                    &child_relative,
                                    OperationError::semantic(
                                        ErrorCategory::DestinationChanged,
                                        "revalidate_dst_dir",
                                        child_relative.clone(),
                                        "new destination directory changed type after creation",
                                    ),
                                )?;
                                continue;
                            }
                            Err(error) => {
                                self.account_failed_subtree(
                                    &source_entry.path,
                                    &child_relative,
                                    destination_access_error(
                                        "revalidate_dst_dir",
                                        &child_relative,
                                        &error,
                                    ),
                                )?;
                                continue;
                            }
                        }
                    } else {
                        (false, None)
                    }
                }
                Some(destination_entry)
                    if destination_entry.metadata.kind == ObjectKind::Directory =>
                {
                    (true, Some(destination_entry.metadata))
                }
                Some(_) => {
                    self.account_failed_subtree(
                        &source_entry.path,
                        &child_relative,
                        OperationError::semantic(
                            ErrorCategory::TypeConflict,
                            "join",
                            child_relative.clone(),
                            "destination object is not a real directory",
                        ),
                    )?;
                    continue;
                }
            };
            tasks.push_back(DirectoryTask::Enter {
                source: source_entry.path,
                destination: child_destination,
                relative: child_relative,
                source_metadata: source_entry.metadata,
                destination_exists: child_exists,
                destination_metadata: child_destination_metadata,
            });
        }
        Ok(())
    }

    fn exit_directory(
        &mut self,
        source: &Path,
        destination: &Path,
        relative: &Path,
        source_metadata: &ObjectMetadata,
        destination_exists: bool,
        destination_metadata: Option<&ObjectMetadata>,
    ) -> Result<(), BigcpError> {
        // A directory's timestamps may only be stamped after its own files
        // stop mutating it; waiting only for *this* directory's jobs keeps
        // sibling directories copying in parallel (a global drain here
        // measurably serialized the whole run directory-by-directory).
        while self.dir_outstanding.contains_key(relative) {
            self.receive_file_job()?;
        }
        if self.options.dry_run || !destination_exists {
            if self.options.dry_run {
                self.counters.dirs_planned = self.counters.dirs_planned.saturating_add(1);
            } else {
                self.counters.dir_done = self.counters.dir_done.saturating_add(1);
            }
            return Ok(());
        }
        let Some(expected_destination) = destination_metadata else {
            self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
            self.record_error(OperationError::semantic(
                ErrorCategory::Internal,
                "revalidate_dst_dir",
                relative.to_path_buf(),
                "destination directory has no captured identity",
            ))?;
            return Ok(());
        };
        match metadata_at(destination) {
            Ok(observed)
                if observed.kind == ObjectKind::Directory
                    && observed.identity == expected_destination.identity => {}
            Ok(_) => {
                self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
                self.record_error(OperationError::semantic(
                    ErrorCategory::DestinationChanged,
                    "revalidate_dst_dir",
                    relative.to_path_buf(),
                    "destination directory identity or type changed during the run",
                ))?;
                return Ok(());
            }
            Err(error) => {
                self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
                self.record_error(destination_access_error(
                    "revalidate_dst_dir",
                    relative,
                    &error,
                ))?;
                return Ok(());
            }
        }
        // Directory last-write updates can be committed lazily by NTFS after
        // a fixture or other prior child/ADS creation handle closes. Re-read
        // at post-order finalization for the stable value, while requiring the
        // enumerated directory identity and type to remain unchanged. The
        // exclusive-source precondition remains responsible for tree-shape
        // stability; the refreshed snapshot is checked again after aux I/O.
        let current_source_metadata = match metadata_at(source) {
            Ok(metadata)
                if metadata.kind == ObjectKind::Directory
                    && metadata.identity == source_metadata.identity =>
            {
                metadata
            }
            Ok(_) => {
                self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
                self.record_error(OperationError::semantic(
                    ErrorCategory::SourceChanged,
                    "revalidate_dir",
                    relative.to_path_buf(),
                    "source directory identity or type changed during the run",
                ))?;
                return Ok(());
            }
            Err(error) => {
                self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
                self.record_error(source_access_error("revalidate_dir", relative, &error))?;
                return Ok(());
            }
        };
        let stream_result = match self.copy_directory_streams(
            source,
            destination,
            relative,
            current_source_metadata.identity,
            expected_destination.identity,
        ) {
            Ok(0) => Ok(()),
            Ok(dropped) => {
                // Directory ADS drops get the same per-item log visibility as
                // file-level drops — never a silent counter (section 4.2, I7).
                self.audit.emit(&AuditEvent::Warning {
                    kind: "streams_dropped".to_owned(),
                    rel: Some(AuditPath::from_path(relative)),
                    message: format!(
                        "{dropped} directory named stream(s) dropped: destination volume lacks named-stream capability"
                    ),
                })?;
                Ok(())
            }
            Err(error) => Err(error),
        };
        let auxiliary_result = if !self.destination_policy.supports_eas() {
            if source_metadata.ea_size > 0 {
                self.increment_warning("ea_dropped");
                self.audit.emit(&AuditEvent::Warning {
                    kind: "ea_dropped".to_owned(),
                    rel: Some(AuditPath::from_path(relative)),
                    message: "destination volume does not advertise directory EA support"
                        .to_owned(),
                })?;
            }
            stream_result
        } else {
            stream_result.and_then(|()| {
                if source_metadata.ea_size > 0
                    || destination_metadata.is_some_and(|metadata| metadata.ea_size > 0)
                    || relative.as_os_str().is_empty()
                {
                    let source_eas = if self.source_supports_eas {
                        read_extended_attributes_checked(source, current_source_metadata.identity)
                            .map_err(|error| source_access_error("read_dir_ea", relative, &error))
                    } else {
                        Ok(bigcp_win::ExtendedAttributes::empty())
                    };
                    source_eas.and_then(|source_eas| {
                        let destination_eas = read_extended_attributes_checked(
                            destination,
                            expected_destination.identity,
                        )
                        .map_err(|error| {
                            destination_access_error("read_dir_ea", relative, &error)
                        })?;
                        if source_eas == destination_eas {
                            Ok(())
                        } else {
                            if !destination_eas.is_empty() {
                                clear_extended_attributes_checked(
                                    destination,
                                    expected_destination.identity,
                                )
                                .map_err(|error| {
                                    destination_access_error("clear_dir_ea", relative, &error)
                                })?;
                            }
                            if source_eas.is_empty() {
                                Ok(())
                            } else {
                                write_extended_attributes_checked(
                                    destination,
                                    expected_destination.identity,
                                    &source_eas,
                                )
                                .map_err(|error| {
                                    destination_access_error("write_dir_ea", relative, &error)
                                })
                            }
                        }
                    })
                } else {
                    Ok(())
                }
            })
        };
        if let Err(error) = auxiliary_result {
            self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
            self.record_error(error)?;
            return Ok(());
        }
        match metadata_at(source) {
            Ok(observed) if same_directory_source(&observed, &current_source_metadata) => {}
            Ok(_) => {
                self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
                self.record_error(OperationError::semantic(
                    ErrorCategory::SourceChanged,
                    "revalidate_dir",
                    relative.to_path_buf(),
                    "source directory changed during metadata finalization",
                ))?;
                return Ok(());
            }
            Err(error) => {
                self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
                self.record_error(source_access_error("revalidate_dir", relative, &error))?;
                return Ok(());
            }
        }
        if let Err(error) = set_basic_at_checked(
            destination,
            expected_destination.identity,
            self.destination_policy
                .project_basic(current_source_metadata.basic),
        ) {
            self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
            self.record_error(destination_access_error("set_dir_meta", relative, &error))?;
            return Ok(());
        }
        self.counters.dir_done = self.counters.dir_done.saturating_add(1);
        self.audit.emit(&AuditEvent::Directory {
            action: "stamped".to_owned(),
            rel: AuditPath::from_path(relative),
        })?;
        Ok(())
    }

    fn copy_directory_streams(
        &mut self,
        source: &Path,
        destination: &Path,
        relative: &Path,
        expected_source: bigcp_win::FileIdentity,
        expected_destination: bigcp_win::FileIdentity,
    ) -> Result<u64, OperationError> {
        if !self.source_supports_streams {
            return Ok(0);
        }
        let streams = list_streams(source)
            .map_err(|error| source_access_error("list_dir_streams", relative, &error))?;
        let named: Vec<_> = streams
            .iter()
            .filter(|stream| !stream.is_unnamed())
            .collect();
        if !self.destination_policy.supports_streams() {
            let dropped = named.len() as u64;
            for _ in 0..dropped {
                self.increment_warning("streams_dropped");
            }
            return Ok(dropped);
        }
        let mut buffer = vec![0_u8; self.chunk_bytes.clamp(64 * 1024, 8 * 1024 * 1024)];
        for stream in named {
            let mut input = SourceStream::open_reparse(source, stream)
                .map_err(|error| source_access_error("open_dir_stream", relative, &error))?;
            if input
                .identity()
                .map_err(|error| source_access_error("identify_dir_stream", relative, &error))?
                != expected_source
            {
                return Err(OperationError::semantic(
                    ErrorCategory::SourceChanged,
                    "identify_dir_stream",
                    relative.to_path_buf(),
                    "directory stream belongs to a different source object",
                ));
            }
            let mut output =
                DestinationStream::create_checked(destination, expected_destination, stream, true)
                    .map_err(|error| {
                        destination_access_error("create_dir_stream", relative, &error)
                    })?;
            if output.identity().map_err(|error| {
                destination_access_error("identify_dir_stream", relative, &error)
            })? != expected_destination
            {
                return Err(OperationError::semantic(
                    ErrorCategory::DestinationChanged,
                    "identify_dir_stream",
                    relative.to_path_buf(),
                    "directory stream belongs to a different destination object",
                ));
            }
            let mut copied = 0_u64;
            loop {
                let count = input
                    .read(&mut buffer)
                    .map_err(|error| source_access_error("read_dir_stream", relative, &error))?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count]).map_err(|error| {
                    destination_access_error("write_dir_stream", relative, &error)
                })?;
                copied = copied.saturating_add(count as u64);
                self.counters.bytes_read_source =
                    self.counters.bytes_read_source.saturating_add(count as u64);
                self.counters.bytes_written_destination = self
                    .counters
                    .bytes_written_destination
                    .saturating_add(count as u64);
            }
            if copied != stream.size {
                return Err(OperationError::semantic(
                    ErrorCategory::SourceChanged,
                    "read_dir_stream",
                    relative.to_path_buf(),
                    format!(
                        "directory stream {} changed size during copy",
                        stream.name.to_string_lossy()
                    ),
                ));
            }
            output
                .flush()
                .map_err(|error| destination_access_error("flush_dir_stream", relative, &error))?;
        }
        Ok(0)
    }

    fn handle_file(
        &mut self,
        source: DirectoryEntry,
        destination: Option<DirectoryEntry>,
        relative: PathBuf,
    ) -> Result<(), BigcpError> {
        if self
            .destination_policy
            .maximum_file_size()
            .is_some_and(|limit| source.metadata.size > limit)
        {
            return self.record_file(
                &relative,
                FileOutcome::Failed {
                    bytes: source.metadata.size,
                    error: OperationError::semantic(
                        ErrorCategory::FsLimit,
                        "fat_file_size",
                        relative.clone(),
                        format!(
                            "{} cannot store a {}-byte file; its maximum is {} bytes",
                            self.destination_policy.filesystem().name(),
                            source.metadata.size,
                            u32::MAX
                        ),
                    ),
                },
                None,
            );
        }
        if is_compressed(source.metadata.basic.attributes) {
            self.increment_warning("compressed_sources");
        }
        if is_sparse(source.metadata.basic.attributes) && !self.destination_policy.supports_sparse()
        {
            self.increment_warning("sparse_expanded");
            self.audit.emit(&AuditEvent::Warning {
                kind: "sparse_expanded".to_owned(),
                rel: Some(AuditPath::from_path(&relative)),
                message: format!(
                    "{} does not support sparse allocation; logical content will be written densely",
                    self.destination_policy.filesystem().name()
                ),
            })?;
        }
        if is_cloud_placeholder(source.metadata.basic.attributes) {
            // `--skip-cloud` placeholders never reach this function:
            // `exclusion_reason` already excluded them during enumeration.
            self.increment_warning("cloud_hydrated");
            self.audit.emit(&AuditEvent::Warning {
                kind: "cloud_hydrated".to_owned(),
                rel: Some(AuditPath::from_path(&relative)),
                message: "cloud placeholder included; reading may hydrate remote content"
                    .to_owned(),
            })?;
        }

        let source_snapshot = EntrySnapshot {
            relative_path: relative.clone(),
            metadata: source.metadata.clone(),
        };
        let destination_snapshot = destination.as_ref().map(|entry| EntrySnapshot {
            relative_path: relative.clone(),
            metadata: entry.metadata.clone(),
        });
        match classify(
            &source_snapshot,
            destination_snapshot.as_ref(),
            self.options.replace,
            self.destination_policy,
        ) {
            Classification::New => {
                self.copy_classified(&source, &relative, &source_snapshot, None, None, true)?;
            }
            Classification::Same => {
                self.record_file(
                    &relative,
                    FileOutcome::SkippedSame {
                        bytes: source.metadata.size,
                    },
                    None,
                )?;
            }
            Classification::MetadataDiff(fields) => {
                let outcome = if self.options.dry_run {
                    self.counters.would_meta_fix = self.counters.would_meta_fix.saturating_add(1);
                    FileOutcome::NotAttempted {
                        bytes: source.metadata.size,
                        reason: "dry_run_would_fix_metadata".to_owned(),
                    }
                } else if let Some(destination) = destination {
                    match repair_metadata(
                        &source,
                        &destination,
                        &fields,
                        &relative,
                        self.destination_policy,
                        self.source_supports_eas,
                    ) {
                        Ok(()) => FileOutcome::MetadataFixed {
                            bytes: source.metadata.size,
                        },
                        Err(error) => FileOutcome::Failed {
                            bytes: source.metadata.size,
                            error,
                        },
                    }
                } else {
                    FileOutcome::Failed {
                        bytes: source.metadata.size,
                        error: OperationError::semantic(
                            ErrorCategory::Internal,
                            "set_meta",
                            relative.clone(),
                            "classifier requested metadata repair without a destination",
                        ),
                    }
                };
                self.record_file(&relative, outcome, Some(fields))?;
            }
            Classification::Replace {
                fields,
                destination_newer,
            } => {
                self.copy_classified(
                    &source,
                    &relative,
                    &source_snapshot,
                    destination_snapshot.as_ref(),
                    Some((fields, destination_newer)),
                    true,
                )?;
            }
            Classification::SkipDifferent {
                fields,
                destination_newer,
            } => {
                let old = destination_snapshot
                    .as_ref()
                    .map(|snapshot| &snapshot.metadata);
                self.record_file(
                    &relative,
                    FileOutcome::SkippedDifferent {
                        bytes: source.metadata.size,
                        destination_newer,
                        old_size: old.map_or(0, |metadata| metadata.size),
                        old_mtime: old.map_or(0, |metadata| metadata.basic.last_write_time),
                        old_attributes: old.map_or(0, |metadata| metadata.basic.attributes),
                    },
                    Some(fields),
                )?;
            }
            Classification::TypeConflict => {
                self.record_file(
                    &relative,
                    FileOutcome::Failed {
                        bytes: source.metadata.size,
                        error: OperationError::semantic(
                            ErrorCategory::TypeConflict,
                            "join",
                            relative.clone(),
                            "source and destination object types conflict",
                        ),
                    },
                    None,
                )?;
            }
        }
        Ok(())
    }

    fn copy_classified(
        &mut self,
        source: &DirectoryEntry,
        relative: &Path,
        source_snapshot: &EntrySnapshot,
        destination_snapshot: Option<&EntrySnapshot>,
        replacement: Option<(Vec<&'static str>, bool)>,
        dispatch: bool,
    ) -> Result<(), BigcpError> {
        let replacement_old = match (&replacement, destination_snapshot) {
            (Some(_), Some(snapshot)) => Some(&snapshot.metadata),
            (Some(_), None) => {
                return Err(BigcpError::Invariant(
                    "replacement classification did not carry its destination snapshot".to_owned(),
                ));
            }
            (None, _) => None,
        };
        // Fast dispatch — the small-file benchmark showed the coordinator's
        // per-file source revalidation and stream probe serializing the
        // whole pipeline (rsync's architecture teaches the same lesson: the
        // stage that discovers work must never do per-item blocking work).
        // Routing keys on the enumerated unnamed size alone; the engine
        // revalidates at open (section 4.8) and discovers streams itself,
        // and a worker that meets `promote_threshold` hands the file back
        // untouched so coordinator-owned checkpoint persistence stays inline.
        // Redirector-streamed jobs share the coordinator's cancellation bit.
        if dispatch && !self.options.dry_run {
            let unnamed = source.metadata.size;
            let preserve_sparse = self.destination_policy.supports_sparse()
                && !self.options.no_sparse
                && is_sparse(source.metadata.basic.attributes);
            if let Some(dispatch) = worker_dispatch(
                self.transport,
                self.destination_policy.endpoint() == bigcp_win::EndpointKind::Wsl,
                self.journal.is_some(),
                unnamed,
                self.options.large_threshold(),
                self.options.checkpoint_threshold(),
                preserve_sparse,
            ) {
                let job = FileCopyJob {
                    source_path: source.path.clone(),
                    destination_path: self.destination_root.join(relative),
                    source_snapshot: source_snapshot.clone(),
                    destination_snapshot: destination_snapshot.cloned(),
                    replacement: replacement.map(|(fields, destination_newer)| ReplacementWork {
                        fields,
                        destination_newer,
                    }),
                    run_id: self.run_id.to_owned(),
                    large_threshold: self.options.large_threshold(),
                    verify: self.options.verify,
                    flush: self.options.flush,
                    source_supports_streams: self.source_supports_streams,
                    source_supports_eas: self.source_supports_eas,
                    destination_supports_streams: self.destination_policy.supports_streams(),
                    destination_supports_eas: self.destination_policy.supports_eas(),
                    destination_supports_encryption: self.destination_policy.supports_encryption(),
                    destination_supports_preallocation: self
                        .destination_policy
                        .supports_preallocation(),
                    destination_supports_persistent_acls: self
                        .destination_policy
                        .supports_persistent_acls(),
                    destination_supports_posix_unlink_rename: self
                        .destination_policy
                        .supports_posix_unlink_rename(),
                    destination_metadata: self
                        .destination_policy
                        .project_basic(source.metadata.basic),
                    destination_requires_post_write_stamp: self
                        .destination_policy
                        .requires_post_write_stamp(),
                    destination_is_wsl: self.destination_policy.endpoint()
                        == bigcp_win::EndpointKind::Wsl,
                    chunk_bytes: self.chunk_bytes,
                    transport: self.transport,
                    checkpoint_threshold: self.options.checkpoint_threshold(),
                    streams: None,
                    logical_bytes: unnamed,
                    promote_threshold: dispatch.promote_threshold,
                    cancel: Arc::clone(&self.worker_cancel),
                    directory_affine: dispatch.directory_affine,
                    phases: Arc::clone(&self.phases),
                };
                return self.submit_file_job(job);
            }
        }
        if self.transport.is_same_spindle() && !self.options.dry_run {
            // The coordinator owns transactional/large files. Letting one run
            // while the phased small-file worker is active would interleave
            // source and destination I/O and undo the same-spindle policy.
            self.drain_file_workers()?;
        }
        if let Err(error) = revalidate_source(source, relative) {
            return self.record_file(
                relative,
                FileOutcome::Failed {
                    bytes: source.metadata.size,
                    error,
                },
                replacement.map(|(fields, _)| fields),
            );
        }
        let streams = if self.source_supports_streams {
            match list_streams(&source.path) {
                Ok(streams) => streams,
                Err(error) => {
                    let error = if error.kind() == std::io::ErrorKind::NotFound {
                        OperationError::from_io_as(
                            ErrorCategory::SourceChanged,
                            "list_streams",
                            relative.to_path_buf(),
                            &error,
                        )
                    } else {
                        OperationError::from_io("list_streams", relative.to_path_buf(), &error)
                    };
                    return self.record_file(
                        relative,
                        FileOutcome::Failed {
                            bytes: source.metadata.size,
                            error,
                        },
                        replacement.map(|(fields, _)| fields),
                    );
                }
            }
        } else {
            vec![bigcp_win::StreamInfo::unnamed(source.metadata.size)]
        };
        let logical_bytes = streams
            .iter()
            .filter(|stream| !stream.is_unnamed())
            .fold(source.metadata.size, |total, stream| {
                total.saturating_add(stream.size)
            });
        if let Err(error) = revalidate_source(source, relative) {
            return self.record_file(
                relative,
                FileOutcome::Failed {
                    bytes: logical_bytes,
                    error,
                },
                replacement.map(|(fields, _)| fields),
            );
        }
        if self.options.dry_run {
            let reason = if replacement.is_some() && replacement_old.is_some() {
                self.counters.would_copy_replaced =
                    self.counters.would_copy_replaced.saturating_add(1);
                "dry_run_would_replace"
            } else {
                self.counters.would_copy_new = self.counters.would_copy_new.saturating_add(1);
                "dry_run_would_copy_new"
            };
            let outcome = FileOutcome::NotAttempted {
                bytes: logical_bytes,
                reason: reason.to_owned(),
            };
            return self.record_file(relative, outcome, replacement.map(|(fields, _)| fields));
        }
        let destination_path = self.destination_root.join(relative);
        let replacement = replacement.map(|(fields, destination_newer)| ReplacementWork {
            fields,
            destination_newer,
        });
        // Every non-dry-run file reaching here copies inline: the fast path
        // above dispatched everything that needs no coordinator-owned journal,
        // and promoted hand-backs re-enter with `dispatch: false`.
        let mut counters = Counters::default();
        // Inline files poll the front end directly. Worker-streamed redirector
        // files poll the shared atomic mirror instead, so both paths retain
        // chunk-boundary graceful cancellation.
        let observer = self.observer;
        let cancel_probe = move || observer.cancellation_requested();
        let request = EngineRequest {
            source_path: &source.path,
            destination_path: &destination_path,
            relative_path: relative,
            source_snapshot,
            replacement_snapshot: destination_snapshot,
            run_id: self.run_id,
            large_threshold: self.options.large_threshold(),
            verify: self.options.verify,
            flush: self.options.flush,
            source_supports_streams: self.source_supports_streams,
            source_supports_eas: self.source_supports_eas,
            destination_supports_streams: self.destination_policy.supports_streams(),
            destination_supports_eas: self.destination_policy.supports_eas(),
            chunk_bytes: self.chunk_bytes,
            transport: self.transport,
            preserve_sparse: self.destination_policy.supports_sparse() && !self.options.no_sparse,
            checkpoint_threshold: self.options.checkpoint_threshold(),
            destination_supports_encryption: self.destination_policy.supports_encryption(),
            destination_supports_preallocation: self.destination_policy.supports_preallocation(),
            destination_supports_persistent_acls: self
                .destination_policy
                .supports_persistent_acls(),
            destination_supports_posix_unlink_rename: self
                .destination_policy
                .supports_posix_unlink_rename(),
            destination_metadata: self.destination_policy.project_basic(source.metadata.basic),
            destination_requires_post_write_stamp: self
                .destination_policy
                .requires_post_write_stamp(),
            destination_is_wsl: self.destination_policy.endpoint() == bigcp_win::EndpointKind::Wsl,
            known_streams: Some(&streams),
            cancel: &cancel_probe,
            promote_threshold: None,
            phases: &self.phases,
        };
        let copy_started = Instant::now();
        let result = copy_file(&request, &mut counters, self.journal.as_mut());
        self.finish_copy(CompletedCopy {
            source_path: source.path.clone(),
            destination_path,
            source_snapshot: source_snapshot.clone(),
            destination_snapshot: destination_snapshot.cloned(),
            replacement,
            counters,
            result,
            logical_bytes,
            seconds: copy_started.elapsed().as_secs_f64(),
        })
    }

    fn submit_file_job(&mut self, job: FileCopyJob) -> Result<(), BigcpError> {
        if self.file_jobs_outstanding >= self.file_workers.capacity() {
            self.receive_file_job()?;
        }
        // Local/generic-UNC small creates remain directory-affine (see
        // FileWorkers). Redirector streams and WSL-destination creates use a
        // separate round-robin shard so one directory can occupy all workers.
        let parent = job
            .source_snapshot
            .relative_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        let shard = if job.directory_affine {
            let mut hasher = DefaultHasher::new();
            parent.hash(&mut hasher);
            usize::try_from(hasher.finish()).unwrap_or(usize::MAX)
        } else {
            let shard = self.next_independent_shard;
            self.next_independent_shard = self.next_independent_shard.wrapping_add(1);
            shard
        };
        let mut job = job;
        loop {
            match self.file_workers.try_submit(job, shard)? {
                None => break,
                Some(returned) => {
                    // The shard's queue is full: drain one completion (which
                    // may be anyone's) and retry — never block with results
                    // unconsumed.
                    self.receive_file_job()?;
                    job = returned;
                }
            }
        }
        self.file_jobs_outstanding = self.file_jobs_outstanding.saturating_add(1);
        *self.dir_outstanding.entry(parent).or_insert(0) += 1;
        Ok(())
    }

    fn receive_file_job(&mut self) -> Result<(), BigcpError> {
        let completed = loop {
            self.observe_cancellation()?;
            if let Some(completed) = self
                .file_workers
                .receive_timeout(Duration::from_millis(100))?
            {
                break completed;
            }
        };
        self.file_jobs_outstanding = self.file_jobs_outstanding.saturating_sub(1);
        let parent = completed
            .source_snapshot
            .relative_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        if let Some(count) = self.dir_outstanding.get_mut(&parent) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.dir_outstanding.remove(&parent);
            }
        }
        let phase_timer = Instant::now();
        let outcome = self.finish_copy(completed);
        self.phases.record(8, phase_timer.elapsed());
        outcome
    }

    fn drain_file_workers(&mut self) -> Result<(), BigcpError> {
        while self.file_jobs_outstanding > 0 {
            self.receive_file_job()?;
        }
        Ok(())
    }

    fn finish_copy(&mut self, completed: CompletedCopy) -> Result<(), BigcpError> {
        if let Err(error) = &completed.result
            && error.operation == crate::engine::PROMOTED_TO_COORDINATOR
        {
            // The worker found a stream large enough to deserve checkpoints
            // and mid-file cancellation, and copied nothing. Rerun the file
            // inline where the journal and the cancel probe live; the rerun
            // records the real outcome, so this attempt contributes nothing.
            let CompletedCopy {
                source_path,
                source_snapshot,
                destination_snapshot,
                replacement,
                ..
            } = completed;
            let source = DirectoryEntry {
                name: source_snapshot
                    .relative_path
                    .file_name()
                    .map(OsString::from)
                    .unwrap_or_default(),
                path: source_path,
                metadata: source_snapshot.metadata.clone(),
            };
            let relative = source_snapshot.relative_path.clone();
            let replacement = replacement.map(|work| (work.fields, work.destination_newer));
            return self.copy_classified(
                &source,
                &relative,
                &source_snapshot,
                destination_snapshot.as_ref(),
                replacement,
                false,
            );
        }
        self.counters.bytes_read_source = self
            .counters
            .bytes_read_source
            .saturating_add(completed.counters.bytes_read_source);
        self.counters.bytes_written_destination = self
            .counters
            .bytes_written_destination
            .saturating_add(completed.counters.bytes_written_destination);
        self.counters.bytes_verified = self
            .counters
            .bytes_verified
            .saturating_add(completed.counters.bytes_verified);
        self.stats.record(
            completed.counters.bytes_read_source,
            completed.counters.bytes_written_destination,
            1,
        );
        let relative = &completed.source_snapshot.relative_path;
        let differences = completed
            .replacement
            .as_ref()
            .map(|replacement| replacement.fields.clone());
        let replacement_old = completed.destination_snapshot.as_ref();
        let outcome = match completed.result {
            Ok(result) => {
                if result.journal_degraded {
                    self.increment_warning("checkpointing_disabled");
                    self.audit.emit(&AuditEvent::Warning {
                        kind: "checkpointing_disabled".to_owned(),
                        rel: Some(AuditPath::from_path(relative)),
                        message: "checkpoint persistence failed; this run continues without further resume checkpoints".to_owned(),
                    })?;
                }
                if result.efs_downgraded {
                    self.increment_warning("efs_downgrade");
                    self.audit.emit(&AuditEvent::Warning {
                        kind: "efs_downgrade".to_owned(),
                        rel: Some(AuditPath::from_path(relative)),
                        message: "source EFS state could not be represented at the destination"
                            .to_owned(),
                    })?;
                }
                if result.eas_dropped {
                    self.increment_warning("ea_dropped");
                    self.audit.emit(&AuditEvent::Warning {
                        kind: "ea_dropped".to_owned(),
                        rel: Some(AuditPath::from_path(relative)),
                        message: "destination volume does not advertise extended-attribute support"
                            .to_owned(),
                    })?;
                }
                if result.checkpoint_used
                    && let Some(journal) = &mut self.journal
                {
                    if journal
                        .append(JournalEvent::PartDone {
                            relative_path: path_key(relative),
                        })
                        .is_err()
                    {
                        self.increment_warning("checkpointing_disabled");
                        self.audit.emit(&AuditEvent::Warning {
                            kind: "checkpointing_disabled".to_owned(),
                            rel: Some(AuditPath::from_path(relative)),
                            message: "completed-part retirement could not be journaled; metadata reclassification remains authoritative on rerun".to_owned(),
                        })?;
                        self.journal = None;
                    }
                }
                if result.streams_dropped > 0 {
                    for _ in 0..result.streams_dropped {
                        self.increment_warning("streams_dropped");
                    }
                    self.audit.emit(&AuditEvent::Warning {
                        kind: "streams_dropped".to_owned(),
                        rel: Some(AuditPath::from_path(relative)),
                        message: format!(
                            "destination volume cannot represent {} named data stream(s)",
                            result.streams_dropped
                        ),
                    })?;
                }
                if self.options.verify {
                    if let Some(digest) = &result.digest {
                        self.verification_targets.push(VerificationTarget {
                            relative_path: relative.clone(),
                            destination_path: completed.destination_path,
                            expected_digest: digest.clone(),
                            expected_size: completed.source_snapshot.metadata.size,
                            expected_metadata: self
                                .destination_policy
                                .project_basic(completed.source_snapshot.metadata.basic),
                            expected_streams: result.stream_digests.clone(),
                            expected_ea_digest: result.ea_digest.clone(),
                        });
                    }
                }
                if let (Some(replacement), Some(old)) = (&completed.replacement, replacement_old) {
                    FileOutcome::CopiedReplaced {
                        bytes: result.bytes,
                        digest: result.digest,
                        destination_newer: replacement.destination_newer,
                        old_size: old.metadata.size,
                        old_mtime: old.metadata.basic.last_write_time,
                        old_attributes: old.metadata.basic.attributes,
                    }
                } else {
                    FileOutcome::CopiedNew {
                        bytes: result.bytes,
                        digest: result.digest,
                    }
                }
            }
            Err(error) if error.operation == crate::engine::CANCELED_MID_FILE => {
                // A clean cancel is not an error condition (exit 3 keeps an
                // empty error summary); the interrupted temp self-deletes or
                // stays behind as a verified-checkpoint resume asset.
                self.canceled = true;
                self.worker_cancel.store(true, Ordering::Release);
                self.increment_warning("canceled_mid_file");
                self.audit.emit(&AuditEvent::Warning {
                    kind: "canceled_mid_file".to_owned(),
                    rel: Some(AuditPath::from_path(relative)),
                    message: "copy stopped between chunks by graceful cancellation; rerun to finish this file".to_owned(),
                })?;
                FileOutcome::NotAttempted {
                    bytes: completed.logical_bytes,
                    reason: "canceled".to_owned(),
                }
            }
            Err(error) => FileOutcome::Failed {
                bytes: completed.logical_bytes,
                error,
            },
        };
        if let Some(analysis) = self.analysis.as_mut()
            && let FileOutcome::CopiedNew { bytes, .. } | FileOutcome::CopiedReplaced { bytes, .. } =
                &outcome
        {
            // The outcome's byte count is the engine's exact logical figure;
            // fast-dispatched jobs only carried the enumerated unnamed size.
            analysis.record(&relative.to_string_lossy(), *bytes, completed.seconds);
        }
        self.record_file(relative, outcome, differences)
    }

    fn handle_reparse(
        &mut self,
        source: DirectoryEntry,
        destination: Option<DirectoryEntry>,
        relative: PathBuf,
    ) -> Result<(), BigcpError> {
        self.counters.links_discovered = self.counters.links_discovered.saturating_add(1);
        if !self.destination_policy.supports_reparse_points() {
            self.counters.links_failed = self.counters.links_failed.saturating_add(1);
            return self.record_link_event(
                &relative,
                "failed_link",
                None,
                None,
                Some(OperationError::semantic(
                    ErrorCategory::FsLimit,
                    "create_reparse",
                    relative.clone(),
                    format!(
                        "{} cannot represent reparse points; the link was not followed or copied as target data",
                        self.destination_policy.filesystem().name()
                    ),
                )),
            );
        }
        let source_snapshot = EntrySnapshot {
            relative_path: relative.clone(),
            metadata: source.metadata.clone(),
        };
        let destination_snapshot = destination.as_ref().map(|entry| EntrySnapshot {
            relative_path: relative.clone(),
            metadata: entry.metadata.clone(),
        });
        let classification = classify(
            &source_snapshot,
            destination_snapshot.as_ref(),
            self.options.replace,
            self.destination_policy,
        );
        let replacement_event = match (&classification, destination_snapshot.as_ref()) {
            (
                Classification::Replace {
                    fields,
                    destination_newer,
                },
                Some(old),
            ) => Some(ReplacementEvent {
                old_size: old.metadata.size,
                old_mtime: old.metadata.basic.last_write_time,
                old_attributes: old.metadata.basic.attributes,
                destination_newer: *destination_newer,
                differences: fields.iter().map(|field| (*field).to_owned()).collect(),
            }),
            _ => None,
        };
        match classification {
            Classification::Same => {
                self.counters.links_skipped = self.counters.links_skipped.saturating_add(1);
                self.record_link_event(
                    &relative,
                    "skipped_link",
                    Some("same".to_owned()),
                    None,
                    None,
                )?;
            }
            Classification::MetadataDiff(fields) => {
                if self.options.dry_run {
                    self.counters.links_planned = self.counters.links_planned.saturating_add(1);
                    self.record_link_event(
                        &relative,
                        "not_attempted_link",
                        Some(format!("dry_run_would_fix_metadata:{}", fields.join(","))),
                        None,
                        None,
                    )?;
                } else {
                    let result = destination.as_ref().map_or_else(
                        || {
                            Err(OperationError::semantic(
                                ErrorCategory::Internal,
                                "set_link_meta",
                                relative.clone(),
                                "classifier requested link metadata repair without a destination",
                            ))
                        },
                        |entry| {
                            repair_metadata(
                                &source,
                                entry,
                                &fields,
                                &relative,
                                self.destination_policy,
                                self.source_supports_eas,
                            )
                        },
                    );
                    match result {
                        Ok(()) => {
                            self.counters.links_copied =
                                self.counters.links_copied.saturating_add(1);
                            self.record_link_event(
                                &relative,
                                "meta_fixed_link",
                                Some(fields.join(",")),
                                None,
                                None,
                            )?;
                        }
                        Err(error) => {
                            self.counters.links_failed =
                                self.counters.links_failed.saturating_add(1);
                            self.record_link_event(
                                &relative,
                                "failed_link",
                                None,
                                None,
                                Some(error),
                            )?;
                        }
                    }
                }
            }
            Classification::SkipDifferent { fields, .. } => {
                self.counters.links_not_attempted =
                    self.counters.links_not_attempted.saturating_add(1);
                self.record_link_event(
                    &relative,
                    "skipped_diff_link",
                    Some(fields.join(",")),
                    None,
                    None,
                )?;
            }
            Classification::TypeConflict => {
                self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                let error = OperationError::semantic(
                    ErrorCategory::TypeConflict,
                    "join",
                    relative.clone(),
                    "destination object type conflicts with source reparse point",
                );
                self.record_link_event(&relative, "failed_link", None, None, Some(error))?;
            }
            Classification::New | Classification::Replace { .. } => {
                if self.options.dry_run {
                    self.counters.links_planned = self.counters.links_planned.saturating_add(1);
                    self.record_link_event(
                        &relative,
                        "not_attempted_link",
                        Some(if replacement_event.is_some() {
                            "dry_run_would_replace".to_owned()
                        } else {
                            "dry_run_would_copy_new".to_owned()
                        }),
                        replacement_event,
                        None,
                    )?;
                    return Ok(());
                }
                if let Err(error) = revalidate_source(&source, &relative) {
                    self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                    self.record_link_event(
                        &relative,
                        "failed_link",
                        None,
                        replacement_event,
                        Some(error),
                    )?;
                    return Ok(());
                }
                let mut protected_dacl = None;
                if let Some(expected) = destination.as_ref() {
                    if let Err(error) = revalidate_destination(expected, &relative) {
                        self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                        self.record_link_event(
                            &relative,
                            "failed_link",
                            None,
                            replacement_event,
                            Some(error),
                        )?;
                        return Ok(());
                    }
                    let destination_path = self.destination_root.join(&relative);
                    if !self.destination_policy.supports_persistent_acls() {
                        protected_dacl = None;
                    } else {
                        match bigcp_win::read_protected_dacl_checked(
                            &destination_path,
                            expected.metadata.identity,
                        ) {
                            Ok(value) => protected_dacl = value,
                            Err(error) => {
                                self.counters.links_failed =
                                    self.counters.links_failed.saturating_add(1);
                                let error = if error.kind() == std::io::ErrorKind::InvalidData {
                                    OperationError::from_io_as(
                                        ErrorCategory::DestinationChanged,
                                        "read_dacl",
                                        relative.clone(),
                                        &error,
                                    )
                                } else {
                                    OperationError::from_io("read_dacl", relative.clone(), &error)
                                };
                                self.record_link_event(
                                    &relative,
                                    "failed_link",
                                    None,
                                    replacement_event,
                                    Some(error),
                                )?;
                                return Ok(());
                            }
                        }
                    }
                }
                let destination_path = self.destination_root.join(&relative);
                match copy_reparse(
                    &source.path,
                    &destination_path,
                    self.run_id,
                    &source.metadata,
                    destination_snapshot
                        .as_ref()
                        .map(|snapshot| &snapshot.metadata),
                    self.options.raw_reparse,
                    self.options.flush,
                    protected_dacl.as_ref(),
                    self.destination_policy.project_basic(source.metadata.basic),
                    self.destination_policy.supports_posix_unlink_rename(),
                ) {
                    Ok(result) => {
                        self.counters.bytes_read_source = self
                            .counters
                            .bytes_read_source
                            .saturating_add(result.named_stream_bytes);
                        self.counters.bytes_written_destination = self
                            .counters
                            .bytes_written_destination
                            .saturating_add(result.named_stream_bytes);
                        self.stats
                            .record(result.named_stream_bytes, result.named_stream_bytes, 1);
                        self.counters.links_copied = self.counters.links_copied.saturating_add(1);
                        self.record_link_event(
                            &relative,
                            "copied_link",
                            None,
                            replacement_event,
                            None,
                        )?;
                    }
                    Err(bigcp_win::ReparseCopyError::SourceChanged) => {
                        self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                        let operation_error = OperationError::semantic(
                            ErrorCategory::SourceChanged,
                            "copy_reparse",
                            relative.clone(),
                            "source reparse point changed while it was being copied",
                        );
                        self.record_link_event(
                            &relative,
                            "failed_link",
                            None,
                            replacement_event,
                            Some(operation_error),
                        )?;
                    }
                    Err(bigcp_win::ReparseCopyError::Io(error)) => {
                        self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                        let operation_error = if error.kind() == std::io::ErrorKind::Unsupported {
                            OperationError::from_io_as(
                                ErrorCategory::UnsupportedReparse,
                                "copy_reparse",
                                relative.clone(),
                                &error,
                            )
                        } else {
                            OperationError::from_io("copy_reparse", relative.clone(), &error)
                        };
                        self.record_link_event(
                            &relative,
                            "failed_link",
                            None,
                            replacement_event,
                            Some(operation_error),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn is_system_candidate(&self, entry: &DirectoryEntry, parent_relative: &Path) -> bool {
        self.source_is_volume_root
            && parent_relative.as_os_str().is_empty()
            && SYSTEM_EXCLUSIONS
                .iter()
                .any(|name| entry.name.to_string_lossy().eq_ignore_ascii_case(name))
    }

    fn exclusion_reason(
        &self,
        entry: &DirectoryEntry,
        parent_relative: &Path,
    ) -> Option<&'static str> {
        if !self.options.include_system && self.is_system_candidate(entry, parent_relative) {
            Some("system")
        } else if self.options.skip_cloud && is_cloud_placeholder(entry.metadata.basic.attributes) {
            Some("cloud_placeholder")
        } else {
            None
        }
    }

    fn record_exclusion(
        &mut self,
        entry: &DirectoryEntry,
        relative: PathBuf,
        reason: &'static str,
    ) -> Result<(), BigcpError> {
        match entry.metadata.kind {
            ObjectKind::File => self.record_file(
                &relative,
                FileOutcome::Excluded {
                    bytes: entry.metadata.size,
                    reason: reason.to_owned(),
                },
                None,
            ),
            ObjectKind::Directory => {
                self.counters.dirs_discovered = self.counters.dirs_discovered.saturating_add(1);
                self.counters.dirs_excluded = self.counters.dirs_excluded.saturating_add(1);
                self.audit.emit(&AuditEvent::Directory {
                    action: format!("excluded_{reason}"),
                    rel: AuditPath::from_path(&relative),
                })
            }
            ObjectKind::Reparse => {
                self.counters.links_discovered = self.counters.links_discovered.saturating_add(1);
                self.counters.links_excluded = self.counters.links_excluded.saturating_add(1);
                self.audit.emit(&AuditEvent::Warning {
                    kind: format!("link_excluded_{reason}"),
                    rel: Some(AuditPath::from_path(&relative)),
                    message: format!("reparse point excluded by {reason} policy"),
                })
            }
        }
    }

    fn record_entry_failure(
        &mut self,
        entry: &DirectoryEntry,
        relative: PathBuf,
        mut error: OperationError,
    ) -> Result<(), BigcpError> {
        error.path.clone_from(&relative);
        match entry.metadata.kind {
            ObjectKind::File => self.record_file(
                &relative,
                FileOutcome::Failed {
                    bytes: entry.metadata.size,
                    error,
                },
                None,
            ),
            ObjectKind::Directory => self.account_failed_subtree(&entry.path, &relative, error),
            ObjectKind::Reparse => {
                self.counters.links_discovered = self.counters.links_discovered.saturating_add(1);
                self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                self.record_error(error)?;
                Ok(())
            }
        }
    }

    fn account_failed_subtree(
        &mut self,
        source_root: &Path,
        relative_root: &Path,
        error: OperationError,
    ) -> Result<(), BigcpError> {
        self.counters.dirs_discovered = self.counters.dirs_discovered.saturating_add(1);
        self.counters.dirs_failed = self.counters.dirs_failed.saturating_add(1);
        self.record_error(error)?;
        self.walk_subtree_not_attempted(source_root, relative_root)
    }

    /// Accounts every discoverable descendant of an already-failed directory
    /// as not-attempted, without generating a per-descendant error: the one
    /// recorded parent failure is the cause, and descendants inherit it
    /// (PLAN section 5.13 `parent_dir_failed`).
    fn walk_subtree_not_attempted(
        &mut self,
        source_root: &Path,
        relative_root: &Path,
    ) -> Result<(), BigcpError> {
        let mut directories = vec![(source_root.to_path_buf(), relative_root.to_path_buf())];
        while let Some((source, relative)) = directories.pop() {
            let Ok(entries) = enumerate_directory(&source) else {
                continue;
            };
            for entry in entries {
                let child_relative = relative.join(&entry.name);
                self.account_entry_not_attempted(&entry, child_relative, &mut directories)?;
            }
        }
        Ok(())
    }

    fn account_entry_not_attempted(
        &mut self,
        entry: &DirectoryEntry,
        relative: PathBuf,
        directories: &mut Vec<(PathBuf, PathBuf)>,
    ) -> Result<(), BigcpError> {
        match entry.metadata.kind {
            ObjectKind::File => {
                self.counters.note_file_discovered(entry.metadata.size);
                self.record_file(
                    &relative,
                    FileOutcome::NotAttempted {
                        bytes: entry.metadata.size,
                        reason: "parent_dir_failed".to_owned(),
                    },
                    None,
                )?;
            }
            ObjectKind::Directory => {
                self.counters.dirs_discovered = self.counters.dirs_discovered.saturating_add(1);
                self.counters.dirs_failed = self.counters.dirs_failed.saturating_add(1);
                self.audit.emit(&AuditEvent::Directory {
                    action: "not_attempted_parent_failed".to_owned(),
                    rel: AuditPath::from_path(&relative),
                })?;
                directories.push((entry.path.clone(), relative));
            }
            ObjectKind::Reparse => {
                self.counters.links_discovered = self.counters.links_discovered.saturating_add(1);
                self.counters.links_not_attempted =
                    self.counters.links_not_attempted.saturating_add(1);
                self.record_link_event(
                    &relative,
                    "not_attempted_link",
                    Some("parent_dir_failed".to_owned()),
                    None,
                    None,
                )?;
            }
        }
        Ok(())
    }

    /// Accounts the direct children of a directory whose destination join
    /// failed. All of them are `not_attempted` (nothing was dispatched), the
    /// same terminal category deeper descendants receive — the parent's one
    /// recorded error is the cause for the whole subtree.
    fn account_children_not_attempted(
        &mut self,
        entries: &[DirectoryEntry],
        parent_relative: &Path,
    ) -> Result<(), BigcpError> {
        let mut directories = Vec::new();
        for entry in entries {
            let relative = parent_relative.join(&entry.name);
            self.account_entry_not_attempted(entry, relative, &mut directories)?;
        }
        while let Some((source, relative)) = directories.pop() {
            let Ok(children) = enumerate_directory(&source) else {
                continue;
            };
            for entry in children {
                let child_relative = relative.join(&entry.name);
                self.account_entry_not_attempted(&entry, child_relative, &mut directories)?;
            }
        }
        Ok(())
    }

    fn record_file(
        &mut self,
        relative: &Path,
        outcome: FileOutcome,
        differences: Option<Vec<&'static str>>,
    ) -> Result<(), BigcpError> {
        if matches!(
            outcome,
            FileOutcome::CopiedNew { .. }
                | FileOutcome::CopiedReplaced { .. }
                | FileOutcome::SkippedSame { .. }
                | FileOutcome::SkippedDifferent { .. }
                | FileOutcome::MetadataFixed { .. }
        ) {
            self.breaker_streak = 0;
        }
        let (action, size, hash, error, reason, replacement_event) = match &outcome {
            FileOutcome::CopiedNew { bytes, digest } => {
                ("copied", *bytes, digest.clone(), None, None, None)
            }
            FileOutcome::CopiedReplaced {
                bytes,
                digest,
                destination_newer,
                old_size,
                old_mtime,
                old_attributes,
            } => {
                let differences_owned = differences
                    .as_ref()
                    .map(|values| values.iter().map(|value| (*value).to_owned()).collect())
                    .unwrap_or_default();
                (
                    "copied",
                    *bytes,
                    digest.clone(),
                    None,
                    None,
                    Some(ReplacementEvent {
                        old_size: *old_size,
                        old_mtime: *old_mtime,
                        old_attributes: *old_attributes,
                        destination_newer: *destination_newer,
                        differences: differences_owned,
                    }),
                )
            }
            FileOutcome::SkippedSame { bytes } => {
                ("skipped", *bytes, None, None, Some("same".to_owned()), None)
            }
            FileOutcome::SkippedDifferent {
                bytes,
                destination_newer,
                old_size,
                old_mtime,
                old_attributes,
            } => (
                "skipped_diff",
                *bytes,
                None,
                None,
                Some(
                    differences
                        .as_ref()
                        .map_or_else(|| "different".to_owned(), |values| values.join(",")),
                ),
                // A withheld replacement logs the same destination detail a
                // performed replacement would (F13/F20).
                Some(ReplacementEvent {
                    old_size: *old_size,
                    old_mtime: *old_mtime,
                    old_attributes: *old_attributes,
                    destination_newer: *destination_newer,
                    differences: differences
                        .as_ref()
                        .map(|values| values.iter().map(|value| (*value).to_owned()).collect())
                        .unwrap_or_default(),
                }),
            ),
            FileOutcome::MetadataFixed { bytes } => (
                "meta_fixed",
                *bytes,
                None,
                None,
                differences.as_ref().map(|values| values.join(",")),
                None,
            ),
            FileOutcome::Failed { bytes, error } => {
                ("failed", *bytes, None, Some(error.clone()), None, None)
            }
            FileOutcome::Excluded { bytes, reason } => {
                ("excluded", *bytes, None, None, Some(reason.clone()), None)
            }
            FileOutcome::NotAttempted { bytes, reason } => (
                "not_attempted",
                *bytes,
                None,
                None,
                Some(reason.clone()),
                None,
            ),
        };
        if let Some(error) = &error {
            self.aggregate_error(error.clone());
        }
        self.record_folder_outcome(relative, &outcome);
        if let FileOutcome::CopiedReplaced {
            bytes,
            destination_newer,
            old_size,
            old_mtime,
            ..
        } = &outcome
        {
            self.replacements.count = self.replacements.count.saturating_add(1);
            self.replacements.bytes = self.replacements.bytes.saturating_add(*bytes);
            if *destination_newer {
                self.replacements.destination_newer =
                    self.replacements.destination_newer.saturating_add(1);
            }
            *self
                .replacements
                .by_folder
                .entry(top_level(relative))
                .or_insert(0) += 1;
            if self.replacements.samples.len() < REPORT_SAMPLE_LIMIT {
                self.replacements.samples.push(ReplacementSample {
                    relative_path: relative.to_string_lossy().into_owned(),
                    old_size: *old_size,
                    old_mtime: *old_mtime,
                    differences: differences
                        .as_ref()
                        .map(|values| values.iter().map(|value| (*value).to_owned()).collect())
                        .unwrap_or_default(),
                });
            }
        }
        self.counters.apply_file(&outcome);
        self.audit.emit(&AuditEvent::File {
            action: action.to_owned(),
            rel: AuditPath::from_path(relative),
            size,
            hash,
            replacement: replacement_event,
            error,
            reason,
        })?;
        if let Some(point) = self.stats.maybe_roll(self.stat_interval()) {
            self.audit.emit(&AuditEvent::Stat {
                counters: self.counters.clone(),
                read_mbps: point.read_mbps,
                write_mbps: point.write_mbps,
            })?;
            self.publish(RunState::Copying);
            self.last_snapshot = Instant::now();
        } else if self.last_snapshot.elapsed() >= Duration::from_millis(250) {
            self.publish(RunState::Copying);
            self.last_snapshot = Instant::now();
        }
        Ok(())
    }

    /// `--analyze` samples the timeline 6x more finely; the 3600-point report
    /// cap still bounds output (5 s x 3600 = five hours of coverage).
    fn stat_interval(&self) -> Duration {
        if self.options.analyze {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(30)
        }
    }

    fn record_error(&mut self, error: OperationError) -> Result<(), BigcpError> {
        self.aggregate_error(error.clone());
        self.audit.emit(&AuditEvent::Error { error })
    }

    fn record_link_event(
        &mut self,
        relative: &Path,
        action: &str,
        reason: Option<String>,
        replacement: Option<ReplacementEvent>,
        error: Option<OperationError>,
    ) -> Result<(), BigcpError> {
        if let Some(error) = &error {
            self.aggregate_error(error.clone());
        }
        self.audit.emit(&AuditEvent::File {
            action: action.to_owned(),
            rel: AuditPath::from_path(relative),
            size: 0,
            hash: None,
            replacement,
            error,
            reason,
        })
    }

    fn record_folder_outcome(&mut self, relative: &Path, outcome: &FileOutcome) {
        let summary = self.folders.entry(top_level(relative)).or_default();
        summary.files_discovered = summary.files_discovered.saturating_add(1);
        let bytes = match outcome {
            FileOutcome::CopiedNew { bytes, .. } => {
                summary.copied_new = summary.copied_new.saturating_add(1);
                summary.logical_bytes_copied = summary.logical_bytes_copied.saturating_add(*bytes);
                *bytes
            }
            FileOutcome::CopiedReplaced { bytes, .. } => {
                summary.copied_replaced = summary.copied_replaced.saturating_add(1);
                summary.logical_bytes_copied = summary.logical_bytes_copied.saturating_add(*bytes);
                *bytes
            }
            FileOutcome::SkippedSame { bytes } => {
                summary.skipped_same = summary.skipped_same.saturating_add(1);
                *bytes
            }
            FileOutcome::SkippedDifferent { bytes, .. } => {
                summary.skipped_diff = summary.skipped_diff.saturating_add(1);
                *bytes
            }
            FileOutcome::MetadataFixed { bytes } => {
                summary.meta_fixed = summary.meta_fixed.saturating_add(1);
                *bytes
            }
            FileOutcome::Failed { bytes, .. } => {
                summary.failed = summary.failed.saturating_add(1);
                *bytes
            }
            FileOutcome::Excluded { bytes, .. } => {
                summary.excluded = summary.excluded.saturating_add(1);
                *bytes
            }
            FileOutcome::NotAttempted { bytes, .. } => {
                summary.not_attempted = summary.not_attempted.saturating_add(1);
                *bytes
            }
        };
        summary.logical_bytes_discovered = summary.logical_bytes_discovered.saturating_add(bytes);
    }

    fn reconcile_folders(&self) -> Result<(), String> {
        let mut total = FolderSummary::default();
        for summary in self.folders.values() {
            total.files_discovered = total
                .files_discovered
                .saturating_add(summary.files_discovered);
            total.copied_new = total.copied_new.saturating_add(summary.copied_new);
            total.copied_replaced = total
                .copied_replaced
                .saturating_add(summary.copied_replaced);
            total.skipped_same = total.skipped_same.saturating_add(summary.skipped_same);
            total.skipped_diff = total.skipped_diff.saturating_add(summary.skipped_diff);
            total.meta_fixed = total.meta_fixed.saturating_add(summary.meta_fixed);
            total.failed = total.failed.saturating_add(summary.failed);
            total.excluded = total.excluded.saturating_add(summary.excluded);
            total.not_attempted = total.not_attempted.saturating_add(summary.not_attempted);
            total.logical_bytes_discovered = total
                .logical_bytes_discovered
                .saturating_add(summary.logical_bytes_discovered);
            total.logical_bytes_copied = total
                .logical_bytes_copied
                .saturating_add(summary.logical_bytes_copied);
        }
        let matches = total.files_discovered == self.counters.files_discovered
            && total.copied_new == self.counters.copied_new
            && total.copied_replaced == self.counters.copied_replaced
            && total.skipped_same == self.counters.skipped_same
            && total.skipped_diff == self.counters.skipped_diff
            && total.meta_fixed == self.counters.meta_fixed
            && total.failed == self.counters.failed
            && total.excluded == self.counters.excluded
            && total.not_attempted == self.counters.not_attempted
            && total.logical_bytes_discovered == self.counters.bytes_logical_discovered
            && total.logical_bytes_copied == self.counters.bytes_logical_copied;
        if matches {
            Ok(())
        } else {
            Err("top-level folder summaries do not reconcile with global file counters".to_owned())
        }
    }

    fn aggregate_error(&mut self, error: OperationError) {
        // Breaker accounting: device-gone and disk-full failures extend the
        // streak; other categories neither extend nor reset it (a failure
        // storm from a removed device surfaces mixed categories), and only a
        // genuine success (record_file) resets it.
        if matches!(
            error.category,
            ErrorCategory::DeviceGone | ErrorCategory::Space
        ) {
            self.breaker_streak = self.breaker_streak.saturating_add(1);
            if self.breaker_streak >= BREAKER_THRESHOLD && self.breaker.is_none() {
                self.breaker = Some(error.category);
                self.worker_cancel.store(true, Ordering::Release);
            }
        }
        let folder = top_level(&error.path);
        let summary = self
            .errors
            .entry(error.category)
            .or_insert_with(|| ErrorSummary {
                category: error.category,
                count: 0,
                hint: error.hint.clone(),
                by_folder: BTreeMap::new(),
                samples: Vec::new(),
            });
        summary.count = summary.count.saturating_add(1);
        *summary.by_folder.entry(folder).or_insert(0) += 1;
        if summary.samples.len() < REPORT_SAMPLE_LIMIT {
            summary.samples.push(error);
        }
    }

    fn increment_warning(&mut self, warning: &str) {
        let count = self.warnings.entry(warning.to_owned()).or_insert(0);
        *count = count.saturating_add(1);
    }

    fn publish(&self, state: RunState) {
        let (read_bytes_per_second, write_bytes_per_second) = self.stats.current_rates();
        let failures_by_category = self
            .errors
            .iter()
            .map(|(category, summary)| (*category, summary.count))
            .collect();
        self.observer.on_snapshot(&RunSnapshot {
            state,
            counters: self.counters.clone(),
            read_bytes_per_second,
            write_bytes_per_second,
            failures_by_category,
            active_paths: Vec::new(),
        });
    }
}

struct Preflight {
    source: PathBuf,
    destination: PathBuf,
    destination_exists: bool,
    source_volume: VolumeInfo,
    destination_volume: VolumeInfo,
    /// Destination device write cache reported disabled — the Quick-removal
    /// signature, measured at ~3.4× cost on metadata-heavy small-file
    /// workloads (BENCHMARKS.md 2026-07-29). Surfaced as a warning and a
    /// hint; never changed by bigcp.
    destination_write_cache_disabled: bool,
    profile: CopyProfile,
    state_dir: PathBuf,
    log_path: PathBuf,
    report_path: PathBuf,
    _source_pin: File,
    _destination_pin: Option<File>,
    _lock: DestinationLock,
}

fn preflight(options: &CopyOptions) -> Result<Preflight, BigcpError> {
    let source_input = absolute_extended(&options.source)
        .map_err(|error| BigcpError::io("normalize source", error))?;
    let source_pin =
        open_root(&source_input).map_err(|error| BigcpError::io("open source root", error))?;
    let source =
        final_path(&source_pin).map_err(|error| BigcpError::io("resolve source root", error))?;
    let source_root_metadata =
        metadata_at(&source).map_err(|error| BigcpError::io("read source root metadata", error))?;
    if source_root_metadata.kind != ObjectKind::Directory {
        return Err(BigcpError::Invalid(
            "source must resolve to a real directory, not a file or reparse point".to_owned(),
        ));
    }

    let destination_input = absolute_extended(&options.destination)
        .map_err(|error| BigcpError::io("normalize destination", error))?;
    let (destination_ancestor, missing) = nearest_existing(&destination_input)?;
    let ancestor_pin = open_root(&destination_ancestor)
        .map_err(|error| BigcpError::io("open destination ancestor", error))?;
    let final_ancestor = final_path(&ancestor_pin)
        .map_err(|error| BigcpError::io("resolve destination ancestor", error))?;
    let prospective_destination = missing
        .iter()
        .fold(final_ancestor.clone(), |path, component| {
            path.join(component)
        });
    let source_endpoint = classify_endpoint(&source);
    let destination_endpoint = classify_endpoint(&final_ancestor);
    if is_same_or_descendant_with(
        &prospective_destination,
        &source,
        source_endpoint.names_are_case_sensitive(),
    )
    .map_err(|error| BigcpError::io("compare roots", error))?
        || is_same_or_descendant_with(
            &source,
            &prospective_destination,
            destination_endpoint.names_are_case_sensitive(),
        )
        .map_err(|error| BigcpError::io("compare roots", error))?
    {
        return Err(BigcpError::Invalid(
            "source and destination must be distinct, non-nested trees".to_owned(),
        ));
    }
    let destination_lock_key = path_hash(&prospective_destination)?;
    let lock = DestinationLock::acquire(&destination_lock_key[..32]).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            BigcpError::Locked(error.to_string())
        } else {
            BigcpError::io("acquire destination lock", error)
        }
    })?;
    let (state_dir, log_path, report_path) =
        audit_paths(options, &source, &prospective_destination)?;

    let source_volume =
        probe_volume(&source).map_err(|error| BigcpError::io("probe source volume", error))?;
    let destination_volume = probe_volume(&final_ancestor)
        .map_err(|error| BigcpError::io("probe destination volume", error))?;
    if (source_volume.endpoint.is_remote() || destination_volume.endpoint.is_remote())
        && !options.accept_remote_paths
        && !options.dry_run
    {
        return Err(BigcpError::Invalid(
            "UNC/WSL copying requires explicit remote-path acceptance; rerun interactively or pass --accept-remote-paths"
                .to_owned(),
        ));
    }
    if destination_volume.filesystem.is_fat_family()
        && !options.accept_degraded_filesystem
        && !options.dry_run
    {
        return Err(BigcpError::Invalid(format!(
            "{} destination requires explicit fidelity-degradation acceptance; rerun interactively or pass --accept-degraded-filesystem",
            destination_volume.filesystem.name()
        )));
    }
    let source_device = bigcp_win::profile_device(&source_volume);
    let destination_device = bigcp_win::profile_device(&destination_volume);
    let profile = select_copy_profile(
        &source_device,
        &destination_device,
        options.source_profile,
        options.destination_profile,
        &options.tune,
    )?;
    let destination_preexisted = missing.is_empty();
    let (destination, destination_pin) = if destination_preexisted {
        let pin = ancestor_pin;
        (final_ancestor, Some(pin))
    } else if options.dry_run {
        (prospective_destination, Some(ancestor_pin))
    } else {
        drop(ancestor_pin);
        create_missing_components(&final_ancestor, &missing)?;
        let pin = open_root(&prospective_destination)
            .map_err(|error| BigcpError::io("pin created destination root", error))?;
        let resolved = final_path(&pin)
            .map_err(|error| BigcpError::io("resolve created destination root", error))?;
        if !is_same_or_descendant_with(
            &resolved,
            &final_ancestor,
            destination_endpoint.names_are_case_sensitive(),
        )
        .map_err(|error| BigcpError::io("revalidate created destination root", error))?
        {
            return Err(BigcpError::Invalid(
                "created destination escaped its pinned ancestor through a reparse point"
                    .to_owned(),
            ));
        }
        (resolved, Some(pin))
    };
    if destination_preexisted {
        let metadata = metadata_at(&destination)
            .map_err(|error| BigcpError::io("read destination root metadata", error))?;
        if metadata.kind != ObjectKind::Directory {
            return Err(BigcpError::Invalid(
                "destination must resolve to a real directory, not a file or reparse point"
                    .to_owned(),
            ));
        }
    }

    Ok(Preflight {
        source,
        destination,
        destination_exists: destination_preexisted || !options.dry_run,
        source_volume,
        destination_volume,
        destination_write_cache_disabled: destination_device.write_cache_enabled == Some(false),
        profile,
        state_dir,
        log_path,
        report_path,
        _source_pin: source_pin,
        _destination_pin: destination_pin,
        _lock: lock,
    })
}

fn audit_paths(
    options: &CopyOptions,
    source: &Path,
    destination: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf), BigcpError> {
    let key = path_pair_hash(source, destination)?;
    let state_dir = options.state_dir.clone().unwrap_or_else(|| {
        let base = std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from);
        base.join("bigcp").join("state").join(&key[..16])
    });
    let state_dir = absolute_extended(&state_dir)
        .map_err(|error| BigcpError::io("normalize state directory", error))?;
    let run_stub = Uuid::new_v4().simple().to_string();
    let log_path = absolute_extended(
        options
            .log_path
            .as_deref()
            .unwrap_or(&state_dir.join(format!("run-{run_stub}.log.jsonl"))),
    )
    .map_err(|error| BigcpError::io("normalize log path", error))?;
    let report_path = absolute_extended(
        options
            .report_path
            .as_deref()
            .unwrap_or(&state_dir.join(format!("run-{run_stub}.report.json"))),
    )
    .map_err(|error| BigcpError::io("normalize report path", error))?;
    let resolved_state = prospective_final_path(&state_dir)?;
    let resolved_log = prospective_final_path(&log_path)?;
    let resolved_report = prospective_final_path(&report_path)?;
    validate_audit_layout(&resolved_state, &resolved_log, &resolved_report)?;
    for (label, resolved) in [
        ("state directory", &resolved_state),
        ("log", &resolved_log),
        ("report", &resolved_report),
    ] {
        if is_same_or_descendant_with(
            resolved,
            source,
            classify_endpoint(source).names_are_case_sensitive(),
        )
        .map_err(|error| BigcpError::io("validate audit path", error))?
            || is_same_or_descendant_with(
                resolved,
                destination,
                classify_endpoint(destination).names_are_case_sensitive(),
            )
            .map_err(|error| BigcpError::io("validate audit path", error))?
        {
            return Err(BigcpError::Invalid(format!(
                "{label} may not be inside the source or destination tree"
            )));
        }
    }
    Ok((state_dir, log_path, report_path))
}

/// Keeps the three durable artifact roles disjoint. Log/report files may live
/// inside the state directory (the defaults do), but neither may replace its
/// container, its journal, the other artifact, or an ancestor of the state
/// directory. These checks run on paths resolved through their nearest
/// existing ancestors, so aliases cannot bypass them.
fn validate_audit_layout(state: &Path, log: &Path, report: &Path) -> Result<(), BigcpError> {
    if resolved_paths_equal(log, report)? {
        return Err(BigcpError::Invalid(
            "log and report paths must be distinct".to_owned(),
        ));
    }
    let journal = state.join("journal.jsonl");
    for (label, artifact) in [("log", log), ("report", report)] {
        if resolved_paths_equal(artifact, state)? {
            return Err(BigcpError::Invalid(format!(
                "{label} path may not replace the state directory"
            )));
        }
        if resolved_paths_equal(artifact, &journal)? {
            return Err(BigcpError::Invalid(format!(
                "{label} path may not replace the checkpoint journal"
            )));
        }
        let case_sensitive = classify_endpoint(state).names_are_case_sensitive()
            || classify_endpoint(artifact).names_are_case_sensitive();
        if is_same_or_descendant_with(state, artifact, case_sensitive)
            .map_err(|error| BigcpError::io("validate audit artifact layout", error))?
        {
            return Err(BigcpError::Invalid(format!(
                "{label} path may not be an ancestor of the state directory"
            )));
        }
    }
    Ok(())
}

fn resolved_paths_equal(left: &Path, right: &Path) -> Result<bool, BigcpError> {
    let case_sensitive = classify_endpoint(left).names_are_case_sensitive()
        || classify_endpoint(right).names_are_case_sensitive();
    Ok(comparison_key(left.as_os_str(), case_sensitive)
        .map_err(|error| BigcpError::io("compare audit artifact paths", error))?
        == comparison_key(right.as_os_str(), case_sensitive)
            .map_err(|error| BigcpError::io("compare audit artifact paths", error))?)
}

fn prospective_final_path(path: &Path) -> Result<PathBuf, BigcpError> {
    let (ancestor, missing) = nearest_existing(path)?;
    let pin =
        open_root(&ancestor).map_err(|error| BigcpError::io("open audit path ancestor", error))?;
    let resolved =
        final_path(&pin).map_err(|error| BigcpError::io("resolve audit path ancestor", error))?;
    Ok(missing
        .iter()
        .fold(resolved, |current, component| current.join(component)))
}

fn nearest_existing(path: &Path) -> Result<(PathBuf, Vec<OsString>), BigcpError> {
    let mut candidate = path.to_path_buf();
    let mut missing = Vec::new();
    while !candidate.exists() {
        let name = candidate.file_name().ok_or_else(|| {
            BigcpError::Invalid("destination has no existing ancestor".to_owned())
        })?;
        missing.push(name.to_os_string());
        if !candidate.pop() {
            return Err(BigcpError::Invalid(
                "destination has no existing ancestor".to_owned(),
            ));
        }
    }
    missing.reverse();
    Ok((candidate, missing))
}

fn create_missing_components(base: &Path, missing: &[OsString]) -> Result<(), BigcpError> {
    let mut current = base.to_path_buf();
    for component in missing {
        current.push(component);
        create_directory(&current)
            .map_err(|error| BigcpError::io("create destination root component", error))?;
        let metadata = metadata_at(&current)
            .map_err(|error| BigcpError::io("verify destination root component", error))?;
        if metadata.kind != ObjectKind::Directory {
            return Err(BigcpError::Invalid(format!(
                "destination component is not a real directory: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn destination_join(
    path: &Path,
    policy: FilesystemPolicy,
) -> std::io::Result<HashMap<Vec<u16>, DirectoryEntry>> {
    let mut map = HashMap::new();
    for entry in enumerate_directory(path)? {
        let key = comparison_key(&entry.name, policy.names_are_case_sensitive())?;
        if map.insert(key, entry).is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "destination contains duplicate names under its matching semantics",
            ));
        }
    }
    Ok(map)
}

fn path_pair_hash(source: &Path, destination: &Path) -> Result<String, BigcpError> {
    path_hash_parts([source, destination])
}

fn path_hash(path: &Path) -> Result<String, BigcpError> {
    path_hash_parts([path])
}

fn path_hash_parts<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Result<String, BigcpError> {
    let mut hasher = Sha256::new();
    for path in paths {
        let folded = comparison_key(
            path.as_os_str(),
            classify_endpoint(path).names_are_case_sensitive(),
        )
        .map_err(|error| BigcpError::io("case-fold path lock identity", error))?;
        for code_unit in folded {
            hasher.update(code_unit.to_le_bytes());
        }
        hasher.update([0_u8, 0_u8]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn same_path(left: &Path, right: &Path) -> Result<bool, BigcpError> {
    let left = absolute_extended(left).map_err(|error| BigcpError::io("normalize path", error))?;
    let right =
        absolute_extended(right).map_err(|error| BigcpError::io("normalize path", error))?;
    Ok(comparison_key(
        left.as_os_str(),
        classify_endpoint(&left).names_are_case_sensitive(),
    )
    .map_err(|error| BigcpError::io("compare path", error))?
        == comparison_key(
            right.as_os_str(),
            classify_endpoint(&right).names_are_case_sensitive(),
        )
        .map_err(|error| BigcpError::io("compare path", error))?)
}

fn format_time(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

fn derive_hints(runner: &Runner<'_>, preflight: &Preflight) -> Vec<Hint> {
    let mut hints = Vec::new();
    if preflight.source_volume.endpoint.is_remote()
        || preflight.destination_volume.endpoint.is_remote()
    {
        hints.push(Hint {
            id: "remote_endpoint".to_owned(),
            text: format!(
                "Remote endpoint active (source: {}, destination: {}). Re-run after any disconnect; --verify checks the completed tree but cannot force or attest server-side cache durability",
                preflight.source_volume.endpoint.name(),
                preflight.destination_volume.endpoint.name()
            ),
            confidence: "high".to_owned(),
        });
    }
    if preflight.source_volume.endpoint == bigcp_win::EndpointKind::Wsl
        || preflight.destination_volume.endpoint == bigcp_win::EndpointKind::Wsl
    {
        hints.push(Hint {
            id: "wsl_interop".to_owned(),
            text: "WSL UNC access crosses the Windows/Linux 9P boundary. Native Linux copy tools remain faster for sustained Linux-filesystem work; review projected metadata and unsupported reparse failures"
                .to_owned(),
            confidence: "high".to_owned(),
        });
    }
    if preflight.destination_volume.filesystem.is_fat_family() {
        hints.push(Hint {
            id: "degraded_filesystem".to_owned(),
            text: format!(
                "{} cannot preserve NTFS/ReFS-only metadata. Review streams_dropped, ea_dropped, fs_limit, and link failures; use NTFS when full fidelity is required",
                preflight.destination_volume.filesystem.name()
            ),
            confidence: "high".to_owned(),
        });
    }
    if preflight.destination_write_cache_disabled {
        hints.push(Hint {
            id: "quick_removal_destination".to_owned(),
            text: "Destination write caching is off (Quick-removal policy). Switching the drive to \
                   'Better performance' with 'Enable write caching on the device' checked sped this \
                   class of workload ~3.4x in measurement; leave 'Turn off Windows write-cache \
                   buffer flushing' UNCHECKED, and always use Safely Remove before unplugging"
                .to_owned(),
            confidence: "high".to_owned(),
        });
    }
    if preflight.profile.transport.is_same_spindle() {
        hints.push(Hint {
            id: "same_spindle_transport".to_owned(),
            text: format!(
                "Source and destination share rotational media; the {} MiB phased transport was active. A destination on a separate physical drive can still be faster because one spindle cannot read and write simultaneously",
                preflight.profile.transport.burst_bytes / (1024 * 1024)
            ),
            confidence: "high".to_owned(),
        });
    }
    if runner.counters.failed > 0 {
        hints.push(Hint {
            id: "rerun_failures".to_owned(),
            text: "Resolve the grouped errors, then run the same command again; completed files will skip"
                .to_owned(),
            confidence: "high".to_owned(),
        });
    }
    if preflight.source_volume.filesystem == FileSystem::Refs
        && preflight.destination_volume.filesystem == FileSystem::Refs
        && preflight.source_volume.serial == preflight.destination_volume.serial
        && preflight.destination_volume.capabilities.block_refcounting
    {
        hints.push(Hint {
            id: "refs_clone".to_owned(),
            text:
                "This is a same-volume ReFS copy; the OS copy engine may block-clone it much faster"
                    .to_owned(),
            confidence: "high".to_owned(),
        });
    }
    hints
}

fn summarize_phases(points: &[crate::stats::TimelinePoint]) -> PhaseSummary {
    let active: Vec<_> = points
        .iter()
        .filter(|point| point.read_mbps > 0.0 || point.write_mbps > 0.0)
        .collect();
    let fastest = active
        .iter()
        .max_by(|left, right| left.write_mbps.total_cmp(&right.write_mbps))
        .map(|point| (*point).clone());
    let slowest = active
        .iter()
        .min_by(|left, right| left.write_mbps.total_cmp(&right.write_mbps))
        .map(|point| (*point).clone());
    PhaseSummary { fastest, slowest }
}

fn completed_exit(
    canceled: bool,
    breaker_tripped: bool,
    counters: &Counters,
    verification: Option<&crate::report::VerificationSummary>,
) -> i32 {
    // The breaker outranks cancellation: exit 4 names the actionable cause
    // (reconnect the device or free space, then rerun to resume).
    if breaker_tripped {
        4
    } else if canceled {
        3
    } else if counters.failed > 0
        || counters.dirs_failed > 0
        || counters.dirs_meta_failed > 0
        || counters.links_failed > 0
        || verification.is_some_and(|summary| summary.failed > 0)
    {
        2
    } else {
        0
    }
}

fn repair_metadata(
    source: &DirectoryEntry,
    destination: &DirectoryEntry,
    differences: &[&str],
    relative: &Path,
    destination_policy: FilesystemPolicy,
    source_supports_eas: bool,
) -> Result<(), OperationError> {
    revalidate_source(source, relative)?;
    let source_eas = if differences.contains(&"ea_size") {
        Some(if source_supports_eas {
            read_extended_attributes_checked(&source.path, source.metadata.identity)
                .map_err(|error| source_access_error("read_ea", relative, &error))?
        } else {
            bigcp_win::ExtendedAttributes::empty()
        })
    } else {
        None
    };
    revalidate_source(source, relative)?;
    revalidate_destination(destination, relative)?;
    if let Some(source_eas) = source_eas {
        clear_extended_attributes_checked(&destination.path, destination.metadata.identity)
            .map_err(|error| destination_access_error("clear_ea", relative, &error))?;
        if !source_eas.is_empty() {
            write_extended_attributes_checked(
                &destination.path,
                destination.metadata.identity,
                &source_eas,
            )
            .map_err(|error| destination_access_error("write_ea", relative, &error))?;
        }
    }
    set_basic_at_checked(
        &destination.path,
        destination.metadata.identity,
        destination_policy.project_basic(source.metadata.basic),
    )
    .map_err(|error| destination_access_error("set_meta", relative, &error))
}

fn same_directory_source(observed: &ObjectMetadata, expected: &ObjectMetadata) -> bool {
    observed.identity == expected.identity
        && observed.kind == ObjectKind::Directory
        && observed.basic.last_write_time == expected.basic.last_write_time
        && observed.basic.attributes == expected.basic.attributes
        && observed.reparse_tag == expected.reparse_tag
}

fn source_access_error(operation: &str, relative: &Path, error: &std::io::Error) -> OperationError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::InvalidData
    ) {
        OperationError::from_io_as(
            ErrorCategory::SourceChanged,
            operation,
            relative.to_path_buf(),
            error,
        )
    } else {
        OperationError::from_io(operation, relative.to_path_buf(), error)
    }
}

fn destination_access_error(
    operation: &str,
    relative: &Path,
    error: &std::io::Error,
) -> OperationError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::AlreadyExists
            | std::io::ErrorKind::InvalidData
    ) {
        OperationError::from_io_as(
            ErrorCategory::DestinationChanged,
            operation,
            relative.to_path_buf(),
            error,
        )
    } else {
        OperationError::from_io(operation, relative.to_path_buf(), error)
    }
}

fn revalidate_source(source: &DirectoryEntry, relative: &Path) -> Result<(), OperationError> {
    let observed = match metadata_at(&source.path) {
        Ok(observed) => observed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(OperationError::from_io_as(
                ErrorCategory::SourceChanged,
                "revalidate_src",
                relative.to_path_buf(),
                &error,
            ));
        }
        Err(error) => {
            return Err(OperationError::from_io(
                "revalidate_src",
                relative.to_path_buf(),
                &error,
            ));
        }
    };
    let expected = &source.metadata;
    if observed.identity != expected.identity
        || observed.kind != expected.kind
        || observed.size != expected.size
        || observed.basic.last_write_time != expected.basic.last_write_time
        || observed.basic.attributes != expected.basic.attributes
        || observed.reparse_tag != expected.reparse_tag
    {
        return Err(OperationError::semantic(
            ErrorCategory::SourceChanged,
            "revalidate_src",
            relative.to_path_buf(),
            "source changed after enumeration",
        ));
    }
    Ok(())
}

fn revalidate_destination(
    destination: &DirectoryEntry,
    relative: &Path,
) -> Result<(), OperationError> {
    let observed = match metadata_at(&destination.path) {
        Ok(observed) => observed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(OperationError::semantic(
                ErrorCategory::DestinationChanged,
                "revalidate_dst",
                relative.to_path_buf(),
                "destination disappeared after classification",
            ));
        }
        Err(error) => {
            return Err(OperationError::from_io(
                "revalidate_dst",
                relative.to_path_buf(),
                &error,
            ));
        }
    };
    let expected = &destination.metadata;
    if observed.identity != expected.identity
        || observed.kind != expected.kind
        || observed.size != expected.size
        || observed.basic.last_write_time != expected.basic.last_write_time
        || observed.basic.attributes != expected.basic.attributes
        || observed.reparse_tag != expected.reparse_tag
    {
        return Err(OperationError::semantic(
            ErrorCategory::DestinationChanged,
            "revalidate_dst",
            relative.to_path_buf(),
            "destination changed after classification",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        WorkerDispatch, completed_exit, path_hash, summarize_phases, validate_audit_layout,
        worker_dispatch,
    };
    use crate::model::Counters;
    use crate::report::VerificationSummary;
    use crate::stats::TimelinePoint;
    use crate::transport::TransportProfile;

    #[test]
    fn redirector_dispatches_streamed_files_but_keeps_checkpoint_owners_inline() {
        const MIB: u64 = 1024 * 1024;
        const MIB_USIZE: usize = 1024 * 1024;
        let transport = TransportProfile::redirector(8 * MIB_USIZE);
        assert_eq!(
            worker_dispatch(transport, false, true, 64 * MIB, 16 * MIB, 256 * MIB, false,),
            Some(WorkerDispatch {
                promote_threshold: Some(256 * MIB),
                directory_affine: false,
            })
        );
        assert_eq!(
            worker_dispatch(
                transport,
                false,
                true,
                256 * MIB,
                16 * MIB,
                256 * MIB,
                false,
            ),
            None
        );
        assert_eq!(
            worker_dispatch(transport, false, true, 8 * MIB, 16 * MIB, 256 * MIB, true,),
            None
        );
    }

    #[test]
    fn standard_transport_dispatch_remains_small_file_only() {
        const MIB: u64 = 1024 * 1024;
        const MIB_USIZE: usize = 1024 * 1024;
        let transport = TransportProfile::standard(8 * MIB_USIZE);
        assert!(
            worker_dispatch(transport, false, true, 8 * MIB, 16 * MIB, 256 * MIB, false,).is_some()
        );
        assert_eq!(
            worker_dispatch(transport, false, true, 64 * MIB, 16 * MIB, 256 * MIB, false,),
            None
        );
    }

    #[test]
    fn wsl_destination_stripes_small_files_without_changing_other_affinity() {
        const MIB: u64 = 1024 * 1024;
        const MIB_USIZE: usize = 1024 * 1024;
        let transport = TransportProfile::wsl(8 * MIB_USIZE);
        let wsl = worker_dispatch(transport, true, true, 8 * MIB, 16 * MIB, 256 * MIB, false);
        assert!(wsl.is_some_and(|dispatch| !dispatch.directory_affine));

        let local_destination =
            worker_dispatch(transport, false, true, 8 * MIB, 16 * MIB, 256 * MIB, false);
        assert!(local_destination.is_some_and(|dispatch| dispatch.directory_affine));
    }

    #[test]
    fn every_completed_failure_universe_controls_the_exit_code() {
        assert_eq!(completed_exit(true, false, &Counters::default(), None), 3);
        // The breaker outranks cancellation and per-object failures: exit 4
        // names the actionable cause.
        assert_eq!(completed_exit(false, true, &Counters::default(), None), 4);
        assert_eq!(completed_exit(true, true, &Counters::default(), None), 4);
        assert_eq!(
            completed_exit(
                false,
                true,
                &Counters {
                    failed: 1,
                    ..Counters::default()
                },
                None
            ),
            4
        );

        for counters in [
            Counters {
                failed: 1,
                ..Counters::default()
            },
            Counters {
                dirs_failed: 1,
                ..Counters::default()
            },
            Counters {
                dirs_meta_failed: 1,
                ..Counters::default()
            },
            Counters {
                links_failed: 1,
                ..Counters::default()
            },
        ] {
            assert_eq!(completed_exit(false, false, &counters, None), 2);
        }

        let verification = VerificationSummary {
            failed: 1,
            ..VerificationSummary::default()
        };
        assert_eq!(
            completed_exit(false, false, &Counters::default(), Some(&verification)),
            2
        );
        assert_eq!(completed_exit(false, false, &Counters::default(), None), 0);
    }

    #[test]
    fn destination_lock_identity_is_case_insensitive() {
        let upper = path_hash(Path::new(r"\\?\C:\CopyTarget"));
        let lower = path_hash(Path::new(r"\\?\c:\copytarget"));
        assert!(upper.is_ok());
        assert_eq!(upper.ok(), lower.ok());
    }

    #[test]
    fn audit_artifact_roles_cannot_collide() {
        let state = Path::new(r"\\?\C:\safe\state");
        let log = state.join("run.log.jsonl");
        let report = state.join("run.report.json");
        assert!(validate_audit_layout(state, &log, &report).is_ok());
        assert!(validate_audit_layout(state, &log, &log).is_err());
        assert!(validate_audit_layout(state, &log, &state.join("journal.jsonl")).is_err());
        assert!(validate_audit_layout(state, Path::new(r"\\?\C:\safe"), &report).is_err());
    }

    #[test]
    fn fastest_and_slowest_phases_ignore_inactive_windows() {
        let point = |seconds, write_mbps| TimelinePoint {
            seconds,
            read_mbps: write_mbps,
            write_mbps,
            files_per_second: 1.0,
            hypothesis: "balanced".to_owned(),
        };
        let phases = summarize_phases(&[point(1.0, 0.0), point(2.0, 10.0), point(3.0, 5.0)]);
        assert_eq!(
            phases.fastest.as_ref().map(|value| value.write_mbps),
            Some(10.0)
        );
        assert_eq!(
            phases.slowest.as_ref().map(|value| value.write_mbps),
            Some(5.0)
        );
    }
}
