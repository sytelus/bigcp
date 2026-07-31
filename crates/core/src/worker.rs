//! Bounded worker pipeline for independent small-file transfers.
//!
//! Scheduling and outcome accounting stay on the coordinator thread. Workers
//! own immutable snapshots and distinct destination paths, return only a typed
//! result plus actual-I/O counters, and never write audit state.

use std::sync::Arc;
use std::thread::JoinHandle;

use bigcp_win::StreamInfo;
use crossbeam_channel::{Receiver, Sender};

use crate::engine::{
    EngineRequest, EngineResult, PreparedPlainSmall, SmallPreparation, WrittenPlainSmall,
    copy_file, finish_plain_small, prepare_plain_small, write_plain_small,
};
use crate::error::{BigcpError, OperationError};
use crate::model::{Counters, EntrySnapshot};
use crate::phase::PhaseTracker;
use crate::transport::TransportProfile;

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
    pub source_supports_streams: bool,
    pub source_supports_eas: bool,
    pub destination_supports_streams: bool,
    pub destination_supports_eas: bool,
    pub destination_supports_encryption: bool,
    pub destination_supports_preallocation: bool,
    pub destination_supports_persistent_acls: bool,
    pub destination_supports_posix_unlink_rename: bool,
    pub destination_metadata: bigcp_win::BasicMetadata,
    pub destination_requires_post_write_stamp: bool,
    pub chunk_bytes: usize,
    pub transport: TransportProfile,
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
    /// Measurements owned by this run and shared with every worker.
    pub phases: Arc<PhaseTracker>,
}

impl FileCopyJob {
    fn request(&self) -> EngineRequest<'_> {
        EngineRequest {
            source_path: &self.source_path,
            destination_path: &self.destination_path,
            relative_path: &self.source_snapshot.relative_path,
            source_snapshot: &self.source_snapshot,
            replacement_snapshot: self.destination_snapshot.as_ref(),
            run_id: &self.run_id,
            large_threshold: self.large_threshold,
            verify: self.verify,
            flush: self.flush,
            source_supports_streams: self.source_supports_streams,
            source_supports_eas: self.source_supports_eas,
            destination_supports_streams: self.destination_supports_streams,
            destination_supports_eas: self.destination_supports_eas,
            chunk_bytes: self.chunk_bytes,
            transport: self.transport,
            preserve_sparse: false,
            checkpoint_threshold: self.checkpoint_threshold,
            destination_supports_encryption: self.destination_supports_encryption,
            destination_supports_preallocation: self.destination_supports_preallocation,
            destination_supports_persistent_acls: self.destination_supports_persistent_acls,
            destination_supports_posix_unlink_rename: self.destination_supports_posix_unlink_rename,
            destination_metadata: self.destination_metadata,
            destination_requires_post_write_stamp: self.destination_requires_post_write_stamp,
            known_streams: self.streams.as_deref(),
            cancel: &never_cancel,
            promote_threshold: self.promote_threshold,
            phases: &self.phases,
        }
    }

    fn execute(self) -> CompletedCopy {
        let started = std::time::Instant::now();
        let mut counters = Counters::default();
        let request = self.request();
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
        complete_job(self, counters, result, started.elapsed().as_secs_f64())
    }
}

fn complete_job(
    job: FileCopyJob,
    counters: Counters,
    result: Result<EngineResult, OperationError>,
    seconds: f64,
) -> CompletedCopy {
    CompletedCopy {
        source_path: job.source_path,
        destination_path: job.destination_path,
        source_snapshot: job.source_snapshot,
        destination_snapshot: job.destination_snapshot,
        replacement: job.replacement,
        counters,
        result,
        logical_bytes: job.logical_bytes,
        seconds,
    }
}

