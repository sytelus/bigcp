//! CRC-tagged append-only checkpoint journal.
//!
//! Journal records can accelerate resume but can never authorize a completed
//! skip. Callers must validate source metadata and digest the temporary prefix
//! before trusting any checkpoint.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::BigcpError;

/// Stable identity of the bigcp-owned temporary named by a checkpoint.
///
/// The name alone is not proof of ownership: an old temporary can be removed
/// and an unrelated object can later appear at the same path. Resume therefore
/// requires both this identity and the checkpointed prefix digest to match.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointFileIdentity {
    /// Volume serial reported by Windows `FileIdInfo`.
    volume_serial: u64,
    /// Lowercase hexadecimal encoding of the 128-bit filesystem file ID.
    file_id: String,
}

impl CheckpointFileIdentity {
    /// Captures a filesystem identity in the journal's stable JSON shape.
    #[must_use]
    pub fn from_file(identity: bigcp_win::FileIdentity) -> Self {
        Self {
            volume_serial: identity.volume_serial,
            file_id: hex::encode(identity.file_id),
        }
    }

    /// Returns whether a currently opened object is the checkpointed temp.
    #[must_use]
    pub fn matches(&self, identity: bigcp_win::FileIdentity) -> bool {
        self.volume_serial == identity.volume_serial
            && self.file_id == hex::encode(identity.file_id)
    }
}

/// One resumable stream checkpoint.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Checkpoint {
    /// Relative display path.
    pub relative_path: String,
    /// Empty for unnamed stream, otherwise the named stream.
    pub stream: String,
    /// Opaque sibling temporary path.
    pub temp_name: String,
    /// Identity of the temporary at checkpoint time.
    ///
    /// This is optional only so version-one journals written by older builds
    /// remain parseable. Identity-less records are never eligible for resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_identity: Option<CheckpointFileIdentity>,
    /// Identity of the source file that produced this partial.
    ///
    /// Size and last-write time remain the user-visible equality heuristic,
    /// but a replacement file that happens to share both must not be spliced
    /// onto an older temporary prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<CheckpointFileIdentity>,
    /// Source stream size at checkpoint time.
    pub source_size: u64,
    /// Source last-write FILETIME.
    pub source_mtime: i64,
    /// Tentative contiguous write watermark.
    pub watermark: u64,
    /// xxh3-128 digest of exactly the prefix before watermark.
    pub prefix_digest: String,
}

