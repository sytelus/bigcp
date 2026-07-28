//! Versioned JSONL audit logging with reopen and state-directory failover.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use time::OffsetDateTime;

use crate::error::{BigcpError, OperationError};
use crate::model::Counters;
use crate::{LOG_SCHEMA_VERSION, options::CopyOptions};

/// A display path plus lossless UTF-16 when lossy conversion was necessary.
#[derive(Clone, Debug, Serialize)]
pub struct AuditPath {
    /// Human-readable UTF-8 representation.
    pub path: String,
    /// Little-endian UTF-16 bytes in hex, present only for lossy names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_raw: Option<String>,
}

impl AuditPath {
    /// Captures a path without losing audit identity.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let display = path.to_string_lossy();
        let round_trips = path.as_os_str().to_str().is_some();
        let path_raw = (!round_trips).then(|| {
            let mut bytes = Vec::new();
            for code_unit in path.as_os_str().encode_wide() {
                bytes.extend_from_slice(&code_unit.to_le_bytes());
            }
            hex::encode(bytes)
        });
        Self {
            path: display.into_owned(),
            path_raw,
        }
    }
}

/// Complete set of stable log event families.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum AuditEvent {
    /// Run configuration and resolved roots.
    RunStart {
        /// Schema version.
        v: u32,
        /// Unique run identifier.
        run_id: String,
        /// Source root.
        source: AuditPath,
        /// Destination root.
        destination: AuditPath,
        /// Dry-run status.
        dry_run: bool,
        /// Verification status.
        verify: bool,
        /// Replacement authorization.
        replace: bool,
    },
    /// Static profile selected before copy I/O.
    Profile {
        /// Source class.
        source_class: String,
        /// Destination class.
        destination_class: String,
        /// Source queue depth.
        qd_source: usize,
        /// Destination queue depth.
        qd_destination: usize,
        /// Composed chunk bytes.
        chunk_bytes: usize,
        /// Concurrent stream cap.
        streams: usize,
        /// Small-file worker cap.
        workers: usize,
        /// Whether volume extents intersect.
        same_physical_disk: bool,
    },
    /// Directory discovery or completion.
    Directory {
        /// created, exists, stamped, or failed.
        action: String,
        /// Relative object path.
        rel: AuditPath,
    },
    /// File terminal outcome.
    File {
        /// Stable terminal file/link action such as copied, skipped, or failed.
        action: String,
        /// Relative object path.
        rel: AuditPath,
        /// Logical unnamed-stream bytes.
        size: u64,
        /// Optional content digest.
        #[serde(skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        /// Replacement decision detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        replacement: Option<ReplacementEvent>,
        /// Failure detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
        /// Non-error reason.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Non-file object failure with complete operation context.
    Error {
        /// Stable category, operation, path, raw code, message, and repair hint.
        error: OperationError,
    },
    /// Destination-only object.
    Extra {
        /// Relative path.
        rel: AuditPath,
    },
    /// Capability or policy warning.
    Warning {
        /// Stable warning identifier.
        kind: String,
        /// Optional object path.
        #[serde(skip_serializing_if = "Option::is_none")]
        rel: Option<AuditPath>,
        /// Human-readable detail.
        message: String,
    },
    /// Periodic run statistics.
    Stat {
        /// Exact counters at the sample point.
        counters: Counters,
        /// Source read rate.
        read_mbps: f64,
        /// Destination write rate.
        write_mbps: f64,
    },
    /// Run terminal record.
    RunEnd {
        /// Exact final counters.
        counters: Counters,
        /// logical or durable.
        durability: String,
        /// ok, degraded, or failed.
        audit: String,
        /// ok or failed.
        integrity: String,
        /// Process exit contract code.
        exit: i32,
    },
}

/// Fields required to audit an overwrite decision.
#[derive(Clone, Debug, Serialize)]
pub struct ReplacementEvent {
    /// Previous logical size.
    pub old_size: u64,
    /// Previous last-write FILETIME.
    pub old_mtime: i64,
    /// Previous raw attributes.
    pub old_attributes: u32,
    /// Whether the previous destination was newer.
    pub destination_newer: bool,
    /// Fields that caused replacement.
    pub differences: Vec<String>,
}

