//! CRC-tagged append-only checkpoint journal.
//!
//! Journal records can accelerate resume but can never authorize a completed
//! skip. Callers must validate source metadata and digest the temporary prefix
//! before trusting any checkpoint.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::BigcpError;

/// One resumable stream checkpoint.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Checkpoint {
    /// Relative display path.
    pub relative_path: String,
    /// Empty for unnamed stream, otherwise the named stream.
    pub stream: String,
    /// Opaque sibling temporary path.
    pub temp_name: String,
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
    file: File,
    resumable: BTreeMap<(String, String), Checkpoint>,
    torn_tail: bool,
    active_job: Option<JobSignature>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JobSignature {
    source: String,
    destination: String,
    options_hash: String,
}

impl Journal {
    /// Loads a journal, dropping a bad line and everything after it.
    pub fn open(path: PathBuf, fresh: bool) -> Result<Self, BigcpError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| BigcpError::io("create journal parent", error))?;
        }
        let mut resumable = BTreeMap::new();
        let mut torn_tail = false;
        let mut valid_length = 0_u64;
        let mut active_job: Option<JobSignature> = None;
        if path.exists() && !fresh {
            let mut reader = BufReader::new(
                File::open(&path).map_err(|error| BigcpError::io("open journal", error))?,
            );
            loop {
                let mut line = String::new();
                let bytes = match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(bytes) => bytes,
                    Err(_) => {
                        torn_tail = true;
                        break;
                    }
                };
                let line = line.trim_end_matches(['\r', '\n']);
                let Ok(stored) = serde_json::from_str::<StoredRecord>(line) else {
                    torn_tail = true;
                    break;
                };
                if !crc_matches(&stored)? {
                    torn_tail = true;
                    break;
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
                        active_job = Some(signature);
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
                valid_length = valid_length.saturating_add(bytes as u64);
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
            file,
            resumable,
            torn_tail,
            active_job,
        })
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
        self.append(JournalEvent::Job {
            run_id,
            source,
            destination,
            options_hash,
            timestamp,
        })?;
        self.active_job = Some(signature);
        Ok(())
    }

    /// Appends one CRC-tagged record and flushes it.
    pub fn append(&mut self, event: JournalEvent) -> Result<(), BigcpError> {
        let state_event = event.clone();
        let unsigned = UnsignedRecord {
            j: 1,
            event: event.clone(),
        };
        let bytes = serde_json::to_vec(&unsigned)
            .map_err(|error| BigcpError::Format(format!("serialize journal CRC input: {error}")))?;
        let stored = StoredRecord {
            j: 1,
            event,
            crc: format!("{:08x}", crc32c::crc32c(&bytes)),
        };
        let mut line = serde_json::to_vec(&stored)
            .map_err(|error| BigcpError::Format(format!("serialize journal record: {error}")))?;
        line.push(b'\n');
        let before = self
            .file
            .seek(std::io::SeekFrom::End(0))
            .map_err(|error| BigcpError::io("seek journal append", error))?;
        if let Err(error) = self.file.write_all(&line) {
            let _ = self.file.set_len(before);
            let _ = self.file.seek(std::io::SeekFrom::Start(before));
            return Err(BigcpError::io("append journal", error));
        }
        self.file
            .flush()
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
    use super::{Journal, JournalEvent};
    use std::fs;

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
}
