//! Harmless end-to-end contract test on a newly created system-drive sandbox.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::symlink_file;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use bigcp_core::journal::{Checkpoint, CheckpointFileIdentity, Journal, JournalEvent, path_key};
use bigcp_core::{
    CopyOptions, DeviceClass, RunObserver, RunSnapshot, RunState, VerifyOptions, run_copy,
};
use bigcp_testkit::sandbox::{initialize_empty, validated_system_temp};
use bigcp_testkit::{SandboxRoot, check_trees};
use bigcp_win::{
    BasicMetadata, DestinationStream, DestinationTemp, ExtendedAttributes, SourceStream,
    StreamInfo, absolute_extended, clear_extended_attributes, final_path, is_sparse, metadata_at,
    open_root, read_extended_attributes, write_extended_attributes,
};
use xxhash_rust::xxh3::Xxh3;

const FIXTURE_WRITE_BUDGET: u64 = 16 * 1024 * 1024;

/// Builds the options every end-to-end run starts from. The state directory
/// is always a child of the test's own sandbox: without an explicit
/// `state_dir` the product would fall back to the machine-wide
/// `%LOCALAPPDATA%` state root, which tests must never touch.
fn sandboxed_options(
    sandbox: &SandboxRoot,
    source: PathBuf,
    destination: PathBuf,
    state_label: &str,
) -> Result<CopyOptions, Box<dyn std::error::Error>> {
    let mut options = CopyOptions::new(source, destination);
    options.state_dir = Some(sandbox.child(Path::new(state_label))?);
    Ok(options)
}

struct SilentObserver;

impl RunObserver for SilentObserver {
    fn on_snapshot(&self, _snapshot: &RunSnapshot) {}

    fn on_message(&self, _message: &str) {}
}

struct ImmediateCancel;

impl RunObserver for ImmediateCancel {
    fn on_snapshot(&self, _snapshot: &RunSnapshot) {}

    fn on_message(&self, _message: &str) {}

    fn cancellation_requested(&self) -> bool {
        true
    }
}

struct TerminalArtifactObserver {
    report_path: PathBuf,
    log_path: PathBuf,
    saw_final_artifacts: AtomicBool,
}

impl RunObserver for TerminalArtifactObserver {
    fn on_snapshot(&self, snapshot: &RunSnapshot) {
        if snapshot.state != RunState::Complete {
            return;
        }
        let report_ready = self.report_path.is_file();
        let log_ready = fs::read_to_string(&self.log_path)
            .ok()
            .and_then(|contents| contents.lines().last().map(str::to_owned))
            .and_then(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
            .is_some_and(|event| {
                event.get("ev").and_then(serde_json::Value::as_str) == Some("run_end")
            });
        self.saw_final_artifacts
            .store(report_ready && log_ready, Ordering::SeqCst);
    }

    fn on_message(&self, _message: &str) {}
}

struct CorruptOnVerify {
    destination_file: PathBuf,
    attempted: AtomicBool,
    succeeded: AtomicBool,
}

impl RunObserver for CorruptOnVerify {
    fn on_snapshot(&self, snapshot: &RunSnapshot) {
        if snapshot.state == RunState::Verifying && !self.attempted.swap(true, Ordering::SeqCst) {
            self.succeeded.store(
                fs::write(&self.destination_file, b"evil").is_ok(),
                Ordering::SeqCst,
            );
        }
    }

    fn on_message(&self, _message: &str) {}
}

#[test]
fn copy_rerun_and_both_verification_forms_converge() -> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-e2e-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let state = sandbox.child(Path::new("state"))?;
    fs::create_dir(&source)?;
    fs::create_dir(source.join("nested"))?;

    let directory_stream = StreamInfo {
        name: OsString::from(":directory-test:$DATA"),
        size: 21,
    };
    let mut directory_alternate =
        DestinationStream::create_reparse(&source.join("nested"), &directory_stream, true)?;
    directory_alternate.write_all(b"directory stream data")?;
    directory_alternate.flush()?;
    drop(directory_alternate);

    let small = source.join("small.txt");
    fs::write(&small, b"small, deterministic fixture")?;
    let source_eas = ExtendedAttributes::from_pairs(&[(b"bigcp.test", b"ea-value")])?;
    write_extended_attributes(&small, &source_eas)?;
    let large_bytes = vec![0x5a_u8; 2 * 1024 * 1024];
    assert!(large_bytes.len() as u64 <= FIXTURE_WRITE_BUDGET);
    fs::write(source.join("nested").join("large.bin"), &large_bytes)?;

    let stream = StreamInfo {
        name: OsString::from(":bigcp-test:$DATA"),
        size: 8 * 1024,
    };
    let mut alternate = DestinationStream::create(&small, &stream, true)?;
    let stream_size = usize::try_from(stream.size)?;
    alternate.write_all(&vec![0xa5_u8; stream_size])?;
    alternate.flush()?;
    drop(alternate);

    let sparse_path = source.join("sparse.bin");
    let mut sparse = DestinationTemp::create(&source, "fixture", false, false)?;
    sparse.mark_sparse()?;
    // Deliberately below the 4 MiB worker threshold: sparse preservation is
    // a storage-fidelity decision, not a large-file-only optimization.
    sparse.set_len(1024 * 1024)?;
    sparse.seek(SeekFrom::Start(768 * 1024))?;
    sparse.write_all(&vec![0x3c_u8; 4096])?;
    sparse.flush()?;
    sparse.commit(
        &sparse_path,
        false,
        BasicMetadata {
            creation_time: 0,
            last_access_time: 0,
            last_write_time: 0,
            attributes: 0,
        },
        false,
        true,
    )?;
    let expected_file_logical_bytes = fs::metadata(&small)?
        .len()
        .saturating_add(stream.size)
        .saturating_add(large_bytes.len() as u64)
        .saturating_add(fs::metadata(&sparse_path)?.len());