#[derive(Serialize)]
struct Envelope<'a> {
    ts: String,
    #[serde(flatten)]
    event: &'a AuditEvent,
}

/// Lossless event sink with one same-path reopen and a fallback path.
pub struct AuditWriter {
    current_path: PathBuf,
    primary_path: PathBuf,
    fallback_path: PathBuf,
    file: File,
    degraded: bool,
    last_flush: Instant,
}

impl AuditWriter {
    /// Creates a new log file and its parent directories.
    pub fn create(primary: PathBuf, fallback: PathBuf) -> Result<Self, BigcpError> {
        create_parent(&primary).map_err(|error| BigcpError::io("create log parent", error))?;
        create_parent(&fallback)
            .map_err(|error| BigcpError::io("create fallback log parent", error))?;
        let file = open_log(&primary).map_err(|error| BigcpError::io("open JSONL log", error))?;
        Ok(Self {
            current_path: primary.clone(),
            primary_path: primary,
            fallback_path: fallback,
            file,
            degraded: false,
            last_flush: Instant::now(),
        })
    }

    /// Emits one complete JSON object or fails rather than continuing unaudited.
    pub fn emit(&mut self, event: &AuditEvent) -> Result<(), BigcpError> {
        let envelope = Envelope {
            ts: format_timestamp(OffsetDateTime::now_utc()),
            event,
        };
        let mut line = serde_json::to_vec(&envelope)
            .map_err(|error| BigcpError::Format(format!("serialize log event: {error}")))?;
        line.push(b'\n');
        if let Err(first) = write_atomic_line(&mut self.file, &line) {
            if self.reopen_and_retry(&line).is_err() && self.failover_and_retry(&line).is_err() {
                return Err(BigcpError::Audit(format!(
                    "JSONL log failed at {} and fallback {} was unavailable: {first}",
                    self.primary_path.display(),
                    self.fallback_path.display()
                )));
            }
        }
        if self.last_flush.elapsed() >= Duration::from_secs(2) {
            self.flush()?;
        }
        Ok(())
    }

    /// Flushes all event bytes to the operating system.
    pub fn flush(&mut self) -> Result<(), BigcpError> {
        self.file
            .flush()
            .map_err(|error| BigcpError::io("flush JSONL log", error))?;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Flushes and durably synchronizes the terminal audit record.
    pub fn finish(&mut self) -> Result<(), BigcpError> {
        self.flush()?;
        self.file
            .sync_data()
            .map_err(|error| BigcpError::io("synchronize JSONL log", error))
    }

    /// Returns the actual current audit path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.current_path
    }

    /// Reports whether failover was required.
    #[must_use]
    pub const fn degraded(&self) -> bool {
        self.degraded
    }

    fn reopen_and_retry(&mut self, line: &[u8]) -> io::Result<()> {
        self.file = open_log(&self.current_path)?;
        write_atomic_line(&mut self.file, line)
    }

    fn failover_and_retry(&mut self, line: &[u8]) -> io::Result<()> {
        self.file = open_log(&self.fallback_path)?;
        self.current_path.clone_from(&self.fallback_path);
        self.degraded = true;
        write_atomic_line(&mut self.file, line)
    }
}

/// Returns a compact, audit-safe option summary.
#[must_use]
pub fn option_summary(options: &CopyOptions) -> serde_json::Value {
    serde_json::json!({
        "dry_run": options.dry_run,
        "verify": options.verify,
        "include_system": options.include_system,
        "skip_cloud": options.skip_cloud,
        "replace": options.replace,
        "flush": options.flush,
        "no_sparse": options.no_sparse,
        "no_unbuffered": options.no_unbuffered,
        "raw_reparse": options.raw_reparse,
        "fresh": options.fresh,
        "source_profile": options.source_profile,
        "destination_profile": options.destination_profile,
        "tune": options.tune,
        "log_schema": LOG_SCHEMA_VERSION,
    })
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
}

fn write_atomic_line(file: &mut File, line: &[u8]) -> io::Result<()> {
    let before = file.seek(io::SeekFrom::End(0))?;
    if let Err(error) = file.write_all(line) {
        let _ = file.set_len(before);
        return Err(error);
    }
    Ok(())
}

fn create_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn format_timestamp(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| timestamp.unix_timestamp().to_string())
}
