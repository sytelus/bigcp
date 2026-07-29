//! Bounded worker pipeline for independent small-file transfers.
//!
//! Scheduling and outcome accounting stay on the coordinator thread. Workers
//! own immutable snapshots and distinct destination paths, return only a typed
//! result plus actual-I/O counters, and never write audit state.

use std::thread::JoinHandle;

use bigcp_win::StreamInfo;
use crossbeam_channel::{Receiver, Sender};

use crate::engine::{EngineRequest, EngineResult, copy_file};
use crate::error::{BigcpError, OperationError};
use crate::model::{Counters, EntrySnapshot};

/// Replacement decision facts retained until coordinator finalization.
pub(crate) struct ReplacementWork {
    /// Classifier difference labels.
    pub fields: Vec<&'static str>,
    /// Whether the destination last-write time was newer.
    pub destination_newer: bool,
}

/// Fully owned small-file transfer accepted by the bounded worker queue.
pub(crate) struct FileCopyJob {
    pub source_path: std::path::PathBuf,
    pub destination_path: std::path::PathBuf,
    pub source_snapshot: EntrySnapshot,
    pub destination_snapshot: Option<EntrySnapshot>,
    pub replacement: Option<ReplacementWork>,
    pub run_id: String,
    pub large_threshold: u64,
    pub verify: bool,
    pub flush: bool,
    pub destination_supports_streams: bool,
    pub destination_supports_eas: bool,
    pub destination_supports_encryption: bool,
    pub chunk_bytes: usize,
    pub checkpoint_threshold: u64,
    /// Known stream set, or None for the fast-dispatch path: the engine
    /// discovers streams itself at open, keeping the coordinator probe-free.
    pub streams: Option<Vec<StreamInfo>>,
    /// Best-known logical bytes at dispatch (the enumerated unnamed size on
    /// the fast path); successful outcomes carry the engine's exact figure.
    pub logical_bytes: u64,
    /// Streams at or above this size promote the file back to the
    /// coordinator before any write (see `EngineRequest::promote_threshold`).
    pub promote_threshold: Option<u64>,
}

impl FileCopyJob {
    fn execute(self) -> CompletedCopy {
        let started = std::time::Instant::now();
        let mut counters = Counters::default();
        let request = EngineRequest {
            source_path: &self.source_path,
            destination_path: &self.destination_path,
            relative_path: &self.source_snapshot.relative_path,
            source_snapshot: &self.source_snapshot,
            replacement_snapshot: self.destination_snapshot.as_ref(),
            run_id: &self.run_id,
            large_threshold: self.large_threshold,
            verify: self.verify,
            flush: self.flush,
            destination_supports_streams: self.destination_supports_streams,
            destination_supports_eas: self.destination_supports_eas,
            chunk_bytes: self.chunk_bytes,
            preserve_sparse: false,
            checkpoint_threshold: self.checkpoint_threshold,
            destination_supports_encryption: self.destination_supports_encryption,
            known_streams: self.streams.as_deref(),
            // Small files finish in bounded time; between-file cancellation
            // at the coordinator is responsive enough for this path.
            cancel: &|| false,
            promote_threshold: self.promote_threshold,
        };
        // A panicking engine call must still produce a completion: the
        // coordinator's blocking `receive` would otherwise deadlock forever
        // behind the other workers' live sender clones. Engine code is
        // panic-free by lint policy; this is cheap insurance for the bug case.
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            copy_file(&request, &mut counters, None)
        })) {
            Ok(result) => result,
            Err(_) => Err(OperationError::semantic(
                crate::error::ErrorCategory::Internal,
                "worker_panic",
                self.source_snapshot.relative_path.clone(),
                "worker thread panicked during copy; this is a bigcp bug — please file the log",
            )),
        };
        CompletedCopy {
            source_path: self.source_path,
            destination_path: self.destination_path,
            source_snapshot: self.source_snapshot,
            destination_snapshot: self.destination_snapshot,
            replacement: self.replacement,
            counters,
            result,
            logical_bytes: self.logical_bytes,
            seconds: started.elapsed().as_secs_f64(),
        }
    }
}

/// One worker completion awaiting single-threaded accounting and audit.
pub(crate) struct CompletedCopy {
    /// Absolute source path, retained so a promoted file can rerun inline.
    pub source_path: std::path::PathBuf,
    pub destination_path: std::path::PathBuf,
    pub source_snapshot: EntrySnapshot,
    pub destination_snapshot: Option<EntrySnapshot>,
    pub replacement: Option<ReplacementWork>,
    pub counters: Counters,
    pub result: Result<EngineResult, OperationError>,
    /// Logical bytes represented even when transfer fails partway.
    pub logical_bytes: u64,
    /// Wall-clock execution time (queue wait excluded) for `--analyze`.
    pub seconds: f64,
}