/// Version-one journal event.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum JournalEvent {
    /// Job header.
    Job {
        /// Unique run identifier.
        run_id: String,
        /// Resolved source display path.
        source: String,
        /// Resolved destination display path.
        destination: String,
        /// Stable hash of semantic options.
        options_hash: String,
        /// RFC3339 timestamp.
        timestamp: String,
    },
    /// Tentative large-stream checkpoint.
    Checkpoint(Checkpoint),
    /// All streams committed and old checkpoints retire.
    PartDone {
        /// Relative display path.
        relative_path: String,
    },
    /// Clean run end.
    End {
        /// Run identifier.
        run_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct UnsignedRecord {
    j: u32,
    #[serde(flatten)]
    event: JournalEvent,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredRecord {
    j: u32,
    #[serde(flatten)]
    event: JournalEvent,
    crc: String,
}

/// Loaded journal state and append handle.
pub struct Journal {
    path: PathBuf,
    /// Present during ordinary append operation; temporarily taken and closed
    /// while Windows atomically replaces the journal during compaction.
    file: Option<File>,
    resumable: BTreeMap<(String, String), Checkpoint>,
    torn_tail: bool,
    active_job: Option<JobSignature>,
    /// The verbatim Job record of the current run, kept for compaction.
    job_record: Option<JournalEvent>,
    /// Interior records that failed CRC or parsing and were ignored (never
    /// trusted, never truncated — truncation is reserved for the genuine
    /// torn tail, because a mid-file mismatch is more likely an additive
    /// newer-build field than corruption, and destroying every later valid
    /// record over it would be the real data loss).
    skipped_records: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JobSignature {
    source: String,
    destination: String,
    options_hash: String,
}

impl Journal {
    /// Loads a journal, truncating only an invalid final record and ignoring
    /// invalid interior records without trusting them.
    pub fn open(path: PathBuf, fresh: bool) -> Result<Self, BigcpError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| BigcpError::io("create journal parent", error))?;
        }
        let mut resumable = BTreeMap::new();
        let mut torn_tail = false;
        let mut valid_length = 0_u64;
        let mut active_job: Option<JobSignature> = None;
        let mut skipped_records = 0_u64;
        if path.exists() && !fresh {
            let mut reader = BufReader::new(
                File::open(&path).map_err(|error| BigcpError::io("open journal", error))?,
            );
            // One-record lookahead distinguishes a torn final append from an
            // invalid interior record without retaining the complete journal
            // in memory. Clean-run compaction bounds history in normal use;
            // this also keeps interrupted-run replay O(one record).
            let mut pending: Option<(String, u64)> = None;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        if let Some((raw, bytes)) = pending.take() {
                            if apply_loaded_line(
                                &raw,
                                true,
                                &mut resumable,
                                &mut active_job,
                                &mut skipped_records,
                            )? {
                                valid_length = valid_length.saturating_add(bytes);
                            } else {
                                torn_tail = true;
                            }
                        }
                        break;
                    }
                    Ok(bytes) => {
                        let bytes = u64::try_from(bytes).map_err(|_| {
                            BigcpError::Format(
                                "journal record length exceeds address space".to_owned(),
                            )
                        })?;
                        if let Some((raw, previous_bytes)) = pending.replace((line, bytes)) {
                            let retained = apply_loaded_line(
                                &raw,
                                false,
                                &mut resumable,
                                &mut active_job,
                                &mut skipped_records,
                            )?;
                            debug_assert!(retained, "an interior record is always retained");
                            valid_length = valid_length.saturating_add(previous_bytes);
                        }
                    }
                    Err(_) => {
                        // A read error makes the prior complete line interior;
                        // only unread bytes after it are discarded.
                        if let Some((raw, bytes)) = pending.take() {
                            let retained = apply_loaded_line(
                                &raw,
                                false,
                                &mut resumable,
                                &mut active_job,
                                &mut skipped_records,
                            )?;
                            debug_assert!(retained, "an interior record is always retained");
                            valid_length = valid_length.saturating_add(bytes);
                        }
                        torn_tail = true;
                        break;
                    }
                }
            }
        }

        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        if fresh {
            options.truncate(true);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| BigcpError::io("open journal for append", error))?;
        if torn_tail {
            file.set_len(valid_length)
                .map_err(|error| BigcpError::io("truncate torn journal tail", error))?;
        }
        file.seek(std::io::SeekFrom::End(0))
            .map_err(|error| BigcpError::io("seek journal", error))?;
        Ok(Self {
            path,
            file: Some(file),
            resumable,
            torn_tail,
            active_job,
            job_record: None,
            skipped_records,
        })
    }

    /// Interior records ignored during load (never trusted, never truncated).
    #[must_use]
    pub const fn skipped_records(&self) -> u64 {
        self.skipped_records
    }

    /// Starts or resumes one semantic job and invalidates incompatible hints.
    pub fn begin_job(
        &mut self,
        run_id: String,
        source: String,
        destination: String,
        options_hash: String,
        timestamp: String,
    ) -> Result<(), BigcpError> {
        let signature = JobSignature {
            source: source.clone(),
            destination: destination.clone(),
            options_hash: options_hash.clone(),
        };
        if self
            .active_job
            .as_ref()
            .is_some_and(|current| current != &signature)
        {
            self.resumable.clear();
        }
        let job = JournalEvent::Job {
            run_id,
            source,
            destination,
            options_hash,
            timestamp,
        };
        self.append(job.clone())?;
        self.active_job = Some(signature);
        self.job_record = Some(job);
        Ok(())
    }

    /// Compacts the journal to the current job header plus live checkpoints.
    ///
    /// Called on clean run end (PLAN section 5.12) so the file never replays
    /// unbounded history. Live checkpoints survive: a canceled run's partials
    /// stay resumable. The synchronized unique sibling is atomically published
    /// so compaction neither overwrites a predictable neighbor nor exposes a
    /// remove/rename gap (the journal remains hints only under I8).
    pub fn compact(&mut self) -> Result<(), BigcpError> {
        let Some(job) = self.job_record.clone() else {
            return Ok(());
        };
        let mut contents = Vec::new();
        contents.extend_from_slice(&encode_record(&job)?);
        for checkpoint in self.resumable.values() {
            contents.extend_from_slice(&encode_record(&JournalEvent::Checkpoint(
                checkpoint.clone(),
            ))?);
        }
        // Windows does not permit replacement while this ordinary append
        // handle is open. Closing only our handle keeps the final path intact
        // until the subsequent atomic replace.
        drop(self.file.take());
        let publish = crate::artifact::write_atomic_bytes(&self.path, "journal", &contents)
            .map_err(|error| BigcpError::io("publish compacted journal", error));
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let mut reopened = options
            .open(&self.path)
            .map_err(|error| BigcpError::io("reopen compacted journal", error))?;
        reopened
            .seek(std::io::SeekFrom::End(0))
            .map_err(|error| BigcpError::io("seek compacted journal", error))?;
        self.file = Some(reopened);
        publish
    }

    /// Appends one CRC-tagged record and flushes it.
    pub fn append(&mut self, event: JournalEvent) -> Result<(), BigcpError> {
        let state_event = event.clone();
        let line = encode_record(&event)?;
        let file = self.file.as_mut().ok_or_else(|| {
            BigcpError::Invariant("journal append handle is unavailable".to_owned())
        })?;
        let mut before = file
            .seek(std::io::SeekFrom::End(0))
            .map_err(|error| BigcpError::io("seek journal append", error))?;
        // Self-heal a missing trailing newline (an interrupted append whose
        // rollback also failed): appending onto an unterminated line would
        // silently corrupt both records on the next load.
        if before > 0 {
            let mut last = [0_u8; 1];
            file.seek(std::io::SeekFrom::Start(before - 1))
                .and_then(|_| file.read_exact(&mut last))
                .map_err(|error| BigcpError::io("probe journal tail", error))?;
            file.seek(std::io::SeekFrom::End(0))
                .map_err(|error| BigcpError::io("seek journal append", error))?;
            if last[0] != b'\n' {
                file.write_all(b"\n")
                    .map_err(|error| BigcpError::io("heal journal tail", error))?;
                before = before.saturating_add(1);
            }
        }
        if let Err(error) = file.write_all(&line) {
            let _ = file.set_len(before);
            let _ = file.seek(std::io::SeekFrom::Start(before));
            return Err(BigcpError::io("append journal", error));
        }
        file.flush()
            .map_err(|error| BigcpError::io("flush journal", error))?;
        match state_event {
            JournalEvent::Checkpoint(checkpoint) => {
                self.resumable.insert(
                    (checkpoint.relative_path.clone(), checkpoint.stream.clone()),
                    checkpoint,
                );
            }
            JournalEvent::PartDone { relative_path } => {
                self.resumable.retain(|(path, _), _| path != &relative_path);
            }
            JournalEvent::Job { .. } | JournalEvent::End { .. } => {}
        }
        Ok(())
    }

    /// Returns the last valid checkpoint for a relative stream.
    #[must_use]
    pub fn checkpoint(&self, relative_path: &str, stream: &str) -> Option<&Checkpoint> {
        self.resumable
            .get(&(relative_path.to_owned(), stream.to_owned()))
    }

    /// Clones the last valid hint so callers can append while evaluating it.
    #[must_use]
    pub fn checkpoint_owned(&self, relative_path: &str, stream: &str) -> Option<Checkpoint> {
        self.checkpoint(relative_path, stream).cloned()
    }

    /// Reports whether loading discarded a torn tail.
    #[must_use]
    pub const fn had_torn_tail(&self) -> bool {
        self.torn_tail
    }

    /// Returns the journal path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Applies one complete record. `false` means the final record was invalid and