    let mut options = sandboxed_options(&sandbox, source.clone(), destination.clone(), "state")?;
    options.verify = true;
    options.tune.large_threshold = Some(4 * 1024 * 1024);
    options.tune.checkpoint_threshold = Some(1024 * 1024);
    let first = run_copy(&options, &SilentObserver)?;
    // A clean run end compacts the journal to its job header (PLAN section
    // 5.12): every checkpoint was retired by its PartDone, so nothing else
    // survives. Checkpoint identity binding and live-checkpoint survival are
    // pinned directly by the journal unit tests.
    let journal = fs::read_to_string(state.join("journal.jsonl"))?;
    let journal_lines: Vec<_> = journal.lines().collect();
    assert_eq!(
        journal_lines.len(),
        1,
        "clean run end must compact the journal to its job header: {journal_lines:?}"
    );
    assert!(
        journal_lines[0].contains("\"ev\":\"job\""),
        "compacted journal must retain exactly the job header"
    );
    let destination_names = fs::read_dir(&destination)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    let source_sparse_bytes = fs::read(&sparse_path)?;
    let destination_sparse_bytes = fs::read(destination.join("sparse.bin"))?;
    assert_eq!(
        source_sparse_bytes, destination_sparse_bytes,
        "sparse logical content differs"
    );
    assert!(
        metadata_at(&destination.join("sparse.bin"))
            .is_ok_and(|value| is_sparse(value.basic.attributes)),
        "small sparse source lost its sparse destination representation"
    );
    assert_eq!(first.run.exit, 0, "copy errors: {:?}", first.errors);
    assert_eq!(first.counters.failed, 0);
    assert_eq!(first.counters.copied_new, 3);
    assert_eq!(
        first.counters.bytes_logical_discovered,
        expected_file_logical_bytes
    );
    assert!(
        first.verify.as_ref().is_some_and(|value| value.failed == 0),
        "post-copy verification: {:?}",
        (first.verify.as_ref(), destination_names)
    );

    let oracle = check_trees(
        &SandboxRoot::open(lease.path())?,
        Path::new("source"),
        Path::new("destination"),
    )?;
    assert_eq!(oracle.mismatches, 0, "oracle samples: {:?}", oracle.samples);

    let full = bigcp_core::run_standalone_verify(&VerifyOptions {
        source: source.clone(),
        destination: destination.clone(),
    })?;
    assert_eq!(full.failed, 0, "verify mismatches: {:?}", full.mismatches);

    let destination_small = destination.join("small.txt");
    clear_extended_attributes(&destination_small)?;
    let repaired = run_copy(&options, &SilentObserver)?;
    assert_eq!(repaired.run.exit, 0);
    assert_eq!(repaired.counters.meta_fixed, 1);
    assert_eq!(repaired.counters.skipped_same, 2);
    assert_eq!(
        read_extended_attributes(&small)?,
        read_extended_attributes(&destination_small)?
    );

    let second = run_copy(&options, &SilentObserver)?;
    assert_eq!(second.run.exit, 0);
    assert_eq!(second.counters.copied_new, 0);
    assert_eq!(second.counters.copied_replaced, 0);
    assert_eq!(second.counters.skipped_same, 3);
    Ok(())
}

#[test]
fn dry_run_never_creates_destination_and_direct_replacement_converges()
-> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-dry-run-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    let replace_path = source.join("replace.txt");
    fs::write(&replace_path, b"first version")?;
    let stream = StreamInfo {
        name: OsString::from(":dry-run-count:$DATA"),
        size: 4,
    };
    let mut alternate = DestinationStream::create(&replace_path, &stream, true)?;
    alternate.write_all(b"ads!")?;
    alternate.flush()?;
    drop(alternate);

    let mut options =
        sandboxed_options(&sandbox, source.clone(), destination.clone(), "dry-state")?;
    options.dry_run = true;
    let modeled = run_copy(&options, &SilentObserver)?;
    assert_eq!(modeled.run.exit, 0);
    assert_eq!(modeled.counters.would_copy_new, 1);
    assert_eq!(
        modeled.counters.bytes_logical_discovered,
        fs::metadata(&replace_path)?.len() + stream.size
    );
    assert!(
        !destination.exists(),
        "dry-run created the destination tree"
    );

    let options = sandboxed_options(&sandbox, source.clone(), destination.clone(), "copy-state")?;
    let initial = run_copy(&options, &SilentObserver)?;
    assert_eq!(initial.run.exit, 0, "copy errors: {:?}", initial.errors);
    assert_eq!(initial.counters.copied_new, 1);

    fs::write(&replace_path, b"second, longer version")?;
    let replacement = run_copy(&options, &SilentObserver)?;
    assert_eq!(
        replacement.run.exit, 0,
        "replacement errors: {:?}",
        replacement.errors
    );
    assert_eq!(replacement.counters.copied_replaced, 1);
    assert_eq!(
        fs::read(destination.join("replace.txt"))?,
        b"second, longer version"
    );
    assert!(
        fs::read_dir(&destination)?.all(|entry| entry
            .ok()
            .is_some_and(|value| !value.file_name().to_string_lossy().starts_with(".bigcp-"))),
        "completed replacement left an opaque temporary"
    );
    Ok(())
}

