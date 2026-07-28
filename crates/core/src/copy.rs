//! End-to-end copy orchestration and iterative directory join.
//!
//! Pre-flight resolves and pins both roots, rejects remote or unsupported
//! volumes, validates audit-path containment, and acquires the exact-root
//! machine-wide lock before enumeration begins.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bigcp_win::{
    DestinationLock, DestinationStream, DirectoryEntry, FileSystem, ObjectKind, ObjectMetadata,
    SourceStream, VolumeInfo, absolute_extended, clear_extended_attributes, copy_reparse,
    create_directory, display_path, enumerate_directory, final_path, is_cloud_placeholder,
    is_compressed, is_same_or_descendant, list_streams, metadata_at, open_root, ordinal_case_key,
    probe_volume, read_extended_attributes, set_basic_at, write_extended_attributes,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::REPORT_SCHEMA_VERSION;
use crate::audit::{AuditEvent, AuditPath, AuditWriter, ReplacementEvent, option_summary};
use crate::classify::classify;
use crate::devprofile::{CopyProfile, select_copy_profile};
use crate::engine::{EngineRequest, copy_file};
use crate::error::{BigcpError, ErrorCategory, OperationError};
use crate::journal::{Journal, JournalEvent, path_key};
use crate::model::{Classification, Counters, EntrySnapshot, FileOutcome, RunSnapshot, RunState};
use crate::options::CopyOptions;
use crate::report::{
    BottleneckSummary, ErrorSummary, ExtraSummary, FolderSummary, Hint, PhaseSummary,
    ReplacementSample, ReplacementSummary, RunInfo, RunReport, top_level,
};
use crate::stats::StatsTracker;
use crate::verify::{VerificationTarget, verify_written_targets};
use crate::worker::{CompletedCopy, FileCopyJob, ReplacementWork, SmallFileWorkers};

const REPORT_SAMPLE_LIMIT: usize = 100;
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
    let run_id = Uuid::new_v4().simple().to_string();
    let started_at = OffsetDateTime::now_utc();
    let started = format_time(started_at);

    let fallback_log = preflight
        .state_dir
        .join(format!("run-{run_id}-fallback.log.jsonl"));
    let mut audit = AuditWriter::create(preflight.log_path.clone(), fallback_log)?;
    audit.emit(&AuditEvent::RunStart {
        v: 1,
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
        qd_source: preflight.profile.source.queue_depth,
        qd_destination: preflight.profile.destination.queue_depth,
        chunk_bytes: preflight.profile.chunk_bytes,
        streams: preflight.profile.streams,
        workers: preflight.profile.workers,
        same_physical_disk: preflight.profile.same_physical_disk,
    })?;

    let journal = match Journal::open(preflight.state_dir.join("journal.jsonl"), options.fresh) {
        Ok(mut journal) => {
            if let Err(error) = journal.begin_job(
                run_id.clone(),
                preflight.source.to_string_lossy().into_owned(),
                preflight.destination.to_string_lossy().into_owned(),
                value_hash(&option_summary(options)),
                started.clone(),
            ) {
                audit.emit(&AuditEvent::Warning {
                    kind: "checkpointing_disabled".to_owned(),
                    rel: None,
                    message: format!("journal job header could not be written: {error}"),
                })?;
                None
            } else {
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
    };

    let root_metadata = metadata_at(&preflight.source)
        .map_err(|error| BigcpError::io("read source root metadata", error))?;
    if root_metadata.kind != ObjectKind::Directory {
        return Err(BigcpError::Invalid(
            "source must resolve to a real directory, not a file or reparse point".to_owned(),
        ));
    }

    let small_workers = SmallFileWorkers::new(preflight.profile.workers)?;
    let mut runner = Runner {
        options,
        observer,
        run_id: &run_id,
        source_root: &preflight.source,
        destination_root: &preflight.destination,
        destination_supports_streams: preflight.destination_volume.capabilities.named_streams,
        destination_supports_eas: preflight
            .destination_volume
            .capabilities
            .extended_attributes,
        destination_supports_sparse: preflight.destination_volume.capabilities.sparse_files,
        destination_supports_encryption: preflight.destination_volume.capabilities.encryption,
        chunk_bytes: preflight.profile.chunk_bytes,
        source_is_volume_root: same_path(&preflight.source, &preflight.source_volume.root)?,
        counters: Counters::default(),
        audit,
        stats: StatsTracker::new(),
        errors: BTreeMap::new(),
        warnings: BTreeMap::new(),
        replacements: ReplacementSummary::default(),
        extras: ExtraSummary::default(),
        folders: BTreeMap::new(),
        verification_targets: Vec::new(),
        journal,
        canceled: false,
        small_workers,
        small_jobs_outstanding: 0,
        last_snapshot: Instant::now(),
    };
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
        ))
    } else {
        None
    };

    let integrity = match runner
        .counters
        .reconcile()
        .and_then(|()| runner.reconcile_folders())
    {
        Ok(()) => "ok".to_owned(),
        Err(message) => {
            runner.audit.emit(&AuditEvent::Warning {
                kind: "counter_invariant".to_owned(),
                rel: None,
                message: message.clone(),
            })?;
            return Err(BigcpError::Invariant(message));
        }
    };
    let exit = completed_exit(runner.canceled, &runner.counters, verify.as_ref());
    let audit_status = if runner.audit.degraded() {
        "degraded"
    } else {
        "ok"
    };
    runner.audit.emit(&AuditEvent::RunEnd {
        counters: runner.counters.clone(),
        durability: if options.flush {
            "durable".to_owned()
        } else {
            "logical".to_owned()
        },
        audit: audit_status.to_owned(),
        integrity: integrity.clone(),
        exit,
    })?;
    runner.audit.flush()?;
    if let Some(journal) = &mut runner.journal {
        let _ = journal.append(JournalEvent::End {
            run_id: run_id.clone(),
        });
    }

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
        runner.audit.flush()?;
        report.write_atomic(&fallback).map_err(|fallback_error| {
            BigcpError::Audit(format!(
                "report failed at {} ({primary_error}) and fallback {} failed ({fallback_error})",
                report_path.display(),
                fallback.display()
            ))
        })?;
    }
    Ok(report)
}

