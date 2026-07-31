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
        first.record(0, Duration::from_micros(10));

        assert!(first.summary().contains("open_src:"));
        assert!(second.summary().is_empty());
    }
}
