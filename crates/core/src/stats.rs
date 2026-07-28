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
        self.best_write_bytes_per_second = self.best_write_bytes_per_second.max(write_rate);
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
