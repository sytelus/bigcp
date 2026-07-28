//! Harmless end-to-end contract test on a newly created system-drive sandbox.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::symlink_file;
use std::path::Path;

use bigcp_core::{CopyOptions, RunObserver, RunSnapshot, VerifyOptions, run_copy};
use bigcp_testkit::sandbox::{initialize_empty, validated_system_temp};
use bigcp_testkit::{SandboxRoot, check_trees};
use bigcp_win::{
    BasicMetadata, DestinationStream, DestinationTemp, ExtendedAttributes, SourceStream,
    StreamInfo, clear_extended_attributes, read_extended_attributes, write_extended_attributes,
};

const FIXTURE_WRITE_BUDGET: u64 = 16 * 1024 * 1024;

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
    let mut sparse = DestinationTemp::create(&source, "fixture")?;
    sparse.mark_sparse()?;
    sparse.set_len(4 * 1024 * 1024)?;
    sparse.seek(SeekFrom::Start(3 * 1024 * 1024))?;
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
    )?;
    let expected_file_logical_bytes = fs::metadata(&small)?
        .len()
        .saturating_add(stream.size)
        .saturating_add(large_bytes.len() as u64)
        .saturating_add(fs::metadata(&sparse_path)?.len());

    let mut options = CopyOptions::new(source.clone(), destination.clone());
    options.verify = true;
    options.state_dir = Some(state.clone());
    options.tune.large_threshold = Some(4 * 1024 * 1024);
    options.tune.checkpoint_threshold = Some(1024 * 1024);
    let first = run_copy(&options, &SilentObserver)?;
    let journal = fs::read_to_string(state.join("journal.jsonl"))?;
    assert!(
        journal
            .lines()
            .any(|line| line.contains("\"ev\":\"checkpoint\"")),
        "a checkpoint-eligible file below the large-file threshold bypassed the coordinator"
    );
    assert!(
        journal.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_some_and(|record| {
                    record.get("ev").and_then(serde_json::Value::as_str) == Some("checkpoint")
                        && record.get("temp_identity").is_some()
                        && record.get("source_identity").is_some()
                })
        }),
        "checkpoint did not bind both source and temporary identities"
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
        report_path: None,
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
fn dry_run_never_creates_destination_and_replacement_is_atomic()
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

    let mut options = CopyOptions::new(source.clone(), destination.clone());
    options.dry_run = true;
    options.state_dir = Some(sandbox.child(Path::new("dry-state"))?);
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

    options.dry_run = false;
    options.state_dir = Some(sandbox.child(Path::new("copy-state"))?);
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

    let mut options = CopyOptions::new(source, destination);
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
    let report = run_copy(&options, &ImmediateCancel)?;
    assert_eq!(report.run.exit, 3);
    assert_eq!(report.counters.dirs_discovered, 1);
    assert_eq!(report.counters.dirs_failed, 1);
    assert!(report.counters.reconcile().is_ok());
    let audit = fs::read_to_string(&report.run.log_path)?;
    let has_complete_error = audit.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .is_some_and(|event| {
                event.get("ev").and_then(serde_json::Value::as_str) == Some("error")
                    && event
                        .pointer("/error/operation")
                        .and_then(serde_json::Value::as_str)
                        == Some("cancel_before_enumerate")
                    && event.pointer("/error/category").is_some()
                    && event.pointer("/error/path").is_some()
                    && event.pointer("/error/hint").is_some()
            })
    });
    assert!(has_complete_error, "non-file failure was absent from JSONL");
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

    let mut options = CopyOptions::new(source, destination.clone());
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
fn report_fallback_and_terminal_audit_record_agree() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("report-fallback")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    let state = sandbox.child(Path::new("state"))?;
    let unusable_report_path = sandbox.child(Path::new("existing-directory"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&unusable_report_path)?;

    let mut options = CopyOptions::new(source, destination);
    options.state_dir = Some(state);
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
fn existing_file_is_rejected_as_a_destination_root() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("destination-file")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination.txt"))?;
    fs::create_dir(&source)?;
    fs::write(&destination, b"sentinel must remain unchanged")?;

    let mut options = CopyOptions::new(source, destination.clone());
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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
        report_path: None,
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

    let mut options = CopyOptions::new(source, destination.clone());
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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

    let mut options = CopyOptions::new(source, destination.clone());
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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
        report_path: None,
    })?;
    assert_eq!(
        verification.failed, 0,
        "verify: {:?}",
        verification.mismatches
    );
    Ok(())
}