/// must be truncated; invalid interior records are counted and retained so a
/// newer additive format cannot destroy later valid checkpoints.
fn apply_loaded_line(
    raw: &str,
    is_last: bool,
    resumable: &mut BTreeMap<(String, String), Checkpoint>,
    active_job: &mut Option<JobSignature>,
    skipped_records: &mut u64,
) -> Result<bool, BigcpError> {
    let line = raw.trim_end_matches(['\r', '\n']);
    let parsed = serde_json::from_str::<StoredRecord>(line)
        .ok()
        .filter(|stored| crc_matches(stored).unwrap_or(false));
    let Some(stored) = parsed else {
        if is_last {
            return Ok(false);
        }
        *skipped_records = skipped_records.saturating_add(1);
        return Ok(true);
    };
    if stored.j != 1 {
        return Err(BigcpError::Format(format!(
            "unsupported journal version {}; this build supports 1",
            stored.j
        )));
    }
    match stored.event {
        JournalEvent::Job {
            source,
            destination,
            options_hash,
            ..
        } => {
            let signature = JobSignature {
                source,
                destination,
                options_hash,
            };
            if active_job
                .as_ref()
                .is_some_and(|current| current != &signature)
            {
                resumable.clear();
            }
            *active_job = Some(signature);
        }
        JournalEvent::Checkpoint(checkpoint) => {
            resumable.insert(
                (checkpoint.relative_path.clone(), checkpoint.stream.clone()),
                checkpoint,
            );
        }
        JournalEvent::PartDone { relative_path } => {
            resumable.retain(|(path, _), _| path != &relative_path);
        }
        JournalEvent::End { .. } => {}
    }
    Ok(true)
}

