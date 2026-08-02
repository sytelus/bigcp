//! Run-owned per-phase wall-time accumulators for `--analyze` runs.
//!
//! Recording is a pair of atomic adds (~nanoseconds) and always on; the
//! totals are *reported* only when the user asked for analysis. A tracker is
//! shared only by one run's coordinator and workers, so sequential or
//! concurrent library calls cannot contaminate one another's measurements.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Instrumented phases, in reporting order: worker-side engine phases first,
/// then the coordinator's serial stages (which bound the wall clock — the
/// coordinator is single-threaded, so its totals compare directly to run
/// duration, while worker totals divide by the worker count).
pub const PHASE_NAMES: [&str; 9] = [
    "open_src",
    "list_streams",
    "read",
    "create_dst",
    "write",
    "set_meta",
    "coord_enum_join",
    "coord_entry",
    "coord_finish",
];

// Call sites must use these named indices, never literals: `record` silently
// ignores an out-of-range index, so a drifted literal would misattribute
// every later phase's measurements without any error. The unit test below
// pins each constant to its `PHASE_NAMES` entry.

/// Engine phase: opening the source file.
pub const PHASE_OPEN_SRC: usize = 0;
/// Engine phase: enumerating the source stream set.
pub const PHASE_LIST_STREAMS: usize = 1;
/// Engine phase: source data reads.
pub const PHASE_READ: usize = 2;
/// Engine phase: creating the destination object.
pub const PHASE_CREATE_DST: usize = 3;
/// Engine phase: destination data writes.
pub const PHASE_WRITE: usize = 4;
/// Engine phase: destination metadata stamping.
pub const PHASE_SET_META: usize = 5;
/// Coordinator phase: directory enumeration and join.
pub const PHASE_COORD_ENUM_JOIN: usize = 6;
/// Coordinator phase: per-entry classification and dispatch.
pub const PHASE_COORD_ENTRY: usize = 7;
/// Coordinator phase: worker-result accounting.
pub const PHASE_COORD_FINISH: usize = 8;

/// Thread-safe phase measurements belonging to exactly one copy run.
#[derive(Debug)]
pub struct PhaseTracker {
    nanos: [AtomicU64; PHASE_NAMES.len()],
    calls: [AtomicU64; PHASE_NAMES.len()],
}

impl PhaseTracker {
    /// Creates empty measurements for a new run.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nanos: [const { AtomicU64::new(0) }; PHASE_NAMES.len()],
            calls: [const { AtomicU64::new(0) }; PHASE_NAMES.len()],
        }
    }

    /// Adds one timed call to this run's phase accumulator.
    pub fn record(&self, index: usize, elapsed: Duration) {
        if let (Some(nanos), Some(calls)) = (self.nanos.get(index), self.calls.get(index)) {
            nanos.fetch_add(
                u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Renders this run's accumulated table as one human-readable line set.
    #[must_use]
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (index, name) in PHASE_NAMES.iter().enumerate() {
            let nanos = self.nanos[index].load(Ordering::Relaxed);
            let calls = self.calls[index].load(Ordering::Relaxed);
            if calls == 0 {
                continue;
            }
            let mean_us = nanos as f64 / 1_000.0 / calls as f64;
            let total_s = nanos as f64 / 1e9;
            let _ = write!(
                out,
                "{name}: total={total_s:.2}s calls={calls} mean={mean_us:.0}us; "
            );
        }
        out
    }
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::PhaseTracker;

    #[test]
    fn measurements_are_isolated_per_run() {
        let first = PhaseTracker::new();
        let second = PhaseTracker::new();
        first.record(super::PHASE_OPEN_SRC, Duration::from_micros(10));

        assert!(first.summary().contains("open_src:"));
        assert!(second.summary().is_empty());
    }

    #[test]
    fn phase_index_constants_match_their_reported_names() {
        use super::{
            PHASE_COORD_ENTRY, PHASE_COORD_ENUM_JOIN, PHASE_COORD_FINISH, PHASE_CREATE_DST,
            PHASE_LIST_STREAMS, PHASE_NAMES, PHASE_OPEN_SRC, PHASE_READ, PHASE_SET_META,
            PHASE_WRITE,
        };
        assert_eq!(PHASE_NAMES[PHASE_OPEN_SRC], "open_src");
        assert_eq!(PHASE_NAMES[PHASE_LIST_STREAMS], "list_streams");
        assert_eq!(PHASE_NAMES[PHASE_READ], "read");
        assert_eq!(PHASE_NAMES[PHASE_CREATE_DST], "create_dst");
        assert_eq!(PHASE_NAMES[PHASE_WRITE], "write");
        assert_eq!(PHASE_NAMES[PHASE_SET_META], "set_meta");
        assert_eq!(PHASE_NAMES[PHASE_COORD_ENUM_JOIN], "coord_enum_join");
        assert_eq!(PHASE_NAMES[PHASE_COORD_ENTRY], "coord_entry");
        assert_eq!(PHASE_NAMES[PHASE_COORD_FINISH], "coord_finish");
    }
}
