//! Copy semantics and orchestration for bigcp.
//!
//! The crate has no unsafe code and no direct Win32 dependency. All filesystem
//! mutation is routed through bigcp-win's capability-oriented safe wrappers.

#![deny(missing_docs, unsafe_code)]

pub mod audit;
pub mod classify;
pub mod copy;
pub mod devprofile;
mod engine;
pub mod error;
pub mod journal;
pub mod model;
pub mod options;
pub mod report;
pub mod stats;
pub mod verify;
mod worker;

pub use copy::{RunObserver, run_copy};
pub use error::{BigcpError, ErrorCategory, OperationError};
pub use model::{Counters, RunSnapshot, RunState};
pub use options::{CopyOptions, DeviceClass, TuneOptions, VerifyOptions};
pub use report::{RunReport, load_report};
pub use verify::run_standalone_verify;

/// The report schema version emitted by this build.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// The JSONL log schema version emitted by this build.
pub const LOG_SCHEMA_VERSION: u32 = 1;