#[test]
fn replace_false_preserves_bytes_and_audits_the_withheld_change()
-> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("replace-false")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    fs::write(source.join("different.txt"), b"new source bytes")?;
    fs::write(destination.join("different.txt"), b"old")?;

    let mut options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    options.replace = false;
    let report = run_copy(&options, &SilentObserver)?;

    assert_eq!(report.run.exit, 0);
    assert_eq!(report.counters.skipped_diff, 1);
    assert_eq!(fs::read(destination.join("different.txt"))?, b"old");
    let audit = fs::read_to_string(&report.run.log_path)?;
    let withheld = audit.lines().find_map(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .filter(|event| {
                event.get("ev").and_then(serde_json::Value::as_str) == Some("file")
                    && event.get("action").and_then(serde_json::Value::as_str)
                        == Some("skipped_diff")
            })
    });
    assert!(withheld.as_ref().is_some_and(|event| {
        event
            .pointer("/replacement/old_size")
            .and_then(serde_json::Value::as_u64)
            == Some(3)
            && event
                .pointer("/replacement/differences")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|fields| fields.iter().any(|field| field == "size"))
    }));
    Ok(())
}

#[test]
fn direct_replacement_handles_a_read_only_destination() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("read-only-replacement")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    fs::write(source.join("replace.txt"), b"replacement bytes")?;
    let destination_file = destination.join("replace.txt");
    fs::write(&destination_file, b"old")?;
    let mut permissions = fs::metadata(&destination_file)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&destination_file, permissions)?;

    let options = sandboxed_options(&sandbox, source, destination, "state")?;
    let report = run_copy(&options, &SilentObserver)?;

    assert_eq!(report.run.exit, 0, "errors: {:?}", report.errors);
    assert_eq!(report.counters.copied_replaced, 1);
    assert_eq!(fs::read(&destination_file)?, b"replacement bytes");
    assert!(!fs::metadata(destination_file)?.permissions().readonly());
    Ok(())
}

#[test]
fn cancellation_accounts_every_directory_already_discovered()
-> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-cancel-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::create_dir(source.join("not-visited"))?;

    let options = sandboxed_options(&sandbox, source, destination, "state")?;
    let report = run_copy(&options, &ImmediateCancel)?;
    assert_eq!(report.run.exit, 3);
    assert_eq!(report.counters.dirs_discovered, 1);
    assert_eq!(report.counters.dirs_failed, 1);
    assert!(report.counters.reconcile().is_ok());
    // A clean cancel is not an error condition: the untraversed subtree is a
    // warning with the path attached, and the error report stays empty so
    // exit-3 runs keep meaningful error summaries (PLAN section 5.13).
    assert!(
        report.errors.is_empty(),
        "clean cancellation polluted the error report: {:?}",
        report.errors
    );
    assert!(report.warnings.contains_key("canceled_subtree"));
    let audit = fs::read_to_string(&report.run.log_path)?;
    let has_cancel_warning = audit.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .is_some_and(|event| {
                event.get("ev").and_then(serde_json::Value::as_str) == Some("warning")
                    && event.get("kind").and_then(serde_json::Value::as_str)
                        == Some("canceled_subtree")
                    && event.pointer("/rel").is_some()
            })
    });
    assert!(
        has_cancel_warning,
        "canceled subtree was absent from the JSONL log"
    );
    Ok(())
}

#[test]
fn unsafe_audit_path_is_rejected_before_destination_creation()
-> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-audit-path-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("fixture.txt"), b"bounded fixture")?;

    let mut options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    // Deliberate override of the safe sandbox default: the run must reject an
    // audit state directory that nests inside the destination tree.
    options.state_dir = Some(destination.join("state"));
    let result = run_copy(&options, &SilentObserver);
    assert!(result.is_err());
    assert!(
        !destination.exists(),
        "preflight created destination before rejecting its unsafe audit path"
    );
    Ok(())
}

#[test]
fn colliding_audit_roles_are_rejected_before_destination_creation()
-> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-audit-collision-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let state = sandbox.child(Path::new("state"))?;
    let shared_artifact = sandbox.child(Path::new("shared-audit.json"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("fixture.txt"), b"bounded fixture")?;

    let mut options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    options.log_path = Some(shared_artifact.clone());
    options.report_path = Some(shared_artifact.clone());
    let result = run_copy(&options, &SilentObserver);
    assert!(result.is_err_and(|error| error.to_string().contains("must be distinct")));
    assert!(!destination.exists());
    assert!(!state.exists());
    assert!(!shared_artifact.exists());
    Ok(())
}

#[test]
fn report_fallback_and_terminal_audit_record_agree() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("report-fallback")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let unusable_report_path = sandbox.child(Path::new("existing-directory"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&unusable_report_path)?;

    let mut options = sandboxed_options(&sandbox, source, destination, "state")?;
    options.report_path = Some(unusable_report_path.clone());
    let report = run_copy(&options, &SilentObserver)?;

    assert_eq!(report.run.audit, "degraded");
    assert_ne!(report.run.report_path, unusable_report_path);
    assert!(report.run.report_path.is_file());
    let audit = fs::read_to_string(&report.run.log_path)?;
    let terminal = audit.lines().last().ok_or("audit log was empty")?;
    let terminal: serde_json::Value = serde_json::from_str(terminal)?;
    assert_eq!(
        terminal.get("ev").and_then(serde_json::Value::as_str),
        Some("run_end")
    );
    assert_eq!(
        terminal.get("audit").and_then(serde_json::Value::as_str),
        Some("degraded")
    );
    Ok(())
}

