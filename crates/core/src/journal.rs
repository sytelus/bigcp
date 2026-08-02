//! CRC-tagged append-only checkpoint journal.
//!
//! Journal records can accelerate resume but can never authorize a completed
//! skip. Callers must validate source metadata and digest the temporary prefix
//! before trusting any checkpoint.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::BigcpError;

// A serialized record contains at most a handful of Windows extended-length
// paths plus fixed metadata. One MiB is comfortably above that legitimate
// ceiling while preventing a corrupt journal without a newline from growing
// replay memory until allocation aborts.
const MAX_JOURNAL_RECORD_BYTES: usize = 1024 * 1024;

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

/// One orphaned temporary the journal can prove bigcp created.
///
/// Produced when checkpoints become unreachable — `--fresh` truncation or a
/// job-signature change — instead of dropping the proof of creation
/// silently. The name plus the recorded filesystem identity let the extras
/// scan reclaim exactly the object bigcp wrote; identity-less version-one
/// records are never harvested because they cannot be proven.
#[derive(Clone, Debug)]
pub(crate) struct ReclaimableTemp {
    /// Opaque sibling temporary name recorded by the checkpoint.
    pub temp_name: String,
    /// Filesystem identity captured when bigcp created the temporary.
    pub temp_identity: CheckpointFileIdentity,
    /// Journal key of the final file the temporary was staged for.
    pub relative_path: String,
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
    /// Temps whose checkpoints became unreachable (`--fresh` truncation or a
    /// job-signature change) but whose creation the journal proved with a
    /// recorded identity. Never resumable; only the identity-verified
    /// orphan-reclamation path in the extras scan may consume these.
    reclaimable: Vec<ReclaimableTemp>,
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

/// State recovered from an existing journal file by the tolerant loader.
#[derive(Default)]
struct LoadedState {
    resumable: BTreeMap<(String, String), Checkpoint>,
    reclaimable: Vec<ReclaimableTemp>,
    torn_tail: bool,
    valid_length: u64,
    active_job: Option<JobSignature>,
    skipped_records: u64,
}

impl LoadedState {
    /// Converts every temp this file proves bigcp created into harvest
    /// entries: the surviving resumable checkpoints join whatever earlier
    /// job-signature changes already harvested during replay. Used by
    /// `--fresh`, which is about to truncate the file and would otherwise
    /// destroy the proof along with the hints.
    fn into_reclaimable(mut self) -> Vec<ReclaimableTemp> {
        harvest_reclaimable(&mut self.resumable, &mut self.reclaimable);
        self.reclaimable
    }
}

/// Moves every identity-bearing checkpoint into the reclaimable harvest.
///
/// Identity-less version-one records are dropped exactly as before: without
/// a recorded filesystem identity nothing can ever prove the object on disk
/// is the temp bigcp wrote, so they must stay ordinary (reported, never
/// deleted) extras. Streams of one file share one temp, hence the
/// name-level deduplication.
fn harvest_reclaimable(
    resumable: &mut BTreeMap<(String, String), Checkpoint>,
    reclaimable: &mut Vec<ReclaimableTemp>,
) {
    for checkpoint in std::mem::take(resumable).into_values() {
        let Some(identity) = checkpoint.temp_identity else {
            continue;
        };
        if reclaimable
            .iter()
            .any(|existing| existing.temp_name == checkpoint.temp_name)
        {
            continue;
        }
        reclaimable.push(ReclaimableTemp {
            temp_name: checkpoint.temp_name,
            temp_identity: identity,
            relative_path: checkpoint.relative_path,
        });
    }
}

/// Replays one journal file through the tolerant loader.
fn load_state(path: &Path) -> Result<LoadedState, BigcpError> {
    let mut state = LoadedState::default();
    let mut reader =
        BufReader::new(File::open(path).map_err(|error| BigcpError::io("open journal", error))?);
    // One-record lookahead distinguishes a torn final append from an
    // invalid interior record without retaining the complete journal
    // in memory. Clean-run compaction bounds history in normal use;
    // this also keeps interrupted-run replay O(one record).
    let mut pending: Option<(Vec<u8>, u64, bool)> = None;
    loop {
        match read_bounded_record(&mut reader) {
            Ok(None) => {
                if let Some((raw, bytes, oversized)) = pending.take() {
                    if apply_loaded_line(&raw, oversized, true, &mut state)? {
                        state.valid_length = state.valid_length.saturating_add(bytes);
                    } else {
                        state.torn_tail = true;
                    }
                }
                break;
            }
            Ok(Some((line, bytes, oversized))) => {
                if let Some((raw, previous_bytes, previous_oversized)) =
                    pending.replace((line, bytes, oversized))
                {
                    let retained = apply_loaded_line(&raw, previous_oversized, false, &mut state)?;
                    debug_assert!(retained, "an interior record is always retained");
                    state.valid_length = state.valid_length.saturating_add(previous_bytes);
                }
            }
            Err(error) => {
                // A transient read failure (filter driver, marginal
                // sector) must not rewrite history: truncation is
                // reserved for a proven torn tail, and destroying
                // every unread record could discard valid resume
                // hints for multi-hundred-GB partials. Failing the
                // load makes the caller disable journaling for this
                // run while the file stays intact for a later run.
                return Err(BigcpError::io("read journal", error));
            }
        }
    }
    Ok(state)
}

impl Journal {
    /// Loads a journal, truncating only an invalid final record and ignoring
    /// invalid interior records without trusting them.
    pub fn open(path: PathBuf, fresh: bool) -> Result<Self, BigcpError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| BigcpError::io("create journal parent", error))?;
        }
        let mut loaded = LoadedState::default();
        if path.exists() {
            if fresh {
                // `--fresh` keeps its historical robustness: it never failed
                // on an unreadable or future-version journal (it simply
                // truncated), so a load error here only skips the harvest —
                // the truncation below still happens and the run still gets
                // a working journal. A successful load, however, must not
                // silently discard the proof of what bigcp created: every
                // identity-bearing checkpoint becomes a reclaimable-temp
                // record before the file is emptied, while `resumable`
                // stays empty (fresh resume semantics are unchanged).
                if let Ok(previous) = load_state(&path) {
                    loaded.reclaimable = previous.into_reclaimable();
                }
            } else {
                loaded = load_state(&path)?;
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
        if loaded.torn_tail {
            file.set_len(loaded.valid_length)
                .map_err(|error| BigcpError::io("truncate torn journal tail", error))?;
        }
        file.seek(std::io::SeekFrom::End(0))
            .map_err(|error| BigcpError::io("seek journal", error))?;
        Ok(Self {
            path,
            file: Some(file),
            resumable: loaded.resumable,
            reclaimable: loaded.reclaimable,
            torn_tail: loaded.torn_tail,
            active_job: loaded.active_job,
            job_record: None,
            skipped_records: loaded.skipped_records,
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
            // Mirror the loader's signature-change handling: the cleared
            // checkpoints' provable temps become reclaimable orphan records
            // instead of silently permanent destination residents.
            harvest_reclaimable(&mut self.resumable, &mut self.reclaimable);
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

    /// Returns the resumable checkpoint recorded for a destination child
    /// name: the journal key of the final file it was staged for plus the
    /// recorded temp identity (absent on identity-less version-one records).
    ///
    /// Such a temp is bigcp's own asset. The extras scan uses this to decide
    /// between live (a source child will consume it — skip silently),
    /// provably orphaned (reclaimable only under the identity proof), and
    /// unproven (reported, never deleted).
    #[must_use]
    pub(crate) fn resume_temp_record(
        &self,
        name: &str,
    ) -> Option<(&str, Option<&CheckpointFileIdentity>)> {
        self.resumable
            .values()
            .find(|checkpoint| checkpoint.temp_name == name)
            .map(|checkpoint| {
                (
                    checkpoint.relative_path.as_str(),
                    checkpoint.temp_identity.as_ref(),
                )
            })
    }

    /// Removes and returns the harvested orphan record for one destination
    /// child name, but only when the record's final path lies in
    /// `directory`: a checkpoint speaks only for the directory that holds
    /// its final file, so a same-named object anywhere else can neither
    /// claim nor consume the proof.
    pub(crate) fn take_reclaimable_temp(
        &mut self,
        name: &str,
        directory: &Path,
    ) -> Option<ReclaimableTemp> {
        let index = self.reclaimable.iter().position(|temp| {
            temp.temp_name == name
                && key_to_path(&temp.relative_path)
                    .is_some_and(|path| path.parent() == Some(directory))
        })?;
        Some(self.reclaimable.swap_remove(index))
    }

    /// Reports whether any resumable checkpoint or reclaimable harvest
    /// exists — the cheap gate that keeps per-directory extras scans free
    /// when there is nothing to decide.
    #[must_use]
    pub(crate) fn has_temp_records(&self) -> bool {
        !self.resumable.is_empty() || !self.reclaimable.is_empty()
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

/// Reads through one complete record while retaining at most the configured
/// parse budget. `consumed` includes bytes discarded after the budget so an
/// invalid interior record can remain byte-for-byte untouched on disk.
fn read_bounded_record(reader: &mut impl BufRead) -> std::io::Result<Option<(Vec<u8>, u64, bool)>> {
    let mut retained = Vec::new();
    let mut consumed = 0_u64;
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((consumed != 0).then_some((retained, consumed, oversized)));
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index.saturating_add(1));
        consumed = consumed
            .checked_add(u64::try_from(end).map_err(std::io::Error::other)?)
            .ok_or_else(|| std::io::Error::other("journal record length overflowed"))?;
        let available = MAX_JOURNAL_RECORD_BYTES.saturating_sub(retained.len());
        let keep = end.min(available);
        if keep > 0 {
            retained.try_reserve(keep).map_err(std::io::Error::other)?;
            retained.extend_from_slice(&buffer[..keep]);
        }
        oversized |= keep < end;
        let record_complete = buffer[..end].last() == Some(&b'\n');
        reader.consume(end);
        if record_complete {
            return Ok(Some((retained, consumed, oversized)));
        }
    }
}

/// Applies one complete record. `false` means the final record was invalid and
/// must be truncated; invalid interior records are counted and retained so a
/// corrupt line cannot destroy later valid checkpoints.
///
/// The version probe reads the raw JSON `j` field *before* the typed parse
/// and CRC filter. A future version's records almost always change shape, so
/// the typed parse (or the lossy reserialize inside the CRC check) would
/// otherwise misfile every real `j: 2` record as generic corruption — the
/// version refusal would be unreachable and a newer build's journal would be
/// silently skipped or truncated instead of preserved.
fn apply_loaded_line(
    raw: &[u8],
    oversized: bool,
    is_last: bool,
    state: &mut LoadedState,
) -> Result<bool, BigcpError> {
    let line = raw.strip_suffix(b"\n").unwrap_or(raw);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let parsed = if oversized {
        None
    } else {
        match serde_json::from_slice::<serde_json::Value>(line) {
            Err(_) => None,
            Ok(value) => {
                let version = value.get("j").and_then(serde_json::Value::as_u64);
                if version.is_some_and(|found| found != u64::from(crate::JOURNAL_SCHEMA_VERSION)) {
                    return Err(BigcpError::Format(format!(
                        "unsupported journal version {}; this build supports {}",
                        version.unwrap_or_default(),
                        crate::JOURNAL_SCHEMA_VERSION
                    )));
                }
                serde_json::from_value::<StoredRecord>(value)
                    .ok()
                    .filter(|stored| crc_matches(stored).unwrap_or(false))
            }
        }
    };
    let Some(stored) = parsed else {
        if is_last {
            return Ok(false);
        }
        state.skipped_records = state.skipped_records.saturating_add(1);
        return Ok(true);
    };
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
            if state
                .active_job
                .as_ref()
                .is_some_and(|current| current != &signature)
            {
                // A signature change makes every prior checkpoint
                // unreachable; keep the provable ones as reclaimable
                // orphan records instead of dropping the proof that
                // bigcp created their temporaries.
                harvest_reclaimable(&mut state.resumable, &mut state.reclaimable);
            }
            state.active_job = Some(signature);
        }
        JournalEvent::Checkpoint(checkpoint) => {
            state.resumable.insert(
                (checkpoint.relative_path.clone(), checkpoint.stream.clone()),
                checkpoint,
            );
        }
        JournalEvent::PartDone { relative_path } => {
            state
                .resumable
                .retain(|(path, _), _| path != &relative_path);
        }
        JournalEvent::End { .. } => {}
    }
    Ok(true)
}

/// Serializes one CRC-framed journal line (shared by append and compaction).
fn encode_record(event: &JournalEvent) -> Result<Vec<u8>, BigcpError> {
    let unsigned = UnsignedRecord {
        j: crate::JOURNAL_SCHEMA_VERSION,
        event: event.clone(),
    };
    let bytes = serde_json::to_vec(&unsigned)
        .map_err(|error| BigcpError::Format(format!("serialize journal CRC input: {error}")))?;
    let stored = StoredRecord {
        j: crate::JOURNAL_SCHEMA_VERSION,
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

/// Encodes an OS string as lossless little-endian UTF-16 hex.
///
/// Shared by the journal keys below and the audit log's `path_raw` field so
/// every lossless-path representation in the state artifacts uses one
/// encoding.
pub(crate) fn utf16le_hex(value: &std::ffi::OsStr) -> String {
    let mut bytes = Vec::new();
    for unit in value.encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    hex::encode(bytes)
}

/// Produces a collision-free journal key for an arbitrary Windows path.
#[must_use]
pub fn path_key(path: &Path) -> String {
    format!("u16:{}", utf16le_hex(path.as_os_str()))
}

/// Produces a collision-free journal key for an arbitrary stream suffix.
#[must_use]
pub fn stream_key(stream: &std::ffi::OsStr) -> String {
    format!("u16:{}", utf16le_hex(stream))
}

/// Decodes a `u16:` journal key back into the exact OS path it encoded.
///
/// Returns `None` for anything that is not a well-formed key; callers treat
/// that as "does not match", never as an error.
#[must_use]
pub(crate) fn key_to_path(key: &str) -> Option<PathBuf> {
    let hex = key.strip_prefix("u16:")?;
    let bytes = hex::decode(hex).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(PathBuf::from(std::ffi::OsString::from_wide(&units)))
}

#[cfg(test)]
mod tests {
    use super::{
        Checkpoint, CheckpointFileIdentity, Journal, JournalEvent, MAX_JOURNAL_RECORD_BYTES,
    };
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
        assert_eq!(fs::read(&path).ok(), Some(bytes));

        // A realistic future record does not round-trip through this build's
        // types (its CRC input differs), so the version refusal must fire on
        // the raw `j` field, before the CRC filter can misfile the record as
        // generic corruption and truncate or skip it.
        let future = b"{\"j\":2,\"ev\":\"novel_record\",\"payload\":true,\"crc\":\"00000000\"}\n";
        assert!(fs::write(&path, future).is_ok());
        assert!(Journal::open(path.clone(), false).is_err());
        assert_eq!(fs::read(path).ok(), Some(future.to_vec()));
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

    #[test]
    fn non_utf8_interior_record_preserves_later_valid_checkpoints() {
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

        let original = fs::read(&path).unwrap_or_default();
        let Some(first_line_end) = original.iter().position(|byte| *byte == b'\n') else {
            return;
        };
        let split = first_line_end.saturating_add(1);
        let mut rewritten = Vec::with_capacity(original.len().saturating_add(2));
        rewritten.extend_from_slice(&original[..split]);
        rewritten.extend_from_slice(&[0xff, b'\n']);
        rewritten.extend_from_slice(&original[split..]);
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
        assert_eq!(fs::read(path).ok(), Some(rewritten));
    }

    #[test]
    fn oversized_interior_record_is_bounded_and_preserves_later_checkpoints() {
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
                .append(JournalEvent::Checkpoint(sample_checkpoint("u16:later")))
                .is_ok()
        );
        drop(journal);

        let original = fs::read(&path).unwrap_or_default();
        let Some(first_line_end) = original.iter().position(|byte| *byte == b'\n') else {
            return;
        };
        let split = first_line_end.saturating_add(1);
        let oversized = vec![b'x'; MAX_JOURNAL_RECORD_BYTES.saturating_add(1)];
        let mut rewritten = Vec::with_capacity(
            original
                .len()
                .saturating_add(oversized.len())
                .saturating_add(1),
        );
        rewritten.extend_from_slice(&original[..split]);
        rewritten.extend_from_slice(&oversized);
        rewritten.push(b'\n');
        rewritten.extend_from_slice(&original[split..]);
        assert!(fs::write(&path, &rewritten).is_ok());

        let reloaded = Journal::open(path.clone(), false);
        assert!(reloaded.is_ok());
        let Some(reloaded) = reloaded.ok() else {
            return;
        };
        assert_eq!(reloaded.skipped_records(), 1);
        assert!(!reloaded.had_torn_tail());
        assert!(reloaded.checkpoint("u16:later", "").is_some());
        drop(reloaded);
        assert_eq!(fs::read(path).ok(), Some(rewritten));
    }

    #[test]
    fn fresh_open_harvests_identity_bearing_checkpoints_only() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let path = directory.path().join("journal.jsonl");
        let Some(mut journal) = open_job_journal(&path) else {
            return;
        };
        let proven_key = super::path_key(std::path::Path::new("proven.bin"));
        let unproven_key = super::path_key(std::path::Path::new("unproven.bin"));
        let mut proven = sample_checkpoint(&proven_key);
        proven.temp_name = ".bigcp-fresh-00000000000a.part".to_owned();
        assert!(journal.append(JournalEvent::Checkpoint(proven)).is_ok());
        let mut unproven = sample_checkpoint(&unproven_key);
        unproven.temp_name = ".bigcp-fresh-00000000000b.part".to_owned();
        unproven.temp_identity = None;
        assert!(journal.append(JournalEvent::Checkpoint(unproven)).is_ok());
        drop(journal);

        let reopened = Journal::open(path.clone(), true);
        assert!(reopened.is_ok());
        let Some(mut reopened) = reopened.ok() else {
            return;
        };
        // Fresh semantics are unchanged: nothing is resumable and the file
        // itself was truncated.
        assert!(reopened.checkpoint(&proven_key, "").is_none());
        assert!(reopened.checkpoint(&unproven_key, "").is_none());
        assert_eq!(fs::read(&path).ok().map(|bytes| bytes.len()), Some(0));
        // Only the identity-bearing checkpoint is harvested as provable...
        let root = std::path::Path::new("");
        assert!(
            reopened
                .take_reclaimable_temp(".bigcp-fresh-00000000000b.part", root)
                .is_none()
        );
        let taken = reopened.take_reclaimable_temp(".bigcp-fresh-00000000000a.part", root);
        assert!(taken.is_some_and(|temp| temp.relative_path == proven_key));
        // ...and taking removes: a second take finds nothing.
        assert!(
            reopened
                .take_reclaimable_temp(".bigcp-fresh-00000000000a.part", root)
                .is_none()
        );
    }

    #[test]
    fn reclaimable_take_requires_the_recording_directory() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let path = directory.path().join("journal.jsonl");
        let Some(mut journal) = open_job_journal(&path) else {
            return;
        };
        let nested_key = super::path_key(&std::path::Path::new("nested").join("large.bin"));
        let mut nested = sample_checkpoint(&nested_key);
        nested.temp_name = ".bigcp-nested-00000000000c.part".to_owned();
        assert!(journal.append(JournalEvent::Checkpoint(nested)).is_ok());
        drop(journal);

        let reopened = Journal::open(path, true);
        assert!(reopened.is_ok());
        let Some(mut reopened) = reopened.ok() else {
            return;
        };
        // A checkpoint speaks only for the directory holding its final
        // path: the wrong directory can neither claim nor consume it.
        assert!(
            reopened
                .take_reclaimable_temp(".bigcp-nested-00000000000c.part", std::path::Path::new(""))
                .is_none()
        );
        assert!(
            reopened
                .take_reclaimable_temp(
                    ".bigcp-nested-00000000000c.part",
                    std::path::Path::new("nested")
                )
                .is_some()
        );
    }

    #[test]
    fn job_signature_change_moves_checkpoints_to_reclaimable() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let path = directory.path().join("journal.jsonl");
        let Some(mut journal) = open_job_journal(&path) else {
            return;
        };
        let key = super::path_key(std::path::Path::new("large.bin"));
        let mut checkpoint = sample_checkpoint(&key);
        checkpoint.temp_name = ".bigcp-sig-000000000001.part".to_owned();
        assert!(journal.append(JournalEvent::Checkpoint(checkpoint)).is_ok());
        // A new job with a different destination makes the hint unreachable,
        // but the proof of creation must survive as a reclaimable record.
        let begun = journal.begin_job(
            "run2".to_owned(),
            "u16:source".to_owned(),
            "u16:elsewhere".to_owned(),
            "resume-protocol-v1".to_owned(),
            "2026-08-01T00:00:00Z".to_owned(),
        );
        assert!(begun.is_ok());
        assert!(journal.checkpoint(&key, "").is_none());
        let root = std::path::Path::new("");
        let taken = journal.take_reclaimable_temp(".bigcp-sig-000000000001.part", root);
        assert!(taken.is_some_and(|temp| temp.relative_path == key));
        assert!(
            journal
                .take_reclaimable_temp(".bigcp-sig-000000000001.part", root)
                .is_none()
        );
        drop(journal);

        // The same harvest happens at load time when the signature change is
        // replayed from disk: an interrupted run must not lose the proof.
        let reloaded = Journal::open(path, false);
        assert!(reloaded.is_ok());
        let Some(mut reloaded) = reloaded.ok() else {
            return;
        };
        assert!(reloaded.checkpoint(&key, "").is_none());
        assert!(
            reloaded
                .take_reclaimable_temp(".bigcp-sig-000000000001.part", root)
                .is_some()
        );
    }
}
