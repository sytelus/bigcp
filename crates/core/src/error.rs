//! Stable error categories and operation-rich failures.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use bigcp_win::is_cloud_file_error;

/// Stable public error taxonomy used by logs, reports, and the TUI.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// Access-control failure.
    Permissions,
    /// Sharing violation or lock.
    Locked,
    /// Invalid, missing, or overlong path.
    Path,
    /// Destination capacity exhausted.
    Space,
    /// Device-reported data or media error.
    Media,
    /// Device disappeared.
    DeviceGone,
    /// Filesystem cannot represent the requested feature.
    FsLimit,
    /// Source changed after enumeration.
    SourceChanged,
    /// Destination changed after classification.
    DestinationChanged,
    /// Unsupported reparse tag.
    UnsupportedReparse,
    /// A parent directory could not be prepared.
    ParentDirFailed,
    /// Source and destination object types conflict.
    TypeConflict,
    /// Cloud hydration failure.
    Cloud,
    /// Unexpected invariant or implementation failure.
    Internal,
}

/// One failed operation with enough context for repair and audit.
#[derive(Clone, Debug, Deserialize, Error, Serialize)]
#[error("{operation} failed for {path}: {message}")]
pub struct OperationError {
    /// Stable error category.
    pub category: ErrorCategory,
    /// Operation name, such as open_src or rename.
    pub operation: String,
    /// Source-root-relative path where possible.
    pub path: PathBuf,
    /// Original Win32 error code when available.
    pub code: Option<i32>,
    /// Operating-system or semantic message.
    pub message: String,
    /// Actionable resolution guidance.
    pub hint: String,
}

impl OperationError {
    /// Classifies one I/O error while preserving its raw code.
    #[must_use]
    pub fn from_io(operation: &str, path: PathBuf, error: &io::Error) -> Self {
        let code = error.raw_os_error();
        let category = category_for(code, error.kind());
        Self::from_io_as(category, operation, path, error)
    }

    pub(crate) fn from_io_as(
        category: ErrorCategory,
        operation: &str,
        path: PathBuf,
        error: &io::Error,
    ) -> Self {
        Self {
            category,
            operation: operation.to_owned(),
            path,
            code: error.raw_os_error(),
            message: error.to_string(),
            hint: hint_for(category).to_owned(),
        }
    }

    /// Builds a semantic error that did not originate from one Win32 call.
    #[must_use]
    pub fn semantic(
        category: ErrorCategory,
        operation: &str,
        path: PathBuf,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category,
            operation: operation.to_owned(),
            path,
            code: None,
            message: message.into(),
            hint: hint_for(category).to_owned(),
        }
    }
}

/// Top-level failure before a reportable per-object outcome can be produced.
#[derive(Debug, Error)]
pub enum BigcpError {
    /// Fatal startup or orchestration I/O.
    #[error("{context}: {source}")]
    Io {
        /// What operation failed.
        context: String,
        /// Original error.
        #[source]
        source: io::Error,
    },
    /// Invalid or unsafe configuration.
    #[error("{0}")]
    Invalid(String),
    /// Machine-wide destination lock is already held.
    #[error("{0}")]
    Locked(String),
    /// Audit stream could not be preserved.
    #[error("{0}")]
    Audit(String),
    /// Counter or state invariant was violated.
    #[error("{0}")]
    Invariant(String),
    /// Stored artifact is malformed.
    #[error("{0}")]
    Format(String),
}

impl BigcpError {
    /// Attaches a concise operation label to an I/O error.
    #[must_use]
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

fn category_for(code: Option<i32>, kind: io::ErrorKind) -> ErrorCategory {
    match code {
        Some(5) => ErrorCategory::Permissions,
        Some(32 | 33) => ErrorCategory::Locked,
        Some(39 | 112) => ErrorCategory::Space,
        Some(23 | 1117) => ErrorCategory::Media,
        // Local removals and remote redirector disconnects both invalidate an
        // in-flight endpoint. Group them so the same five-failure breaker
        // stops a dead share/WSL distribution instead of walking the tree and
        // emitting one doomed operation per object.
        Some(21 | 53 | 55 | 59 | 64 | 67 | 121 | 433 | 1167 | 1222 | 1231 | 1236 | 2250) => {
            ErrorCategory::DeviceGone
        }
        Some(80 | 183) => ErrorCategory::DestinationChanged,
        Some(206) => ErrorCategory::Path,
        Some(code) if is_cloud_file_error(code) => ErrorCategory::Cloud,
        _ => match kind {
            io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => ErrorCategory::Path,
            io::ErrorKind::AlreadyExists => ErrorCategory::DestinationChanged,
            io::ErrorKind::PermissionDenied => ErrorCategory::Permissions,
            _ => ErrorCategory::Internal,
        },
    }
}

fn hint_for(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::Permissions => {
            "Check or repair ACLs, take ownership if appropriate, then re-run"
        }
        ErrorCategory::Locked => "Close the process using this object, then re-run",
        ErrorCategory::Path => "Check the path and shorten the destination root if necessary",
        ErrorCategory::Space => "Free destination space, then re-run to resume",
        ErrorCategory::Media => "Check the drive and its health before re-running",
        ErrorCategory::DeviceGone => "Reconnect the device or remote path and re-run to resume",
        ErrorCategory::FsLimit => {
            "Use a destination filesystem that supports this object type, feature, or size"
        }
        ErrorCategory::SourceChanged => "Quiesce source writers and re-run",
        ErrorCategory::DestinationChanged => "Stop other destination writers and re-run",
        ErrorCategory::UnsupportedReparse => {
            "Use --raw-reparse only when the owning filter is present and the risk is understood"
        }
        ErrorCategory::ParentDirFailed => "Resolve the parent directory failure, then re-run",
        ErrorCategory::TypeConflict => {
            "Resolve the conflicting destination object manually; bigcp never deletes it"
        }
        ErrorCategory::Cloud => "Restore cloud provider connectivity/health or use --skip-cloud",
        ErrorCategory::Internal => "Retain the log and report and file a bigcp bug",
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use super::{ErrorCategory, OperationError};

    #[test]
    fn win32_codes_preserve_raw_context_and_stable_categories() {
        for (code, category) in [
            (5, ErrorCategory::Permissions),
            (32, ErrorCategory::Locked),
            (112, ErrorCategory::Space),
            (23, ErrorCategory::Media),
            (1167, ErrorCategory::DeviceGone),
            (64, ErrorCategory::DeviceGone),
            (1231, ErrorCategory::DeviceGone),
            (206, ErrorCategory::Path),
            (183, ErrorCategory::DestinationChanged),
            (362, ErrorCategory::Cloud),
            (386, ErrorCategory::Cloud),
            (388, ErrorCategory::Cloud),
            (404, ErrorCategory::Cloud),
            (426, ErrorCategory::Cloud),
            (475, ErrorCategory::Cloud),
        ] {
            let error = io::Error::from_raw_os_error(code);
            let classified = OperationError::from_io("test", PathBuf::from("file"), &error);
            assert_eq!(classified.category, category);
            assert_eq!(classified.code, Some(code));
            assert!(!classified.hint.is_empty());
        }
    }
}