fn never_cancel() -> bool {
    false
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
    pub fn new(
        worker_count: usize,
        same_spindle_burst_bytes: Option<usize>,
    ) -> Result<Self, BigcpError> {
        if !(1..=256).contains(&worker_count) {
            return Err(BigcpError::Invalid(
                "small-file worker count must be in 1..=256".to_owned(),
            ));
        }
        if same_spindle_burst_bytes.is_some() && worker_count != 1 {
            return Err(BigcpError::Invariant(
                "same-spindle small-file scheduling requires exactly one phased worker".to_owned(),
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
                .spawn(move || {
                    if let Some(burst_bytes) = same_spindle_burst_bytes {
                        same_spindle_worker_loop(&job_receiver, &results, burst_bytes);
                    } else {
                        worker_loop(&job_receiver, &results);
                    }
                })
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

struct PreparedBatchEntry {
    job: FileCopyJob,
    prepared: PreparedPlainSmall,
    counters: Counters,
    seconds: f64,
}

struct WrittenBatchEntry {
    job: FileCopyJob,
    written: WrittenPlainSmall,
    counters: Counters,
    seconds: f64,
}

/// Same-spindle small-file scheduling has three coarse phases: fill the
/// bounded batch from source handles, write every prepared destination, then
/// revalidate the still-open source handles. This removes one source↔target
/// head seek per file while preserving the ordinary engine's stability and
/// completion checks.
fn same_spindle_worker_loop(
    jobs: &Receiver<FileCopyJob>,
    results: &Sender<CompletedCopy>,
    burst_bytes: usize,
) {
    let mut carried = None;
    while let Some(first) = carried.take().or_else(|| jobs.recv().ok()) {
        let mut estimated = usize::try_from(first.logical_bytes).unwrap_or(usize::MAX);
        let mut batch = vec![first];
        while batch.len() < PER_WORKER_QUEUE && estimated < burst_bytes {
            let next = match jobs.recv_timeout(std::time::Duration::from_millis(2)) {
                Ok(job) => job,
                Err(
                    crossbeam_channel::RecvTimeoutError::Timeout
                    | crossbeam_channel::RecvTimeoutError::Disconnected,
                ) => break,
            };
            let next_bytes = usize::try_from(next.logical_bytes).unwrap_or(usize::MAX);
            if estimated.saturating_add(next_bytes) > burst_bytes {
                carried = Some(next);
                break;
            }
            estimated = estimated.saturating_add(next_bytes);
            batch.push(next);
        }
        if !run_same_spindle_batch(batch, results) {
            break;
        }
    }
}

fn run_same_spindle_batch(batch: Vec<FileCopyJob>, results: &Sender<CompletedCopy>) -> bool {
    let mut prepared = Vec::with_capacity(batch.len());
    let mut regular = Vec::new();
    for job in batch {
        let started = std::time::Instant::now();
        let mut counters = Counters::default();
        let result = {
            let request = job.request();
            prepare_plain_small(&request, &mut counters)
        };
        let seconds = started.elapsed().as_secs_f64();
        match result {
            Ok(SmallPreparation::Ready(value)) => prepared.push(PreparedBatchEntry {
                job,
                prepared: value,
                counters,
                seconds,
            }),
            Ok(SmallPreparation::RequiresRegular) => regular.push(job),
            Err(error) => {
                if results
                    .send(complete_job(job, counters, Err(error), seconds))
                    .is_err()
                {
                    return false;
                }
            }
        }
    }

    let mut written = Vec::with_capacity(prepared.len());
    for mut entry in prepared {
        let started = std::time::Instant::now();
        let result = {
            let request = entry.job.request();
            write_plain_small(&request, &mut entry.counters, entry.prepared, true)
        };
        entry.seconds += started.elapsed().as_secs_f64();
        match result {
            Ok(value) => written.push(WrittenBatchEntry {
                job: entry.job,
                written: value,
                counters: entry.counters,
                seconds: entry.seconds,
            }),
            Err(error) => {
                if results
                    .send(complete_job(
                        entry.job,
                        entry.counters,
                        Err(error),
                        entry.seconds,
                    ))
                    .is_err()
                {
                    return false;
                }
            }
        }
    }

    for entry in written {
        let started = std::time::Instant::now();
        let result = {
            let request = entry.job.request();
            finish_plain_small(&request, entry.written)
        };
        let seconds = entry.seconds + started.elapsed().as_secs_f64();
        if results
            .send(complete_job(entry.job, entry.counters, result, seconds))
            .is_err()
        {
            return false;
        }
    }

    // Representable ADS/EA files keep the existing transactional engine.
    // They are rare and are deliberately processed after the ordinary batch
    // so they cannot break its source-only/destination-only phases.
    for job in regular {
        if results.send(job.execute()).is_err() {
            return false;
        }
    }
    true
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
        let workers = SmallFileWorkers::new(8, None);
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