struct Runner<'a> {
    options: &'a CopyOptions,
    observer: &'a dyn RunObserver,
    run_id: &'a str,
    source_root: &'a Path,
    destination_root: &'a Path,
    destination_supports_streams: bool,
    destination_supports_eas: bool,
    destination_supports_sparse: bool,
    destination_supports_encryption: bool,
    chunk_bytes: usize,
    source_is_volume_root: bool,
    counters: Counters,
    audit: AuditWriter,
    stats: StatsTracker,
    errors: BTreeMap<ErrorCategory, ErrorSummary>,
    warnings: BTreeMap<String, u64>,
    replacements: ReplacementSummary,
    extras: ExtraSummary,
    folders: BTreeMap<String, FolderSummary>,
    verification_targets: Vec<VerificationTarget>,
    journal: Option<Journal>,
    canceled: bool,
    small_workers: SmallFileWorkers,
    small_jobs_outstanding: usize,
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
        let mut tasks = vec![DirectoryTask::Enter {
            source: self.source_root.to_path_buf(),
            destination: self.destination_root.to_path_buf(),
            relative: PathBuf::new(),
            source_metadata: root_metadata,
            destination_exists,
            destination_metadata,
        }];
        while let Some(task) = tasks.pop() {
            if !self.canceled && self.observer.cancellation_requested() {
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
            match task {
                DirectoryTask::Enter {
                    source,
                    destination,
                    relative,
                    source_metadata,
                    destination_exists,
                    destination_metadata,
                } => {
                    if !self.canceled {
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
                        self.record_error(OperationError::semantic(
                            ErrorCategory::ParentDirFailed,
                            "cancel_before_enumerate",
                            relative,
                            "directory was discovered but not traversed after cancellation",
                        ))?;
                    }
                }
                DirectoryTask::Exit {
                    source,
                    destination,
                    relative,
                    source_metadata,
                    destination_exists,
                    destination_metadata,
                } => self.exit_directory(
                    &source,
                    &destination,
                    &relative,
                    &source_metadata,
                    destination_exists,
                    destination_metadata.as_ref(),
                )?,
            }
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
        tasks: &mut Vec<DirectoryTask>,
    ) -> Result<(), BigcpError> {
        self.counters.dirs_discovered = self.counters.dirs_discovered.saturating_add(1);
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
            match destination_join(&destination) {
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
        let mut seen_source = HashSet::new();
        let mut child_directories = Vec::new();

        for source_entry in source_entries {
            let child_relative = relative.join(&source_entry.name);
            let key = match ordinal_case_key(&source_entry.name) {
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
                        "source directory contains names that collide under Windows case-insensitive matching",
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
        }
        self.drain_small_workers()?;
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

        tasks.push(DirectoryTask::Exit {
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
                        (true, None)
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
            tasks.push(DirectoryTask::Enter {
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
        if self.options.dry_run || !destination_exists {
            if self.options.dry_run {
                self.counters.dirs_planned = self.counters.dirs_planned.saturating_add(1);
            } else {
                self.counters.dir_done = self.counters.dir_done.saturating_add(1);
            }
            return Ok(());
        }
        // Directory last-write updates can be committed lazily by NTFS after
        // child creation handles close. Re-read at post-order finalization so
        // the destination receives the stable value, while still requiring
        // the enumerated directory identity to remain unchanged.
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
                self.record_error(OperationError::from_io(
                    "revalidate_dir",
                    relative.to_path_buf(),
                    &error,
                ))?;
                return Ok(());
            }
        };
        let stream_result = self.copy_directory_streams(source, destination, relative);
        let ea_result = if source_metadata.ea_size > 0 && !self.destination_supports_eas {
            self.increment_warning("ea_dropped");
            self.audit.emit(&AuditEvent::Warning {
                kind: "ea_dropped".to_owned(),
                rel: Some(AuditPath::from_path(relative)),
                message: "destination volume does not advertise directory EA support".to_owned(),
            })?;
            stream_result
        } else {
            stream_result.and_then(|()| {
                if source_metadata.ea_size > 0
                    || destination_metadata.is_some_and(|metadata| metadata.ea_size > 0)
                    || relative.as_os_str().is_empty()
                {
                    read_extended_attributes(source).and_then(|source_eas| {
                        let destination_eas = read_extended_attributes(destination)?;
                        if source_eas == destination_eas {
                            Ok(())
                        } else {
                            if !destination_eas.is_empty() {
                                clear_extended_attributes(destination)?;
                            }
                            if source_eas.is_empty() {
                                Ok(())
                            } else {
                                write_extended_attributes(destination, &source_eas)
                            }
                        }
                    })
                } else {
                    Ok(())
                }
            })
        };
        let metadata_result =
            ea_result.and_then(|()| set_basic_at(destination, current_source_metadata.basic));
        match metadata_result {
            Ok(()) => {
                self.counters.dir_done = self.counters.dir_done.saturating_add(1);
                self.audit.emit(&AuditEvent::Directory {
                    action: "stamped".to_owned(),
                    rel: AuditPath::from_path(relative),
                })?;
            }
            Err(error) => {
                self.counters.dirs_meta_failed = self.counters.dirs_meta_failed.saturating_add(1);
                self.record_error(OperationError::from_io(
                    "set_dir_meta",
                    relative.to_path_buf(),
                    &error,
                ))?;
            }
        }
        Ok(())
    }

    fn copy_directory_streams(
        &mut self,
        source: &Path,
        destination: &Path,
        relative: &Path,
    ) -> std::io::Result<()> {
        let streams = list_streams(source)?;
        let named: Vec<_> = streams
            .iter()
            .filter(|stream| !stream.is_unnamed())
            .collect();
        if !self.destination_supports_streams {
            for _ in named {
                self.increment_warning("streams_dropped");
            }
            return Ok(());
        }
        let mut buffer = vec![0_u8; self.chunk_bytes.clamp(64 * 1024, 8 * 1024 * 1024)];
        for stream in named {
            let mut input = SourceStream::open(source, stream)?;
            let mut output = DestinationStream::create(destination, stream, true)?;
            let mut copied = 0_u64;
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
                copied = copied.saturating_add(count as u64);
                self.counters.bytes_read_source =
                    self.counters.bytes_read_source.saturating_add(count as u64);
                self.counters.bytes_written_destination = self
                    .counters
                    .bytes_written_destination
                    .saturating_add(count as u64);
            }
            if copied != stream.size {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "directory stream {} at {} changed size during copy",
                        stream.name.to_string_lossy(),
                        relative.display()
                    ),
                ));
            }
            output.flush()?;
        }
        Ok(())
    }

    fn handle_file(
        &mut self,
        source: DirectoryEntry,
        destination: Option<DirectoryEntry>,
        relative: PathBuf,
    ) -> Result<(), BigcpError> {
        if is_compressed(source.metadata.basic.attributes) {
            self.increment_warning("compressed_sources");
        }
        if is_cloud_placeholder(source.metadata.basic.attributes) {
            if self.options.skip_cloud {
                self.record_file(
                    &relative,
                    FileOutcome::Excluded {
                        bytes: source.metadata.size,
                        reason: "cloud_placeholder".to_owned(),
                    },
                    None,
                )?;
                return Ok(());
            }
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
        ) {
            Classification::New => {
                self.copy_classified(&source, &relative, &source_snapshot, None, None)?;
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
                    match repair_file_metadata(&source, &destination, &fields) {
                        Ok(()) => FileOutcome::MetadataFixed {
                            bytes: source.metadata.size,
                        },
                        Err(error) => FileOutcome::Failed {
                            bytes: source.metadata.size,
                            error: OperationError::from_io("set_meta", relative.clone(), &error),
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
                )?;
            }
            Classification::SkipDifferent { fields, .. } => {
                self.record_file(
                    &relative,
                    FileOutcome::SkippedDifferent {
                        bytes: source.metadata.size,
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
                bytes: source.metadata.size,
                reason: reason.to_owned(),
            };
            return self.record_file(relative, outcome, replacement.map(|(fields, _)| fields));
        }
        let destination_path = self.destination_root.join(relative);
        let streams = match list_streams(&source.path) {
            Ok(streams) => streams,
            Err(error) => {
                return self.record_file(
                    relative,
                    FileOutcome::Failed {
                        bytes: source.metadata.size,
                        error: OperationError::from_io(
                            "list_streams",
                            relative.to_path_buf(),
                            &error,
                        ),
                    },
                    replacement.map(|(fields, _)| fields),
                );
            }
        };
        let largest_stream = streams
            .iter()
            .map(|stream| stream.size)
            .max()
            .unwrap_or(source.metadata.size);
        let replacement = replacement.map(|(fields, destination_newer)| ReplacementWork {
            fields,
            destination_newer,
        });
        if largest_stream < self.options.large_threshold() {
            let job = FileCopyJob {
                source_path: source.path.clone(),
                destination_path,
                source_snapshot: source_snapshot.clone(),
                destination_snapshot: destination_snapshot.cloned(),
                replacement,
                run_id: self.run_id.to_owned(),
                large_threshold: self.options.large_threshold(),
                verify: self.options.verify,
                flush: self.options.flush,
                destination_supports_streams: self.destination_supports_streams,
                destination_supports_eas: self.destination_supports_eas,
                destination_supports_encryption: self.destination_supports_encryption,
                chunk_bytes: self.chunk_bytes,
                checkpoint_threshold: self.options.checkpoint_threshold(),
                streams,
            };
            return self.submit_small_job(job);
        }

        let mut counters = Counters::default();
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
            destination_supports_streams: self.destination_supports_streams,
            destination_supports_eas: self.destination_supports_eas,
            chunk_bytes: self.chunk_bytes,
            preserve_sparse: self.destination_supports_sparse && !self.options.no_sparse,
            checkpoint_threshold: self.options.checkpoint_threshold(),
            destination_supports_encryption: self.destination_supports_encryption,
            known_streams: Some(&streams),
        };
        let result = copy_file(&request, &mut counters, self.journal.as_mut());
        self.finish_copy(CompletedCopy {
            destination_path,
            source_snapshot: source_snapshot.clone(),
            destination_snapshot: destination_snapshot.cloned(),
            replacement,
            counters,
            result,
        })
    }

    fn submit_small_job(&mut self, job: FileCopyJob) -> Result<(), BigcpError> {
        if self.small_jobs_outstanding >= self.small_workers.capacity() {
            self.receive_small_job()?;
        }
        self.small_workers.submit(job)?;
        self.small_jobs_outstanding = self.small_jobs_outstanding.saturating_add(1);
        Ok(())
    }

    fn receive_small_job(&mut self) -> Result<(), BigcpError> {
        let completed = self.small_workers.receive()?;
        self.small_jobs_outstanding = self.small_jobs_outstanding.saturating_sub(1);
        self.finish_copy(completed)
    }

    fn drain_small_workers(&mut self) -> Result<(), BigcpError> {
        while self.small_jobs_outstanding > 0 {
            self.receive_small_job()?;
        }
        Ok(())
    }

    fn finish_copy(&mut self, completed: CompletedCopy) -> Result<(), BigcpError> {
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
                        message: "checkpoint journal append failed; this run continues without further resume checkpoints".to_owned(),
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
                            expected_metadata: completed.source_snapshot.metadata.basic,
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
            Err(error) => FileOutcome::Failed {
                bytes: completed.source_snapshot.metadata.size,
                error,
            },
        };
        self.record_file(relative, outcome, differences)
    }

    fn handle_reparse(
        &mut self,
        source: DirectoryEntry,
        destination: Option<DirectoryEntry>,
        relative: PathBuf,
    ) -> Result<(), BigcpError> {
        self.counters.links_discovered = self.counters.links_discovered.saturating_add(1);
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
        ) {
            Classification::Same => {
                self.counters.links_skipped = self.counters.links_skipped.saturating_add(1);
                self.audit.emit(&AuditEvent::File {
                    action: "skipped".to_owned(),
                    rel: AuditPath::from_path(&relative),
                    size: 0,
                    hash: None,
                    replacement: None,
                    error: None,
                    reason: Some("same".to_owned()),
                })?;
            }
            Classification::MetadataDiff(_) => {
                if self.options.dry_run {
                    self.counters.links_planned = self.counters.links_planned.saturating_add(1);
                } else if destination
                    .as_ref()
                    .is_some_and(|entry| set_basic_at(&entry.path, source.metadata.basic).is_ok())
                {
                    self.counters.links_copied = self.counters.links_copied.saturating_add(1);
                } else {
                    self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                    self.record_error(OperationError::semantic(
                        ErrorCategory::Internal,
                        "set_link_meta",
                        relative,
                        "could not repair reparse-point metadata",
                    ))?;
                }
            }
            Classification::SkipDifferent { .. } => {
                self.counters.links_not_attempted =
                    self.counters.links_not_attempted.saturating_add(1);
            }
            Classification::TypeConflict => {
                self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                self.record_error(OperationError::semantic(
                    ErrorCategory::TypeConflict,
                    "join",
                    relative,
                    "destination object type conflicts with source reparse point",
                ))?;
            }
            Classification::New | Classification::Replace { .. } => {
                if self.options.dry_run {
                    self.counters.links_planned = self.counters.links_planned.saturating_add(1);
                    return Ok(());
                }
                let mut protected_dacl = None;
                if let Some(expected) = destination_snapshot.as_ref() {
                    let destination_path = self.destination_root.join(&relative);
                    match metadata_at(&destination_path) {
                        Ok(observed)
                            if observed.identity == expected.metadata.identity
                                && observed.basic.last_write_time
                                    == expected.metadata.basic.last_write_time => {}
                        Ok(_) | Err(_) => {
                            self.counters.links_failed =
                                self.counters.links_failed.saturating_add(1);
                            self.record_error(OperationError::semantic(
                                ErrorCategory::DestinationChanged,
                                "revalidate_dst",
                                relative,
                                "destination reparse point changed after classification",
                            ))?;
                            return Ok(());
                        }
                    }
                    match bigcp_win::read_protected_dacl(&destination_path) {
                        Ok(value) => protected_dacl = value,
                        Err(error) => {
                            self.counters.links_failed =
                                self.counters.links_failed.saturating_add(1);
                            self.record_error(OperationError::from_io(
                                "read_dacl",
                                relative,
                                &error,
                            ))?;
                            return Ok(());
                        }
                    }
                }
                let destination_path = self.destination_root.join(&relative);
                match copy_reparse(
                    &source.path,
                    &destination_path,
                    self.run_id,
                    destination_snapshot.is_some(),
                    source.metadata.basic,
                    self.options.raw_reparse,
                    self.options.flush,
                    protected_dacl.as_ref(),
                ) {
                    Ok(_) => {
                        self.counters.links_copied = self.counters.links_copied.saturating_add(1);
                        self.audit.emit(&AuditEvent::File {
                            action: "copied_link".to_owned(),
                            rel: AuditPath::from_path(&relative),
                            size: 0,
                            hash: None,
                            replacement: None,
                            error: None,
                            reason: None,
                        })?;
                    }
                    Err(error) => {
                        self.counters.links_failed = self.counters.links_failed.saturating_add(1);
                        let category = if error.kind() == std::io::ErrorKind::Unsupported {
                            ErrorCategory::UnsupportedReparse
                        } else {
                            ErrorCategory::Internal
                        };
                        self.record_error(OperationError::semantic(
                            category,
                            "copy_reparse",
                            relative,
                            error.to_string(),
                        ))?;
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
        let mut directories = vec![(source_root.to_path_buf(), relative_root.to_path_buf())];
        while let Some((source, relative)) = directories.pop() {
            let Ok(entries) = enumerate_directory(&source) else {
                continue;
            };
            for entry in entries {
                let child_relative = relative.join(&entry.name);
                match entry.metadata.kind {
                    ObjectKind::File => {
                        self.record_file(
                            &child_relative,
                            FileOutcome::NotAttempted {
                                bytes: entry.metadata.size,
                                reason: "parent_dir_failed".to_owned(),
                            },
                            None,
                        )?;
                    }
                    ObjectKind::Directory => {
                        self.counters.dirs_discovered =
                            self.counters.dirs_discovered.saturating_add(1);
                        self.counters.dirs_failed = self.counters.dirs_failed.saturating_add(1);
                        directories.push((entry.path, child_relative));
                    }
                    ObjectKind::Reparse => {
                        self.counters.links_discovered =
                            self.counters.links_discovered.saturating_add(1);
                        self.counters.links_not_attempted =
                            self.counters.links_not_attempted.saturating_add(1);
                    }
                }
            }
        }
        Ok(())
    }

    fn account_children_not_attempted(
        &mut self,
        entries: &[DirectoryEntry],
        parent_relative: &Path,
    ) -> Result<(), BigcpError> {
        for entry in entries {
            let relative = parent_relative.join(&entry.name);
            self.record_entry_failure(
                entry,
                relative.clone(),
                OperationError::semantic(
                    ErrorCategory::ParentDirFailed,
                    "join",
                    relative,
                    "destination directory could not be enumerated",
                ),
            )?;
        }
        Ok(())
    }

    fn record_file(
        &mut self,
        relative: &Path,
        outcome: FileOutcome,
        differences: Option<Vec<&'static str>>,
    ) -> Result<(), BigcpError> {
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
            FileOutcome::SkippedDifferent { bytes } => (
                "skipped_diff",
                *bytes,
                None,
                None,
                Some(
                    differences
                        .as_ref()
                        .map_or_else(|| "different".to_owned(), |values| values.join(",")),
                ),
                None,
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
        if let Some(point) = self.stats.maybe_roll(Duration::from_secs(30)) {
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

    fn record_error(&mut self, error: OperationError) -> Result<(), BigcpError> {
        self.aggregate_error(error.clone());
        self.audit.emit(&AuditEvent::Error { error })
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
            FileOutcome::SkippedDifferent { bytes } => {
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
    if is_same_or_descendant(&prospective_destination, &source)
        .map_err(|error| BigcpError::io("compare roots", error))?
        || is_same_or_descendant(&source, &prospective_destination)
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
        if !is_same_or_descendant(&resolved, &final_ancestor)
            .map_err(|error| BigcpError::io("revalidate created destination root", error))?
        {
            return Err(BigcpError::Invalid(
                "created destination escaped its pinned ancestor through a reparse point"
                    .to_owned(),
            ));
        }
        (resolved, Some(pin))
    };

    Ok(Preflight {
        source,
        destination,
        destination_exists: destination_preexisted || !options.dry_run,
        source_volume,
        destination_volume,
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
    for (label, path) in [
        ("state directory", &state_dir),
        ("log", &log_path),
        ("report", &report_path),
    ] {
        let resolved = prospective_final_path(path)?;
        if is_same_or_descendant(&resolved, source)
            .map_err(|error| BigcpError::io("validate audit path", error))?
            || is_same_or_descendant(&resolved, destination)
                .map_err(|error| BigcpError::io("validate audit path", error))?
        {
            return Err(BigcpError::Invalid(format!(
                "{label} may not be inside the source or destination tree"
            )));
        }
    }
    Ok((state_dir, log_path, report_path))
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

fn destination_join(path: &Path) -> std::io::Result<HashMap<Vec<u16>, DirectoryEntry>> {
    let mut map = HashMap::new();
    for entry in enumerate_directory(path)? {
        let key = ordinal_case_key(&entry.name)?;
        if map.insert(key, entry).is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "destination contains case-insensitive duplicate names",
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
        let folded = ordinal_case_key(path.as_os_str())
            .map_err(|error| BigcpError::io("case-fold path lock identity", error))?;
        for code_unit in folded {
            hasher.update(code_unit.to_le_bytes());
        }
        hasher.update([0_u8, 0_u8]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn value_hash(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))
}

fn same_path(left: &Path, right: &Path) -> Result<bool, BigcpError> {
    let left = absolute_extended(left).map_err(|error| BigcpError::io("normalize path", error))?;
    let right =
        absolute_extended(right).map_err(|error| BigcpError::io("normalize path", error))?;
    Ok(
        ordinal_case_key(left.as_os_str())
            .map_err(|error| BigcpError::io("compare path", error))?
            == ordinal_case_key(right.as_os_str())
                .map_err(|error| BigcpError::io("compare path", error))?,
    )
}

fn format_time(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

fn derive_hints(runner: &Runner<'_>, preflight: &Preflight) -> Vec<Hint> {
    let mut hints = Vec::new();
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
    counters: &Counters,
    verification: Option<&crate::report::VerificationSummary>,
) -> i32 {
    if canceled {
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

fn repair_file_metadata(
    source: &DirectoryEntry,
    destination: &DirectoryEntry,
    differences: &[&str],
) -> std::io::Result<()> {
    if differences.contains(&"ea_size") {
        let source_eas = read_extended_attributes(&source.path)?;
        clear_extended_attributes(&destination.path)?;
        if !source_eas.is_empty() {
            write_extended_attributes(&destination.path, &source_eas)?;
        }
    }
    set_basic_at(&destination.path, source.metadata.basic)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{completed_exit, path_hash, summarize_phases};
    use crate::model::Counters;
    use crate::report::VerificationSummary;
    use crate::stats::TimelinePoint;

    #[test]
    fn every_completed_failure_universe_controls_the_exit_code() {
        assert_eq!(completed_exit(true, &Counters::default(), None), 3);

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
            assert_eq!(completed_exit(false, &counters, None), 2);
        }

        let verification = VerificationSummary {
            failed: 1,
            ..VerificationSummary::default()
        };
        assert_eq!(
            completed_exit(false, &Counters::default(), Some(&verification)),
            2
        );
        assert_eq!(completed_exit(false, &Counters::default(), None), 0);
    }

    #[test]
    fn destination_lock_identity_is_case_insensitive() {
        let upper = path_hash(Path::new(r"\\?\C:\CopyTarget"));
        let lower = path_hash(Path::new(r"\\?\c:\copytarget"));
        assert!(upper.is_ok());
        assert_eq!(upper.ok(), lower.ok());
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
