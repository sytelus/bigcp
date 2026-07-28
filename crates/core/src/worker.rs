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
    pub streams: Vec<StreamInfo>,
}

impl FileCopyJob {
    fn execute(self) -> CompletedCopy {
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
            known_streams: Some(&self.streams),
        };
        let result = copy_file(&request, &mut counters, None);
        CompletedCopy {
            destination_path: self.destination_path,
            source_snapshot: self.source_snapshot,
            destination_snapshot: self.destination_snapshot,
            replacement: self.replacement,
            counters,
            result,
        }
    }
}

/// One worker completion awaiting single-threaded accounting and audit.
pub(crate) struct CompletedCopy {
    pub destination_path: std::path::PathBuf,
    pub source_snapshot: EntrySnapshot,
    pub destination_snapshot: Option<EntrySnapshot>,
    pub replacement: Option<ReplacementWork>,
    pub counters: Counters,
    pub result: Result<EngineResult, OperationError>,
}

/// Fixed-size worker set with bounded work and result channels.
pub(crate) struct SmallFileWorkers {
    sender: Option<Sender<FileCopyJob>>,
    receiver: Option<Receiver<CompletedCopy>>,
    handles: Vec<JoinHandle<()>>,
    capacity: usize,
}

impl SmallFileWorkers {
    /// Starts the static profile's small-file workers.
    pub fn new(worker_count: usize) -> Result<Self, BigcpError> {
        let worker_count = worker_count.clamp(1, 256);
        let capacity = worker_count.saturating_mul(4).max(4);
        let (job_sender, job_receiver) = crossbeam_channel::bounded::<FileCopyJob>(capacity);
        let (result_sender, result_receiver) = crossbeam_channel::bounded(capacity);
        let mut handles = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let jobs = job_receiver.clone();
            let results = result_sender.clone();
            let handle = std::thread::Builder::new()
                .name(format!("bigcp-small-{index}"))
                .spawn(move || worker_loop(&jobs, &results))
                .map_err(|error| BigcpError::io("start small-file worker", error))?;
            handles.push(handle);
        }
        drop(result_sender);
        Ok(Self {
            sender: Some(job_sender),
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

    /// Submits one job; the coordinator enforces the outstanding cap first.
    pub fn submit(&self, job: FileCopyJob) -> Result<(), BigcpError> {
        self.sender
            .as_ref()
            .ok_or_else(|| BigcpError::Invariant("small-file workers already stopped".to_owned()))?
            .send(job)
            .map_err(|_| BigcpError::Invariant("small-file worker queue disconnected".to_owned()))
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
        drop(self.sender.take());
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