#[test]
fn complete_snapshot_is_published_after_final_artifacts() -> Result<(), Box<dyn std::error::Error>>
{
    let sandbox = SandboxRoot::create_system_temp("terminal-artifacts")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let log_path = sandbox.child(Path::new("audit.jsonl"))?;
    let report_path = sandbox.child(Path::new("report.json"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("fixture.txt"), b"bounded fixture")?;

    let mut options = sandboxed_options(&sandbox, source, destination, "state")?;
    options.log_path = Some(log_path.clone());
    options.report_path = Some(report_path.clone());
    let observer = TerminalArtifactObserver {
        report_path,
        log_path,
        saw_final_artifacts: AtomicBool::new(false),
    };
    let report = run_copy(&options, &observer)?;

    assert_eq!(report.run.exit, 0);
    assert!(observer.saw_final_artifacts.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn same_run_verification_mismatch_is_audited() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("verification-audit")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("fixture.txt"), b"good")?;

    let mut options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    options.verify = true;
    let observer = CorruptOnVerify {
        destination_file: destination.join("fixture.txt"),
        attempted: AtomicBool::new(false),
        succeeded: AtomicBool::new(false),
    };
    let report = run_copy(&options, &observer)?;

    assert!(observer.succeeded.load(Ordering::SeqCst));
    assert_eq!(report.run.exit, 2);
    assert_eq!(
        report.verify.as_ref().map(|summary| summary.failed),
        Some(1)
    );
    let audit = fs::read_to_string(&report.run.log_path)?;
    let verification = audit
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event.get("ev").and_then(serde_json::Value::as_str) == Some("verification"))
        .ok_or("verification event was missing")?;
    assert_eq!(
        verification
            .pointer("/verification/failed")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert!(
        verification
            .pointer("/verification/mismatches")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|mismatches| !mismatches.is_empty())
    );
    Ok(())
}

#[test]
fn existing_file_is_rejected_as_a_destination_root() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("destination-file")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination.txt"))?;
    fs::create_dir(&source)?;
    fs::write(&destination, b"sentinel must remain unchanged")?;

    let options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    assert!(run_copy(&options, &SilentObserver).is_err());
    assert_eq!(fs::read(&destination)?, b"sentinel must remain unchanged");
    Ok(())
}

#[test]
fn standalone_verify_rejects_file_roots_without_modifying_them()
-> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("verify-file-roots")?;
    let source = sandbox.child(Path::new("source.txt"))?;
    let destination = sandbox.child(Path::new("destination.txt"))?;
    fs::write(&source, b"source sentinel")?;
    fs::write(&destination, b"destination sentinel")?;
    let result = bigcp_core::run_standalone_verify(&VerifyOptions {
        source: source.clone(),
        destination: destination.clone(),
    });
    assert!(result.is_err());
    assert_eq!(fs::read(source)?, b"source sentinel");
    assert_eq!(fs::read(destination)?, b"destination sentinel");
    Ok(())
}

#[test]
fn failed_parent_subtree_logs_every_discovered_descendant() -> Result<(), Box<dyn std::error::Error>>
{
    let sandbox = SandboxRoot::create_system_temp("failed-subtree-audit")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    fs::create_dir(source.join("conflict"))?;
    fs::create_dir(source.join("conflict").join("nested"))?;
    fs::write(
        source.join("conflict").join("nested").join("file.txt"),
        b"test-owned descendant",
    )?;
    fs::write(destination.join("conflict"), b"type-conflict sentinel")?;

    let options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 2);
    assert_eq!(report.counters.not_attempted, 1);
    assert_eq!(report.counters.dirs_failed, 2);
    assert!(report.counters.reconcile().is_ok());
    assert_eq!(
        fs::read(destination.join("conflict"))?,
        b"type-conflict sentinel"
    );

    let audit = fs::read_to_string(&report.run.log_path)?;
    assert!(audit.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .is_some_and(|event| {
                event.get("ev").and_then(serde_json::Value::as_str) == Some("directory")
                    && event.get("action").and_then(serde_json::Value::as_str)
                        == Some("not_attempted_parent_failed")
            })
    }));
    assert!(audit.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .is_some_and(|event| {
                event.get("ev").and_then(serde_json::Value::as_str) == Some("file")
                    && event.get("action").and_then(serde_json::Value::as_str)
                        == Some("not_attempted")
            })
    }));
    Ok(())
}

