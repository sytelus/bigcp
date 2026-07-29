//! Independent, sandbox-confined correctness tools for bigcp.
//!
//! This crate does not depend on bigcp-core, so its oracle cannot accidentally
//! share the copy implementation it is intended to check.

#![deny(missing_docs, unsafe_code)]

pub mod extents;
pub mod generator;
pub mod oracle;
pub mod sandbox;

pub use extents::{ExtentReport, measure_extents};
pub use generator::{GenerationReport, HEAVY_TESTS_ENV, Scenario, generate, heavy_tests_enabled};
pub use oracle::{CheckReport, check_trees};
pub use sandbox::{SandboxRoot, allowed_test_drives};