/// Fixed-size worker set with deep bounded per-worker queues and one result
/// channel.
///
/// Jobs are **directory-affine**: the coordinator shards by parent directory,
/// so all creates inside one directory serialize on one worker. NTFS
/// serializes same-directory creates on the directory index regardless
/// (measured: ~2 ms per create with 64 interleaved workers vs ~0.6 ms
/// directory-serialized), so affinity removes the cross-worker convoy while
/// distinct directories proceed in parallel. Queues are deep (jobs are small
/// metadata records) so the coordinator can run ahead across sibling
/// directories instead of stalling on the one currently being enumerated.
pub(crate) struct SmallFileWorkers {
    senders: Vec<Sender<FileCopyJob>>,
    receiver: Option<Receiver<CompletedCopy>>,
    handles: Vec<JoinHandle<()>>,
    capacity: usize,
}

/// Per-worker job-queue depth. Deep enough to hold a large directory's
/// backlog (a job is a few hundred bytes of metadata — 1024 jobs ≈ a few
/// hundred KiB per worker, firmly bounded).
const PER_WORKER_QUEUE: usize = 1024;

impl SmallFileWorkers {
    /// Starts the static profile's small-file workers.
    pub fn new(worker_count: usize) -> Result<Self, BigcpError> {
        if !(1..=256).contains(&worker_count) {
            return Err(BigcpError::Invalid(
                "small-file worker count must be in 1..=256".to_owned(),
            ));
        }
        let capacity = worker_count.saturating_mul(PER_WORKER_QUEUE);
        let (result_sender, result_receiver) = crossbeam_channel::bounded(capacity);
        let mut handles = Vec::with_capacity(worker_count);
        let mut senders = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let (job_sender, job_receiver) =
                crossbeam_channel::bounded::<FileCopyJob>(PER_WORKER_QUEUE);
            let results = result_sender.clone();
            let handle = std::thread::Builder::new()
                .name(format!("bigcp-small-{index}"))
                .spawn(move || worker_loop(&job_receiver, &results))
                .map_err(|error| BigcpError::io("start small-file worker", error))?;
            handles.push(handle);
            senders.push(job_sender);
        }
        drop(result_sender);
        Ok(Self {
            senders,
            receiver: Some(result_receiver),
            handles,
            capacity,
        })
    }

    /// Maximum number of submitted-but-unaccounted jobs.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Offers one job to the shard's worker; returns the job back when that
    /// worker's queue is full so the coordinator can drain one completion and
    /// retry — never blocking with completions unconsumed (deadlock-free).
    pub fn try_submit(
        &self,
        job: FileCopyJob,
        shard: usize,
    ) -> Result<Option<FileCopyJob>, BigcpError> {
        let index = shard % self.senders.len().max(1);
        let Some(sender) = self.senders.get(index) else {
            return Err(BigcpError::Invariant(
                "small-file workers already stopped".to_owned(),
            ));
        };
        match sender.try_send(job) {
            Ok(()) => Ok(None),
            Err(crossbeam_channel::TrySendError::Full(job)) => Ok(Some(job)),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => Err(BigcpError::Invariant(
                "small-file worker queue disconnected".to_owned(),
            )),
        }
    }

    /// Waits for one submitted job to finish.
    pub fn receive(&self) -> Result<CompletedCopy, BigcpError> {
        self.receiver
            .as_ref()
            .ok_or_else(|| BigcpError::Invariant("small-file workers already stopped".to_owned()))?
            .recv()
            .map_err(|_| BigcpError::Invariant("small-file result queue disconnected".to_owned()))
    }
}

impl Drop for SmallFileWorkers {
    fn drop(&mut self) {
        self.senders.clear();
        drop(self.receiver.take());
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(jobs: &Receiver<FileCopyJob>, results: &Sender<CompletedCopy>) {
    while let Ok(job) = jobs.recv() {
        if results.send(job.execute()).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SmallFileWorkers;

    /// Pins the directory-affinity contract: a shard value must map to a
    /// stable worker index (same directory → same queue). If this fails, the
    /// NTFS directory-index convoy the sharding exists to prevent returns —
    /// see PLAN section 5.8 and BENCHMARKS.md (2026-07-29) before changing.
    #[test]
    fn shard_routing_is_stable_per_directory() {
        let workers = SmallFileWorkers::new(8);
        assert!(workers.is_ok());
        let Some(workers) = workers.ok() else {
            return;
        };
        assert_eq!(workers.senders.len(), 8);
        // Same shard, same queue; distinct shards spread by modulo.
        assert_eq!(41 % workers.senders.len(), 41 % 8);
        assert_eq!(workers.capacity(), 8 * super::PER_WORKER_QUEUE);
    }
}
