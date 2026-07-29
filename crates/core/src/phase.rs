//! Process-wide per-phase wall-time accumulators for `--analyze` runs.
//!
//! Recording is a pair of atomic adds (~nanoseconds) and always on; the
//! totals are *reported* only when the user asked for analysis. One copy run
//! per process (the run lock enforces it), so process-wide statics are the
//! simplest correct scope.

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

static NANOS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
static CALLS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// Adds one timed call to a phase accumulator.
pub fn record(index: usize, elapsed: Duration) {
    if let (Some(nanos), Some(calls)) = (NANOS.get(index), CALLS.get(index)) {
        nanos.fetch_add(
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        calls.fetch_add(1, Ordering::Relaxed);
    }
}

/// Renders the accumulated table as one human-readable line set.
#[must_use]
pub fn summary() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (index, name) in PHASE_NAMES.iter().enumerate() {
        let nanos = NANOS[index].load(Ordering::Relaxed);
        let calls = CALLS[index].load(Ordering::Relaxed);
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
