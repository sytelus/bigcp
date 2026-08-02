//! Versioned JSONL audit logging with reopen and state-directory failover.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use time::OffsetDateTime;

use crate::error::{BigcpError, OperationError};
use crate::model::Counters;
use crate::report::VerificationSummary;
use crate::stats::AnalysisSummary;
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
        let path_raw = (!round_trips).then(|| crate::journal::utf16le_hex(path.as_os_str()));
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
        /// Source access endpoint.
        source_endpoint: bigcp_win::EndpointKind,
        /// Destination access endpoint.
        destination_endpoint: bigcp_win::EndpointKind,
        /// Composed chunk bytes.
        chunk_bytes: usize,
        /// Small-file worker cap.
        workers: usize,
        /// Whether volume extents intersect.
        same_physical_disk: bool,
        /// Effective transport strategy.
        transport: crate::transport::TransportKind,
        /// Maximum bytes staged per transport phase.
        burst_bytes: usize,
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
        /// Logical source bytes represented by the outcome. For copied
        /// outcomes this includes named streams when the stream set was
        /// opened, matching `Counters::bytes_logical_copied` — it is not
        /// only the unnamed stream.
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
    /// One bounded `--analyze` insight summary per run (VISION analysis flag).
    Analysis {
        /// Size-class aggregates plus slowest-copy samples.
        analysis: AnalysisSummary,
    },
    /// Same-run verification closure.
    Verification {
        /// Complete bounded verification result also stored in the report.
        verification: VerificationSummary,
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
        })
    }

    /// Emits one complete JSON object or fails rather than continuing unaudited.
    pub fn emit(&mut self, event: &AuditEvent) -> Result<(), BigcpError> {
        let line = encode_event(event)
            .map_err(|error| BigcpError::Format(format!("serialize log event: {error}")))?;
        if let Err(first) = write_atomic_line(&mut self.file, &line) {
            let first_message = first.error.to_string();
            let retried = if first.safe_to_retry {
                self.reopen_and_retry(&line)
            } else {
                Err(first)
            };
            if let Err(last) = retried {
                // A partial append that could not be truncated away poisons
                // its file: any further append there would follow torn JSON.
                // A poisoned primary is abandoned by failing over to the
                // distinct fallback path; a poisoned fallback has nowhere
                // safe left, so the run stops making audit claims.
                if !last.safe_to_retry && self.current_path == self.fallback_path {
                    return Err(BigcpError::Audit(format!(
                        "JSONL fallback log {} tore an event that could not be rolled back: {first_message}",
                        self.fallback_path.display()
                    )));
                }
                if let Err(failover) = self.failover_and_retry(&line) {
                    return Err(BigcpError::Audit(format!(
                        "JSONL log failed at {}; failover to {} also failed: {first_message}; {failover}",
                        self.primary_path.display(),
                        self.fallback_path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Confirms every event line has reached the operating system.
    ///
    /// Event lines are written directly on the unbuffered `File`, so bytes
    /// reach the OS on each `emit` and `File::flush` performs no syscall.
    /// This method exists as the explicit ordering point before durability
    /// claims (`finish`) and cancellation summaries; a former 2-second flush
    /// cadence in `emit` was removed because it never moved any bytes.
    pub fn flush(&mut self) -> Result<(), BigcpError> {
        self.file
            .flush()
            .map_err(|error| BigcpError::io("flush JSONL log", error))
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

    fn reopen_and_retry(&mut self, line: &[u8]) -> Result<(), LineWriteFailure> {
        // A failed open appends nothing, so retrying elsewhere stays safe.
        self.file = open_log(&self.current_path).map_err(|error| LineWriteFailure {
            error,
            safe_to_retry: true,
        })?;
        write_atomic_line(&mut self.file, line)
    }

    fn failover_and_retry(&mut self, line: &[u8]) -> io::Result<()> {
        self.file = open_log(&self.fallback_path)?;
        self.current_path.clone_from(&self.fallback_path);
        self.degraded = true;
        // The switch must be visible in both channels (PLAN section 5.15):
        // note it inside the new log stream and on stderr. The notice precedes
        // the retried event so `run_end` remains the terminal record even when
        // that exact append triggers failover. A rolled-back notice failure
        // gets one reopen retry; an unrepairable partial or failed retry closes
        // the run so later events never follow torn JSON.
        let notice = encode_event(&failover_notice(&self.primary_path, &self.fallback_path))
            .map_err(io::Error::other)?;
        if let Err(first) = write_atomic_line(&mut self.file, &notice) {
            if !first.safe_to_retry {
                return Err(first.error);
            }
            self.file = open_log(&self.fallback_path)?;
            write_atomic_line(&mut self.file, &notice).map_err(|failure| failure.error)?;
        }
        write_atomic_line(&mut self.file, line).map_err(|failure| failure.error)?;
        // A closed stderr pipe must not panic and abort an otherwise viable
        // failover. The same notice is already durable in the fallback log.
        let _ = writeln!(
            io::stderr().lock(),
            "bigcp: JSONL log failed over from {} to {}",
            self.primary_path.display(),
            self.fallback_path.display()
        );
        Ok(())
    }
}

fn failover_notice(primary: &Path, fallback: &Path) -> AuditEvent {
    AuditEvent::Warning {
        kind: "log_failover".to_owned(),
        rel: None,
        message: format!(
            "JSONL log failed over from {} to {}",
            primary.display(),
            fallback.display()
        ),
    }
}

fn encode_event(event: &AuditEvent) -> Result<Vec<u8>, serde_json::Error> {
    let envelope = Envelope {
        ts: format_timestamp(OffsetDateTime::now_utc()),
        event,
    };
    let mut line = serde_json::to_vec(&envelope)?;
    line.push(b'\n');
    Ok(line)
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
        "raw_reparse": options.raw_reparse,
        "fresh": options.fresh,
        "accept_degraded_filesystem": options.accept_degraded_filesystem,
        "accept_remote_paths": options.accept_remote_paths,
        "source_profile": options.source_profile,
        "destination_profile": options.destination_profile,
        "tune": options.tune,
        "analyze": options.analyze,
        "log_schema": LOG_SCHEMA_VERSION,
    })
}

fn open_log(path: &Path) -> io::Result<File> {
    // Deliberately write+read, NOT append: a Windows append-mode handle holds
    // FILE_APPEND_DATA without FILE_WRITE_DATA, and `set_len` (the torn-line
    // rollback in write_atomic_line) requires FILE_WRITE_DATA — with append
    // mode every rollback failed, `safe_to_retry` was always false, and the
    // documented one-shot same-path reopen could never trigger. The single
    // coordinator writer seeks to End before each write, which preserves
    // append behavior without the crippled access mask.
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
}

struct LineWriteFailure {
    error: io::Error,
    /// True when no bytes were appended or a successful truncation restored
    /// the exact pre-write length, so reopening the same path cannot append
    /// after a torn JSON object.
    safe_to_retry: bool,
}

fn write_atomic_line(file: &mut File, line: &[u8]) -> Result<(), LineWriteFailure> {
    let before = file
        .seek(io::SeekFrom::End(0))
        .map_err(|error| LineWriteFailure {
            error,
            safe_to_retry: true,
        })?;
    if let Err(error) = file.write_all(line) {
        let safe_to_retry = file.set_len(before).is_ok();
        return Err(LineWriteFailure {
            error,
            safe_to_retry,
        });
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;

    use super::{AuditEvent, AuditWriter, encode_event, failover_notice, open_log};
    use crate::model::Counters;

    #[test]
    fn log_handles_support_the_torn_line_rollback() {
        // Regression: `.append(true)` produced a Windows handle holding
        // FILE_APPEND_DATA without FILE_WRITE_DATA, so the set_len rollback
        // in write_atomic_line always failed and every torn write became
        // unrecoverable. The log handle must be able to truncate itself.
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let path = sandbox.path().join("audit.log.jsonl");
        let file = open_log(&path);
        assert!(file.is_ok());
        let Some(mut file) = file.ok() else {
            return;
        };
        assert!(file.seek(SeekFrom::End(0)).is_ok());
        assert!(file.write_all(b"{\"partial\":").is_ok());
        assert!(file.set_len(0).is_ok(), "rollback truncation must succeed");
        // Appended writes continue after rollback on the same handle.
        assert!(file.seek(SeekFrom::End(0)).is_ok());
        assert!(file.write_all(b"{}\n").is_ok());
        assert_eq!(fs::read(&path).ok(), Some(b"{}\n".to_vec()));
    }

    #[test]
    fn failover_notice_is_a_complete_escaped_event() {
        let event = failover_notice(
            Path::new(r#"C:\primary\odd"name.log"#),
            Path::new(r"C:\fallback\audit.log"),
        );
        let encoded = encode_event(&event);
        assert!(encoded.is_ok());
        let parsed = encoded
            .ok()
            .and_then(|line| serde_json::from_slice::<serde_json::Value>(&line).ok());
        assert!(parsed.as_ref().is_some_and(|value| {
            value
                .get("ts")
                .and_then(serde_json::Value::as_str)
                .is_some()
                && value.get("ev").and_then(serde_json::Value::as_str) == Some("warning")
                && value.get("kind").and_then(serde_json::Value::as_str) == Some("log_failover")
                && value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|message| message.contains("odd\"name.log"))
        }));
    }

    #[test]
    fn failover_notice_precedes_the_retried_terminal_event() {
        let directory = tempfile::tempdir();
        assert!(directory.is_ok());
        let Some(directory) = directory.ok() else {
            return;
        };
        let primary = directory.path().join("primary.jsonl");
        let fallback = directory.path().join("fallback.jsonl");
        let writer = AuditWriter::create(primary, fallback.clone());
        assert!(writer.is_ok());
        let Some(mut writer) = writer.ok() else {
            return;
        };
        let terminal = encode_event(&AuditEvent::RunEnd {
            counters: Counters::default(),
            durability: "logical".to_owned(),
            audit: "degraded".to_owned(),
            integrity: "ok".to_owned(),
            exit: 0,
        });
        assert!(terminal.is_ok());
        let Some(terminal) = terminal.ok() else {
            return;
        };
        assert!(writer.failover_and_retry(&terminal).is_ok());
        drop(writer);

        let contents = fs::read_to_string(fallback);
        assert!(contents.is_ok());
        let Some(contents) = contents.ok() else {
            return;
        };
        let events = contents
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["ev"], "warning");
        assert_eq!(events[0]["kind"], "log_failover");
        assert_eq!(events[1]["ev"], "run_end");
    }

    #[test]
    fn profile_event_contains_only_effective_execution_settings() {
        let encoded = encode_event(&AuditEvent::Profile {
            source_class: "Nvme".to_owned(),
            destination_class: "Unknown".to_owned(),
            source_endpoint: bigcp_win::EndpointKind::Local,
            destination_endpoint: bigcp_win::EndpointKind::Unc,
            chunk_bytes: 8 * 1024 * 1024,
            workers: 32,
            same_physical_disk: false,
            transport: crate::transport::TransportKind::Redirector,
            burst_bytes: 8 * 1024 * 1024,
        });
        assert!(encoded.is_ok());
        let parsed = encoded
            .ok()
            .and_then(|line| serde_json::from_slice::<serde_json::Value>(&line).ok());
        assert!(parsed.as_ref().is_some_and(|value| {
            value.get("workers").and_then(serde_json::Value::as_u64) == Some(32)
                && value.get("transport").and_then(serde_json::Value::as_str) == Some("redirector")
                && value.get("streams").is_none()
                && value.get("enumeration_threads").is_none()
        }));
    }

    #[test]
    fn wsl_transport_has_a_distinct_stable_audit_value() {
        let encoded = encode_event(&AuditEvent::Profile {
            source_class: "Unknown".to_owned(),
            destination_class: "Unknown".to_owned(),
            source_endpoint: bigcp_win::EndpointKind::Wsl,
            destination_endpoint: bigcp_win::EndpointKind::Local,
            chunk_bytes: 8 * 1024 * 1024,
            workers: 16,
            same_physical_disk: false,
            transport: crate::transport::TransportKind::Wsl,
            burst_bytes: 8 * 1024 * 1024,
        });
        assert!(encoded.is_ok());
        assert!(
            encoded
                .ok()
                .and_then(|line| serde_json::from_slice::<serde_json::Value>(&line).ok())
                .is_some_and(|value| {
                    value.get("transport").and_then(serde_json::Value::as_str) == Some("wsl")
                        && value.get("workers").and_then(serde_json::Value::as_u64) == Some(16)
                })
        );
    }
}
