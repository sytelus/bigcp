//! Harmless end-to-end contract test on a newly created system-drive sandbox.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::symlink_file;
use std::path::Path;

use bigcp_core::{CopyOptions, DeviceClass, RunObserver, RunSnapshot, VerifyOptions, run_copy};
use bigcp_testkit::sandbox::{initialize_empty, validated_system_temp};
use bigcp_testkit::{SandboxRoot, check_trees};
use bigcp_win::{
    BasicMetadata, DestinationStream, DestinationTemp, ExtendedAttributes, SourceStream,
    StreamInfo, clear_extended_attributes, is_sparse, metadata_at, read_extended_attributes,
    write_extended_attributes,
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
    let mut sparse = DestinationTemp::create(&source, "fixture", false)?;
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

    let mut options = CopyOptions::new(source.clone(), destination.clone());
    options.verify = true;
    options.state_dir = Some(state.clone());
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
fn replace_false_preserves_bytes_and_audits_the_withheld_change()
-> Result<(), Box<dyn std::error::Error>> {
    let sandbox = SandboxRoot::create_system_temp("replace-false")?;
    let source = sandbox.child(Path::new("source"))?;
    let destination = sandbox.child(Path::new("destination"))?;
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    fs::write(source.join("different.txt"), b"new source bytes")?;
    fs::write(destination.join("different.txt"), b"old")?;

    let mut options = CopyOptions::new(source, destination.clone());
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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

    let mut options = CopyOptions::new(source, destination);
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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

    let mut options = CopyOptions::new(source, destination);
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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

    let mut options = CopyOptions::new(source, destination.clone());
    options.state_dir = Some(state.clone());
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
    let state = sandbox.child(Path::new("state"))?;
    fs::create_dir(&source)?;
    fs::write(source.join("tiny.txt"), b"tiny")?;
    fs::write(source.join("bigger.bin"), vec![0x11_u8; 256 * 1024])?;

    let mut options = CopyOptions::new(source, destination);
    options.state_dir = Some(state);
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
    let mut sparse = DestinationTemp::create(&source, "same-spindle-fixture", false)?;
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

    let mut options = CopyOptions::new(source, destination);
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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
        let seen = self
            .polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

    let mut options = CopyOptions::new(source, destination.clone());
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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

    let mut options = CopyOptions::new(source.clone(), destination.clone());
    options.state_dir = Some(sandbox.child(Path::new("state"))?);
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
