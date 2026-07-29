//! Low-overhead throughput samples and honest observed-peak analysis.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// One downsampled report point.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TimelinePoint {
    /// Seconds since run start.
    pub seconds: f64,
    /// Source read throughput.
    pub read_mbps: f64,
    /// Destination write throughput.
    pub write_mbps: f64,
    /// Completed files per second.
    pub files_per_second: f64,
    /// Confidence-rated application-side hypothesis.
    pub hypothesis: String,
}

/// Application-side rate accumulator.
pub struct StatsTracker {
    started: Instant,
    window_started: Instant,
    window_read: u64,
    window_written: u64,
    window_files: u64,
    timeline: Vec<TimelinePoint>,
    best_write_bytes_per_second: f64,
}

impl StatsTracker {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            window_started: now,
            window_read: 0,
            window_written: 0,
            window_files: 0,
            timeline: Vec::new(),
            best_write_bytes_per_second: 0.0,
        }
    }

    /// Adds real copy traffic to the current window.
    pub fn record(&mut self, read: u64, written: u64, files: u64) {
        self.window_read = self.window_read.saturating_add(read);
        self.window_written = self.window_written.saturating_add(written);
        self.window_files = self.window_files.saturating_add(files);
    }

    /// Rolls a sample after at least the requested interval.
    pub fn maybe_roll(&mut self, interval: Duration) -> Option<TimelinePoint> {
        let elapsed = self.window_started.elapsed();
        if elapsed < interval {
            return None;
        }
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        let read_rate = self.window_read as f64 / seconds;
        let write_rate = self.window_written as f64 / seconds;
        // "Best observed sustained throughput" (F10) requires a *sustained*
        // window: a sub-5-second tail whose writes landed in cache would
        // otherwise inflate the peak and corrupt the efficiency figure.
        if elapsed >= Duration::from_secs(5) {
            self.best_write_bytes_per_second = self.best_write_bytes_per_second.max(write_rate);
        }
        let hypothesis = if self.window_read == 0 && self.window_written == 0 {
            "discovery-bound"
        } else if write_rate + f64::EPSILON < read_rate * 0.75 {
            "destination-bound"
        } else if read_rate + f64::EPSILON < write_rate * 0.75 {
            "source-bound"
        } else {
            "balanced"
        };
        let point = TimelinePoint {
            seconds: self.started.elapsed().as_secs_f64(),
            read_mbps: read_rate / 1_000_000.0,
            write_mbps: write_rate / 1_000_000.0,
            files_per_second: self.window_files as f64 / seconds,
            hypothesis: hypothesis.to_owned(),
        };
        if self.timeline.len() < 3_600 {
            self.timeline.push(point.clone());
        }
        self.window_started = Instant::now();
        self.window_read = 0;
        self.window_written = 0;
        self.window_files = 0;
        Some(point)
    }

    /// Returns elapsed wall-clock time.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Returns report-ready samples.
    #[must_use]
    pub fn timeline(&self) -> &[TimelinePoint] {
        &self.timeline
    }

    /// Best observed write rate from real traffic.
    #[must_use]
    pub const fn best_write_bytes_per_second(&self) -> f64 {
        self.best_write_bytes_per_second
    }

    /// Returns non-mutating rates for live UI snapshots.
    #[must_use]
    pub fn current_rates(&self) -> (f64, f64) {
        let seconds = self
            .window_started
            .elapsed()
            .as_secs_f64()
            .max(f64::EPSILON);
        (
            self.window_read as f64 / seconds,
            self.window_written as f64 / seconds,
        )
    }
}

impl Default for StatsTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// One `--analyze` size-class aggregate.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SizeClassSummary {
    /// Human-stable bucket label.
    pub label: String,
    /// Completed copies in this bucket.
    pub files: u64,
    /// Logical bytes copied in this bucket.
    pub bytes: u64,
    /// Total wall-clock execution seconds spent in this bucket.
    pub seconds: f64,
}

/// One `--analyze` slowest-copy sample.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SlowFileSample {
    /// Relative display path.
    pub relative_path: String,
    /// Logical bytes copied.
    pub bytes: u64,
    /// Wall-clock execution seconds (queue wait excluded).
    pub seconds: f64,
    /// Effective throughput for this file.
    pub mbps: f64,
}

/// Bounded live-run insight summary produced by `--analyze` (VISION).
///
/// Deliberately minimal-but-high-signal: five fixed size-class buckets plus a
/// top-N slowest table answer "where did the time go" without per-file log
/// growth. Collection is one comparison and an occasional 20-entry insertion
/// per completed file; the output is one JSONL event and one report section.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AnalysisSummary {
    /// Aggregates by logical size class.
    pub size_classes: Vec<SizeClassSummary>,
    /// Slowest completed copies, worst first, capped at 20.
    pub slowest_files: Vec<SlowFileSample>,
}

const SLOWEST_LIMIT: usize = 20;
const SIZE_CLASS_BOUNDS: [(u64, &str); 5] = [
    (64 * 1024, "<=64KiB"),
    (1024 * 1024, "<=1MiB"),
    (16 * 1024 * 1024, "<=16MiB"),
    (256 * 1024 * 1024, "<=256MiB"),
    (u64::MAX, ">256MiB"),
];

/// Accumulates `--analyze` insight with fixed memory.
#[derive(Debug, Default)]
pub struct AnalysisCollector {
    classes: [(u64, u64, f64); 5],
    slowest: Vec<SlowFileSample>,
}

impl AnalysisCollector {
    /// Records one successfully copied file.
    pub fn record(&mut self, relative_path: &str, bytes: u64, seconds: f64) {
        let index = SIZE_CLASS_BOUNDS
            .iter()
            .position(|(bound, _)| bytes <= *bound)
            .unwrap_or(SIZE_CLASS_BOUNDS.len() - 1);
        let class = &mut self.classes[index];
        class.0 = class.0.saturating_add(1);
        class.1 = class.1.saturating_add(bytes);
        class.2 += seconds;
        let qualifies = self.slowest.len() < SLOWEST_LIMIT
            || self
                .slowest
                .last()
                .is_some_and(|slowest| seconds > slowest.seconds);
        if qualifies {
            let effective_seconds = seconds.max(f64::EPSILON);
            self.slowest.push(SlowFileSample {
                relative_path: relative_path.to_owned(),
                bytes,
                seconds,
                mbps: bytes as f64 / effective_seconds / 1_000_000.0,
            });
            self.slowest.sort_by(|left, right| {
                right
                    .seconds
                    .partial_cmp(&left.seconds)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            self.slowest.truncate(SLOWEST_LIMIT);
        }
    }

    /// Produces the report/log summary.
    #[must_use]
    pub fn finish(self) -> AnalysisSummary {
        AnalysisSummary {
            size_classes: SIZE_CLASS_BOUNDS
                .iter()
                .zip(self.classes)
                .map(|((_, label), (files, bytes, seconds))| SizeClassSummary {
                    label: (*label).to_owned(),
                    files,
                    bytes,
                    seconds,
                })
                .collect(),
            slowest_files: self.slowest,
        }
    }
}