#[test]
fn relative_symbolic_links_are_recreated_as_links_when_available()
-> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("symbolic-link")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("target.txt"), b"target")?;
    let source_link = source.join("link.txt");
    if symlink_file("target.txt", &source_link).is_err() {
        return Ok(());
    }
    let stream = StreamInfo {
        name: OsString::from(":link-metadata:$DATA"),
        size: 16,
    };
    let mut output = DestinationStream::create_reparse(&source_link, &stream, true)?;
    output.write_all(b"link stream data")?;
    output.flush()?;
    drop(output);

    let options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 0, "link errors: {:?}", report.errors);
    assert_eq!(report.counters.links_copied, 1);
    assert_eq!(
        fs::read_link(destination.join("link.txt"))?,
        Path::new("target.txt")
    );
    let mut copied_stream = SourceStream::open_reparse(&destination.join("link.txt"), &stream)?;
    let mut bytes = Vec::new();
    copied_stream.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"link stream data");
    let oracle = check_trees(&sandbox, Path::new("source"), Path::new("destination"))?;
    assert_eq!(oracle.mismatches, 0, "oracle samples: {:?}", oracle.samples);
    let verification = bigcp_core::run_standalone_verify(&VerifyOptions {
        source: sandbox.child(Path::new("source"))?,
        destination,
    })?;
    assert_eq!(
        verification.failed, 0,
        "verify: {:?}",
        verification.mismatches
    );
    Ok(())
}

#[test]
fn analyze_flag_produces_bounded_insight() -> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-analyze-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("tiny.txt"), b"tiny")?;
    fs::write(source.join("bigger.bin"), vec![0x11_u8; 256 * 1024])?;

    let mut options = sandboxed_options(&sandbox, source, destination, "state")?;
    options.analyze = true;
    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 0);
    let analysis = report.analysis.as_ref();
    assert!(analysis.is_some(), "--analyze must add a report section");
    let Some(analysis) = analysis else {
        return Ok(());
    };
    // Fixed five buckets, exactly the copied files distributed among them,
    // and a bounded slowest table: the whole VISION contract for the flag.
    assert_eq!(analysis.size_classes.len(), 5);
    let bucket_files: u64 = analysis.size_classes.iter().map(|class| class.files).sum();
    assert_eq!(bucket_files, report.counters.copied_new);
    assert!(!analysis.slowest_files.is_empty());
    assert!(analysis.slowest_files.len() <= 20);
    let audit = fs::read_to_string(&report.run.log_path)?;
    assert!(
        audit
            .lines()
            .any(|line| line.contains("\"ev\":\"analysis\"")),
        "one analysis event must land in the JSONL log"
    );

    // Without the flag the section and event must be absent (zero cost).
    let plain = run_copy(&options_without_analyze(&options), &SilentObserver)?;
    assert!(plain.analysis.is_none());
    Ok(())
}

fn options_without_analyze(base: &CopyOptions) -> CopyOptions {
    let mut options = base.clone();
    options.analyze = false;
    options.fresh = true;
    options
}

#[test]
fn same_spindle_profile_batches_small_files_and_bursts_large_data()
-> Result<(), Box<dyn std::error::Error>> {
    // Fresh writes: <2 MiB of source/destination payload plus bounded run
    // state, within the 16 MiB end-to-end fixture budget in docs/TESTING.md.
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-same-spindle-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    for index in 0_u8..4 {
        fs::write(
            source.join(format!("small-{index}.bin")),
            vec![index; 16 * 1024],
        )?;
    }
    let large = source.join("large.bin");
    fs::write(&large, vec![0xA7; 320 * 1024])?;
    let stream = StreamInfo {
        name: OsString::from(":same-spindle:$DATA"),
        size: 192 * 1024,
    };
    let mut alternate = DestinationStream::create(&large, &stream, true)?;
    alternate.write_all(&vec![0x5C; usize::try_from(stream.size)?])?;
    alternate.flush()?;
    drop(alternate);

    let sparse_path = source.join("sparse.bin");
    let mut sparse = DestinationTemp::create(&source, "same-spindle-fixture", false, false)?;
    sparse.mark_sparse()?;
    sparse.set_len(512 * 1024)?;
    sparse.seek(SeekFrom::Start(384 * 1024))?;
    sparse.write_all(&vec![0x3D; 4096])?;
    sparse.commit(
        &sparse_path,
        false,
        BasicMetadata {
            creation_time: 0,
            last_access_time: 0,
            last_write_time: 0,
            attributes: 0,
        },
        false,
        true,
    )?;

    let mut options = sandboxed_options(&sandbox, source, destination, "state")?;
    // Both roots are on this one test volume. The explicit rotational class
    // makes the deterministic topology policy testable on SSD-only CI.
    options.source_profile = DeviceClass::Hdd;
    options.destination_profile = DeviceClass::Hdd;
    options.tune.large_threshold = Some(64 * 1024);
    options.tune.chunk_bytes = Some(64 * 1024);
    options.tune.same_spindle_burst_bytes = Some(1024 * 1024);
    options.verify = true;

    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 0, "copy errors: {:?}", report.errors);
    assert!(report.devices.same_physical_disk);
    assert!(report.devices.transport.is_same_spindle());
    assert_eq!(report.devices.workers, 1);
    assert_eq!(report.counters.copied_new, 6);
    assert!(report.counters.reconcile().is_ok());
    assert!(
        report
            .hints
            .iter()
            .any(|hint| hint.id == "same_spindle_transport")
    );
    assert!(report.verify.is_some_and(|summary| summary.failed == 0));
    Ok(())
}

/// Requests cancellation only after the run is already inside the large-file
/// engine, proving a graceful stop takes effect between chunks instead of
/// waiting for the whole file (PLAN section 5.13).
struct CancelAfterFirstPoll {
    polls: std::sync::atomic::AtomicUsize,
}

impl RunObserver for CancelAfterFirstPoll {
    fn on_snapshot(&self, _snapshot: &RunSnapshot) {}