/// Serializes one CRC-framed journal line (shared by append and compaction).
fn encode_record(event: &JournalEvent) -> Result<Vec<u8>, BigcpError> {
    let unsigned = UnsignedRecord {
        j: 1,
        event: event.clone(),
    };
    let bytes = serde_json::to_vec(&unsigned)
        .map_err(|error| BigcpError::Format(format!("serialize journal CRC input: {error}")))?;
    let stored = StoredRecord {
        j: 1,
        event: event.clone(),
        crc: format!("{:08x}", crc32c::crc32c(&bytes)),
    };
    let mut line = serde_json::to_vec(&stored)
        .map_err(|error| BigcpError::Format(format!("serialize journal record: {error}")))?;
    line.push(b'\n');
    Ok(line)
}

fn crc_matches(stored: &StoredRecord) -> Result<bool, BigcpError> {
    let unsigned = UnsignedRecord {
        j: stored.j,
        event: stored.event.clone(),
    };
    let bytes = serde_json::to_vec(&unsigned)
        .map_err(|error| BigcpError::Format(format!("serialize journal CRC input: {error}")))?;
    Ok(stored.crc == format!("{:08x}", crc32c::crc32c(&bytes)))
}

/// Produces a collision-free journal key for an arbitrary Windows path.
#[must_use]
pub fn path_key(path: &Path) -> String {
    let mut bytes = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    format!("u16:{}", hex::encode(bytes))
}