    fn on_message(&self, _message: &str) {}

    fn cancellation_requested(&self) -> bool {
        let seen = self.polls.fetch_add(1, Ordering::Relaxed);
        seen >= 1
    }
}

#[test]
fn graceful_cancel_stops_inside_a_large_file() -> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-midfile-cancel-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("large.bin"), vec![0x7e_u8; 512 * 1024])?;

    let mut options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    // Route the file through the inline large-file path in small chunks so
    // several cancel polls happen inside one file.
    options.tune.large_threshold = Some(64 * 1024);
    options.tune.chunk_bytes = Some(64 * 1024);
    let report = run_copy(
        &options,
        &CancelAfterFirstPoll {
            polls: std::sync::atomic::AtomicUsize::new(0),
        },
    )?;

    assert_eq!(report.run.exit, 3);
    assert_eq!(report.counters.not_attempted, 1);
    assert!(report.counters.reconcile().is_ok());
    assert!(
        report.errors.is_empty(),
        "mid-file cancellation polluted the error report: {:?}",
        report.errors
    );
    assert!(report.warnings.contains_key("canceled_mid_file"));
    // Nothing may remain at the final name and the opaque temp must have
    // self-deleted (it never reached a checkpoint).
    assert!(!destination.join("large.bin").exists());
    let leftovers: Vec<_> = fs::read_dir(&destination)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "destination should hold no partial artifacts: {leftovers:?}"
    );
    Ok(())
}

#[test]
fn hidden_large_stream_is_promoted_from_worker_to_inline_streaming()
-> Result<(), Box<dyn std::error::Error>> {
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-promote-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    // Tiny unnamed stream routes the file to a worker; the 256 KiB ADS is
    // above the tuned large-threshold, so the worker must hand the file
    // back and the coordinator must stream it inline — with the ADS intact.
    let host = source.join("tiny-with-big-ads.txt");
    fs::write(&host, b"tiny")?;
    let stream = StreamInfo {
        name: OsString::from(":huge:$DATA"),
        size: 256 * 1024,
    };
    let mut alternate = DestinationStream::create(&host, &stream, true)?;
    alternate.write_all(&vec![0xC3_u8; 256 * 1024])?;
    alternate.flush()?;
    drop(alternate);

    let mut options = sandboxed_options(&sandbox, source.clone(), destination.clone(), "state")?;
    options.verify = true;
    options.tune.large_threshold = Some(64 * 1024);
    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 0, "errors: {:?}", report.errors);
    assert_eq!(report.counters.copied_new, 1);
    assert!(report.counters.reconcile().is_ok());
    // The promotion round-trip must not double-count discovery or outcomes.
    assert_eq!(report.counters.files_discovered, 1);
    let full = bigcp_core::run_standalone_verify(&VerifyOptions {
        source,
        destination,
    })?;
    assert_eq!(full.failed, 0, "verify mismatches: {:?}", full.mismatches);
    Ok(())
}