/// Produces a collision-free journal key for an arbitrary stream suffix.
#[must_use]
pub fn stream_key(stream: &std::ffi::OsStr) -> String {
    let mut bytes = Vec::new();
    for unit in stream.encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    format!("u16:{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::{Checkpoint, CheckpointFileIdentity, Journal, JournalEvent};
    use bigcp_win::FileIdentity;
    use std::fs;

    #[test]
    fn old_checkpoint_without_temp_identity_is_parseable_but_not_resumable() {
        let checkpoint = serde_json::from_value::<Checkpoint>(serde_json::json!({
            "relative_path": "u16:00",
            "stream": "",
            "temp_name": ".bigcp-old-000000000000.part",
            "source_size": 8,
            "source_mtime": 9,
            "watermark": 4,
            "prefix_digest": "xxh3:00000000000000000000000000000000"
        }));
        assert!(checkpoint.is_ok());
        assert!(
            checkpoint.ok().is_some_and(
                |value| value.temp_identity.is_none() && value.source_identity.is_none()
            )
        );
    }

    #[test]
    fn checkpoint_identity_requires_both_volume_and_file_id() {
        let original = FileIdentity {
            volume_serial: 7,
            file_id: [3; 16],
        };
        let recorded = CheckpointFileIdentity::from_file(original);
        assert!(recorded.matches(original));
        assert!(!recorded.matches(FileIdentity {
            volume_serial: 8,
            ..original
        }));
        assert!(!recorded.matches(FileIdentity {
            file_id: [4; 16],
            ..original
        }));
    }

    #[test]
    fn unsupported_journal_version_is_rejected_without_truncation() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let path = directory.path().join("journal.jsonl");
        let event = JournalEvent::End {
            run_id: "future".to_owned(),
        };
        let unsigned = super::UnsignedRecord {
            j: 2,
            event: event.clone(),
        };
        let serialized = serde_json::to_vec(&unsigned);
        assert!(serialized.is_ok());
        let Some(serialized) = serialized.ok() else {
            return;
        };
        let stored = super::StoredRecord {
            j: 2,
            event,
            crc: format!("{:08x}", crc32c::crc32c(&serialized)),
        };
        let mut bytes = serde_json::to_vec(&stored).unwrap_or_default();
        bytes.push(b'\n');
        assert!(fs::write(&path, &bytes).is_ok());
        assert!(Journal::open(path.clone(), false).is_err());
        assert_eq!(fs::read(path).ok(), Some(bytes));
    }

    #[test]
    fn torn_tail_never_panics_or_trusts_following_records() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let path = sandbox.path().join("journal.jsonl");
        let journal = Journal::open(path.clone(), false);
        assert!(journal.is_ok());
        let Some(mut journal) = journal.ok() else {
            return;
        };
        assert!(
            journal
                .append(JournalEvent::End {
                    run_id: "one".to_owned()
                })
                .is_ok()
        );
        drop(journal);
        let append = OpenOptionsForTest::append(&path, b"{broken");
        assert!(append.is_ok());
        let loaded = Journal::open(path, false);
        assert!(loaded.is_ok_and(|value| value.had_torn_tail()));
    }

    #[test]
    fn every_byte_truncation_is_safe_to_load() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let complete = sandbox.path().join("complete.jsonl");
        let journal = Journal::open(complete.clone(), false);
        assert!(journal.is_ok());
        let Some(mut journal) = journal.ok() else {
            return;
        };
        assert!(
            journal
                .append(JournalEvent::End {
                    run_id: "complete".to_owned(),
                })
                .is_ok()
        );
        drop(journal);
        let bytes = fs::read(&complete);
        assert!(bytes.is_ok());
        let Some(bytes) = bytes.ok() else {
            return;
        };
        for offset in 0..bytes.len() {
            let candidate = sandbox.path().join(format!("truncated-{offset}.jsonl"));
            assert!(fs::write(&candidate, &bytes[..offset]).is_ok());
            assert!(
                Journal::open(candidate, false).is_ok(),
                "loader rejected truncation at byte {offset}"
            );
        }
    }

    struct OpenOptionsForTest;

    impl OpenOptionsForTest {
        fn append(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            use std::io::Write;
            let mut file = fs::OpenOptions::new().append(true).open(path)?;
            file.write_all(bytes)
        }
    }

    fn sample_checkpoint(relative_path: &str) -> Checkpoint {
        Checkpoint {
            relative_path: relative_path.to_owned(),
            stream: String::new(),
            temp_name: ".bigcp-test-000000000000.part".to_owned(),
            temp_identity: Some(CheckpointFileIdentity::from_file(FileIdentity {
                volume_serial: 1,
                file_id: [1; 16],
            })),
            source_identity: Some(CheckpointFileIdentity::from_file(FileIdentity {
                volume_serial: 1,
                file_id: [2; 16],
            })),
            source_size: 64,
            source_mtime: 7,
            watermark: 32,
            prefix_digest: "xxh3:00000000000000000000000000000000".to_owned(),
        }
    }

    fn open_job_journal(path: &std::path::Path) -> Option<Journal> {
        let journal = Journal::open(path.to_path_buf(), false);
        assert!(journal.is_ok());
        let mut journal = journal.ok()?;
        let begun = journal.begin_job(
            "run".to_owned(),
            "u16:source".to_owned(),
            "u16:destination".to_owned(),
            "resume-protocol-v1".to_owned(),
            "2026-07-29T00:00:00Z".to_owned(),
        );
        assert!(begun.is_ok());
        Some(journal)
    }

    #[test]
    fn clean_end_compaction_keeps_job_header_and_live_checkpoints() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let path = directory.path().join("journal.jsonl");
        let Some(mut journal) = open_job_journal(&path) else {
            return;
        };
        let live = sample_checkpoint("u16:live");
        let retired = sample_checkpoint("u16:retired");
        assert!(journal.append(JournalEvent::Checkpoint(live)).is_ok());
        assert!(journal.append(JournalEvent::Checkpoint(retired)).is_ok());
        assert!(
            journal
                .append(JournalEvent::PartDone {
                    relative_path: "u16:retired".to_owned(),
                })
                .is_ok()
        );
        assert!(
            journal
                .append(JournalEvent::End {
                    run_id: "run".to_owned(),
                })
                .is_ok()
        );
        assert!(journal.compact().is_ok());
        drop(journal);
        let contents = fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<_> = contents.lines().collect();
        // Job header plus exactly the still-live checkpoint survive; the
        // retired checkpoint, its PartDone, and the End record are gone.
        assert_eq!(lines.len(), 2, "compacted journal: {lines:?}");
        assert!(lines[0].contains("\"ev\":\"job\""));
        assert!(lines[1].contains("u16:live"));
        let reloaded = Journal::open(path, false);
        assert!(reloaded.is_ok());
        assert!(
            reloaded
                .ok()
                .is_some_and(|journal| journal.checkpoint("u16:live", "").is_some())
        );
    }

    #[test]
    fn compaction_never_overwrites_the_old_predictable_sibling_name() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let path = directory.path().join("journal.jsonl");
        let old_temporary = path.with_extension("compact-tmp");
        assert!(fs::write(&old_temporary, b"unrelated sentinel").is_ok());
        let Some(mut journal) = open_job_journal(&path) else {
            return;
        };
        assert!(journal.compact().is_ok());
        assert_eq!(
            fs::read(&old_temporary).ok(),
            Some(b"unrelated sentinel".to_vec())
        );
        let entries = fs::read_dir(directory.path())
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            entries
                .iter()
                .all(|name| !name.starts_with(".bigcp-journal-")),
            "leftover compaction temporary: {entries:?}"
        );
    }

    #[test]
    fn interior_bad_record_is_skipped_without_truncating_later_records() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let path = directory.path().join("journal.jsonl");
        let Some(mut journal) = open_job_journal(&path) else {
            return;
        };
        assert!(
            journal
                .append(JournalEvent::Checkpoint(sample_checkpoint("u16:kept")))
                .is_ok()
        );
        drop(journal);
        // Simulate an additive newer-build record (or corruption) BETWEEN two
        // valid records: it must be ignored, never used as a truncation point
        // that would destroy the later valid checkpoint.
        let original = fs::read_to_string(&path).unwrap_or_default();
        let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
        assert_eq!(lines.len(), 2);
        lines.insert(
            1,
            "{\"j\":1,\"ev\":\"job\",\"crc\":\"deadbeef\"}".to_owned(),
        );
        let rewritten = lines.join(
            "
",
        ) + "
";
        assert!(fs::write(&path, &rewritten).is_ok());
        let reloaded = Journal::open(path.clone(), false);
        assert!(reloaded.is_ok());
        let Some(reloaded) = reloaded.ok() else {
            return;
        };
        assert_eq!(reloaded.skipped_records(), 1);
        assert!(!reloaded.had_torn_tail());
        assert!(reloaded.checkpoint("u16:kept", "").is_some());
        drop(reloaded);
        let after = fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(after, rewritten, "interior skip must not rewrite the file");
    }
}