#[test]
fn verified_checkpoint_resume_completes_an_interrupted_large_file()
-> Result<(), Box<dyn std::error::Error>> {
    // Deterministic synthetic interruption: the destination holds a persistent
    // opaque temp with a correct 1 MiB prefix and the state directory holds
    // the matching journal checkpoint — exactly what a canceled run leaves
    // behind — so the rerun must resume through verified-prefix validation
    // instead of restarting. Fresh writes stay near 5 MiB, inside the 16 MiB
    // fixture budget.
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-resume-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let state = sandbox.child(Path::new("state"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;

    let size = 2 * 1024 * 1024_u64;
    let watermark = 1024 * 1024_u64;
    let mut payload = vec![0_u8; usize::try_from(size)?];
    let mut pattern = 0x2f_u8;
    for byte in &mut payload {
        pattern = pattern.wrapping_mul(31).wrapping_add(7);
        *byte = pattern;
    }
    let source_file = source.join("large.bin");
    fs::write(&source_file, &payload)?;
    let source_metadata = metadata_at(&source_file)?;

    // The interrupted-run partial: a bigcp-shaped opaque sibling holding
    // exactly the checkpointed prefix, persisted for journal-backed resume.
    let prefix = &payload[..usize::try_from(watermark)?];
    let mut temp = DestinationTemp::create(&destination, "resume-seed", false, false)?;
    temp.write_all(prefix)?;
    temp.flush()?;
    temp.persist_for_resume()?;
    let temp_identity = CheckpointFileIdentity::from_file(temp.identity()?);
    let temp_name = temp
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or("opaque temporary name was not valid UTF-8")?;
    drop(temp);
    let mut prefix_hasher = Xxh3::new();
    prefix_hasher.update(prefix);

    // The matching journal. Its job signature must reproduce what run_copy
    // computes — final-path-resolved roots plus the resume-protocol constant —
    // or begin_job would discard the checkpoint; a signature drift therefore
    // fails this test loudly instead of skipping the resume silently.
    let resolved_source = final_path(&open_root(&absolute_extended(&source)?)?)?;
    let resolved_destination = final_path(&open_root(&absolute_extended(&destination)?)?)?;
    let journal_path = state.join("journal.jsonl");
    let mut journal = Journal::open(journal_path.clone(), false)?;
    journal.begin_job(
        "interrupted-run".to_owned(),
        path_key(&resolved_source),
        path_key(&resolved_destination),
        "resume-protocol-v1".to_owned(),
        "2026-08-01T00:00:00Z".to_owned(),
    )?;
    journal.append(JournalEvent::Checkpoint(Checkpoint {
        relative_path: path_key(Path::new("large.bin")),
        stream: String::new(),
        temp_name: temp_name.clone(),
        temp_identity: Some(temp_identity.clone()),
        source_identity: Some(CheckpointFileIdentity::from_file(source_metadata.identity)),
        source_size: size,
        source_mtime: source_metadata.basic.last_write_time,
        watermark,
        prefix_digest: format!("xxh3:{:032x}", prefix_hasher.digest128()),
    }))?;
    drop(journal);
    assert!(destination.join(&temp_name).is_file());

    let mut options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    // Make the 2 MiB fixture checkpoint-eligible so it takes the same
    // journaled transactional path a 40 GB production file would.
    options.tune.checkpoint_threshold = Some(64 * 1024);
    let report = run_copy(&options, &SilentObserver)?;

    assert_eq!(report.run.exit, 0, "resume errors: {:?}", report.errors);
    assert_eq!(report.counters.copied_new, 1);
    // Resumed, not restarted: the run re-hashed exactly the checkpointed
    // prefix from the temp and read only the remaining tail from the source.
    assert_eq!(report.counters.bytes_verified, watermark);
    assert_eq!(report.counters.bytes_read_source, size - watermark);
    assert_eq!(report.counters.bytes_written_destination, size - watermark);
    let final_file = destination.join("large.bin");
    assert_eq!(fs::read(&final_file)?, payload);
    // The seeded temp object itself became the final file (same filesystem
    // identity), proving the partial was consumed rather than replaced.
    assert!(temp_identity.matches(metadata_at(&final_file)?.identity));
    let leftovers: Vec<_> = fs::read_dir(&destination)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with(".bigcp-"))
        .collect();
    assert!(leftovers.is_empty(), "unconsumed partials: {leftovers:?}");
    // The retired checkpoint is gone: a clean end compacts the journal back
    // to its job header alone (PLAN section 5.12).
    let compacted = fs::read_to_string(&journal_path)?;
    assert_eq!(compacted.lines().count(), 1, "journal: {compacted}");
    assert!(compacted.contains("\"ev\":\"job\""));
    let oracle = check_trees(&sandbox, Path::new("source"), Path::new("destination"))?;
    assert_eq!(oracle.mismatches, 0, "oracle samples: {:?}", oracle.samples);
    Ok(())
}

/// Seeds exactly what an interrupted run leaves behind: a persistent opaque
/// destination temp holding `prefix` plus this destination's journal
/// checkpoint describing it (real temp identity, run_copy-compatible job
/// signature). Returns the temp's name and recorded identity.
fn seed_checkpointed_partial(
    source: &Path,
    destination: &Path,
    state: &Path,
    relative_file: &str,
    prefix: &[u8],
    source_size: u64,
    source_mtime: i64,
    source_identity: Option<CheckpointFileIdentity>,
) -> Result<(String, CheckpointFileIdentity), Box<dyn std::error::Error>> {
    let mut temp = DestinationTemp::create(destination, "reclaim-seed", false, false)?;
    temp.write_all(prefix)?;
    temp.flush()?;
    temp.persist_for_resume()?;
    let temp_identity = CheckpointFileIdentity::from_file(temp.identity()?);
    let temp_name = temp
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or("opaque temporary name was not valid UTF-8")?;
    drop(temp);
    let mut prefix_hasher = Xxh3::new();
    prefix_hasher.update(prefix);
    // The job signature must reproduce what run_copy computes (resolved
    // roots plus the resume-protocol constant) or begin_job would discard
    // the checkpoint through the signature-change path instead of the one
    // under test.
    let resolved_source = final_path(&open_root(&absolute_extended(source)?)?)?;
    let resolved_destination = final_path(&open_root(&absolute_extended(destination)?)?)?;
    let mut journal = Journal::open(state.join("journal.jsonl"), false)?;
    journal.begin_job(
        "interrupted-run".to_owned(),
        path_key(&resolved_source),
        path_key(&resolved_destination),
        "resume-protocol-v1".to_owned(),
        "2026-08-01T00:00:00Z".to_owned(),
    )?;
    journal.append(JournalEvent::Checkpoint(Checkpoint {
        relative_path: path_key(Path::new(relative_file)),
        stream: String::new(),
        temp_name: temp_name.clone(),
        temp_identity: Some(temp_identity.clone()),
        source_identity,
        source_size,
        source_mtime,
        watermark: u64::try_from(prefix.len())?,
        prefix_digest: format!("xxh3:{:032x}", prefix_hasher.digest128()),
    }))?;
    drop(journal);
    Ok((temp_name, temp_identity))
}

#[test]
fn orphaned_temp_with_deleted_source_is_reclaimed() -> Result<(), Box<dyn std::error::Error>> {
    // Scenario (a) of the orphan contract: the checkpointed source file was
    // deleted before the rerun, so the persisted partial would otherwise
    // squat at the destination forever. The journal proves bigcp created it
    // (name plus matching filesystem identity), so the rerun reclaims it.
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-reclaim-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let state = sandbox.child(Path::new("state"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    fs::write(source.join("keep.txt"), b"unrelated survivor")?;
    let (temp_name, _) = seed_checkpointed_partial(
        &source,
        &destination,
        &state,
        "ghost.bin",
        &vec![0x42_u8; 4096],
        64 * 1024,
        12345,
        None,
    )?;
    assert!(destination.join(&temp_name).is_file());

    let options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 0, "errors: {:?}", report.errors);
    assert!(
        !destination.join(&temp_name).exists(),
        "identity-proven orphan must be reclaimed"
    );
    assert_eq!(report.warnings.get("temp_reclaimed").copied(), Some(1));
    assert_eq!(report.counters.extra, 0, "a reclaimed temp is not an extra");
    // The reclaimed checkpoint was retired, so clean-end compaction leaves
    // only the job header: replay cannot resurrect the hint.
    let compacted = fs::read_to_string(state.join("journal.jsonl"))?;
    assert_eq!(compacted.lines().count(), 1, "journal: {compacted}");

    // A rerun stays clean: nothing re-reports and nothing re-reclaims.
    let rerun = run_copy(&options, &SilentObserver)?;
    assert_eq!(rerun.run.exit, 0, "rerun errors: {:?}", rerun.errors);
    assert!(!rerun.warnings.contains_key("temp_reclaimed"));
    assert_eq!(rerun.counters.extra, 0);
    Ok(())
}

#[test]
fn temp_name_reuse_with_wrong_identity_is_never_deleted() -> Result<(), Box<dyn std::error::Error>>
{
    // The journal names this temp, but the object now at that name is a
    // different filesystem object (the user removed bigcp's temp and put
    // their own file there). Identity mismatch means no proof: the file is
    // reported as an ordinary extra and must never be deleted.
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-noproof-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let state = sandbox.child(Path::new("state"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    let (temp_name, _) = seed_checkpointed_partial(
        &source,
        &destination,
        &state,
        "ghost.bin",
        &vec![0x42_u8; 4096],
        64 * 1024,
        12345,
        None,
    )?;
    let user_payload = b"user data that happens to share the temp name".to_vec();
    fs::remove_file(destination.join(&temp_name))?;
    fs::write(destination.join(&temp_name), &user_payload)?;

    let options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 0, "errors: {:?}", report.errors);
    assert_eq!(
        fs::read(destination.join(&temp_name))?,
        user_payload,
        "an unproven look-alike must survive byte-for-byte"
    );
    assert!(!report.warnings.contains_key("temp_reclaimed"));
    assert_eq!(
        report.counters.extra, 1,
        "the look-alike stays a reported extra"
    );
    assert!(
        report
            .extras
            .samples
            .iter()
            .any(|sample| sample.contains(&temp_name)),
        "extras samples: {:?}",
        report.extras.samples
    );
    Ok(())
}

#[test]
fn fresh_run_reclaims_the_previous_partial() -> Result<(), Box<dyn std::error::Error>> {
    // Scenario (b): `--fresh` truncates the journal, so the previous run's
    // partial can never be resumed — but the pre-truncation records still
    // prove bigcp created it, so the fresh run recopies the file in full
    // and reclaims the now-orphaned temp.
    let allowed_temp = validated_system_temp()?;
    let lease = tempfile::Builder::new()
        .prefix("bigcp-freshgc-")
        .tempdir_in(allowed_temp)?;
    let sandbox = initialize_empty(lease.path())?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let state = sandbox.child(Path::new("state"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;

    let size = 2 * 1024 * 1024_u64;
    let watermark = 1024 * 1024_usize;
    let mut payload = vec![0_u8; usize::try_from(size)?];
    let mut pattern = 0x2f_u8;
    for byte in &mut payload {
        pattern = pattern.wrapping_mul(31).wrapping_add(7);
        *byte = pattern;
    }
    let source_file = source.join("large.bin");
    fs::write(&source_file, &payload)?;
    let source_metadata = metadata_at(&source_file)?;
    let (temp_name, temp_identity) = seed_checkpointed_partial(
        &source,
        &destination,
        &state,
        "large.bin",
        &payload[..watermark],
        size,
        source_metadata.basic.last_write_time,
        Some(CheckpointFileIdentity::from_file(source_metadata.identity)),
    )?;
    assert!(destination.join(&temp_name).is_file());

    let mut options = sandboxed_options(&sandbox, source, destination.clone(), "state")?;
    options.fresh = true;
    options.tune.checkpoint_threshold = Some(64 * 1024);
    let report = run_copy(&options, &SilentObserver)?;
    assert_eq!(report.run.exit, 0, "errors: {:?}", report.errors);
    assert_eq!(report.counters.copied_new, 1);
    // Full recopy, not a resume: every source byte was read and no
    // checkpointed prefix was digest-verified.
    assert_eq!(report.counters.bytes_read_source, size);
    assert_eq!(report.counters.bytes_verified, 0);
    let final_file = destination.join("large.bin");
    assert_eq!(fs::read(&final_file)?, payload);
    // The final file is a fresh object — the seeded partial was not spliced
    // in — and the orphan was reclaimed without becoming an extra.
    assert!(!temp_identity.matches(metadata_at(&final_file)?.identity));
    assert!(!destination.join(&temp_name).exists());
    assert_eq!(report.warnings.get("temp_reclaimed").copied(), Some(1));
    assert_eq!(report.counters.extra, 0);
    let leftovers: Vec<_> = fs::read_dir(&destination)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with(".bigcp-"))
        .collect();
    assert!(leftovers.is_empty(), "stray partials: {leftovers:?}");
    Ok(())
}
