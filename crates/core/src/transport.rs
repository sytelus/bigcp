//! Static copy-transport policy selected from preflight topology.
//!
//! The normal local path retains one request-sized buffer. Redirector paths
//! use a bounded two-buffer pipeline so a source read and destination write
//! can be in flight at the same time. A rotational source and destination that
//! share a physical disk instead use a much larger bounded staging buffer so
//! reads and writes occur in coarse phases rather than forcing a disk-head seek
//! after every request. This module owns only transport mechanics; file
//! semantics, hashing, checkpoints, and publication remain in `engine`.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Effective data-transport strategy recorded in logs and reports.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Existing request-at-a-time buffered streaming for independent devices
    /// and solid-state same-device copies.
    #[default]
    Standard,
    /// Bounded read/write overlap for generic UNC and mapped-network paths.
    Redirector,
    /// WSL's local Plan 9 redirector, with independently evolvable scheduling.
    Wsl,
    /// Phased reads and writes for intersecting rotational media.
    SameSpindle,
}

/// Immutable transport settings selected before copy I/O starts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TransportProfile {
    /// Selected strategy.
    pub kind: TransportKind,
    /// Standard/redirector/WSL request bytes, or maximum same-spindle staging.
    pub burst_bytes: usize,
}

impl TransportProfile {
    /// Creates the unchanged request-at-a-time transport.
    #[must_use]
    pub const fn standard(chunk_bytes: usize) -> Self {
        Self {
            kind: TransportKind::Standard,
            burst_bytes: chunk_bytes,
        }
    }

    /// Creates the rotational same-device phased transport.
    #[must_use]
    pub const fn same_spindle(burst_bytes: usize) -> Self {
        Self {
            kind: TransportKind::SameSpindle,
            burst_bytes,
        }
    }

    /// Creates the bounded redirector pipeline.
    #[must_use]
    pub const fn redirector(chunk_bytes: usize) -> Self {
        Self {
            kind: TransportKind::Redirector,
            burst_bytes: chunk_bytes,
        }
    }

    /// Creates the bounded WSL Plan 9 pipeline.
    #[must_use]
    pub const fn wsl(chunk_bytes: usize) -> Self {
        Self {
            kind: TransportKind::Wsl,
            burst_bytes: chunk_bytes,
        }
    }

    /// Whether reads and writes must be serialized into coarse phases.
    #[must_use]
    pub const fn is_same_spindle(self) -> bool {
        matches!(self.kind, TransportKind::SameSpindle)
    }

    /// Whether streamed files should overlap source reads and destination
    /// writes through the bounded redirector pipeline.
    #[must_use]
    pub const fn is_redirector(self) -> bool {
        matches!(self.kind, TransportKind::Redirector | TransportKind::Wsl)
    }

    /// Whether the transfer crosses WSL's Plan 9 provider.
    #[must_use]
    pub const fn is_wsl(self) -> bool {
        matches!(self.kind, TransportKind::Wsl)
    }
}

/// Fixed number of request buffers owned by one redirector transfer.
///
/// Two buffers are sufficient to overlap one synchronous read with one
/// synchronous write. A deeper userspace queue would increase memory without
/// increasing either handle's I/O depth.
pub const PIPELINE_BUFFERS: usize = 2;

const CHANNEL_POLL: Duration = Duration::from_millis(50);

/// Thread-safe cancellation source shared by coordinator and worker
/// transports without coupling this module to a particular front end.
pub(crate) trait CancelProbe: Sync {
    fn is_canceled(&self) -> bool;
}

impl<F> CancelProbe for F
where
    F: Fn() -> bool + Sync,
{
    fn is_canceled(&self) -> bool {
        self()
    }
}

impl CancelProbe for AtomicBool {
    fn is_canceled(&self) -> bool {
        self.load(Ordering::Acquire)
    }
}

/// Actual I/O completed by a redirector pipeline, including read-ahead that
/// was already issued when a later write or cancellation stopped the copy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PipelinedTransfer {
    pub bytes_read: u64,
    pub bytes_written: u64,
}

/// Cause and exact I/O counters for an interrupted redirector pipeline.
pub(crate) struct PipelinedFailure {
    pub kind: PipelinedFailureKind,
    pub transfer: PipelinedTransfer,
}

/// Stage at which a redirector pipeline stopped.
pub(crate) enum PipelinedFailureKind {
    Allocate(std::collections::TryReserveError),
    Canceled,
    Read(io::Error),
    Write(io::Error),
    UnexpectedEof,
    ReaderPanicked,
    Internal(&'static str),
}

enum ReadPacket {
    Data(Vec<u8>, usize),
    Failed(io::Error),
    UnexpectedEof,
    Done,
}

/// Copies exactly `length` bytes while overlapping one source read with one
/// destination write.
///
/// The reader owns two fallibly allocated request buffers and never advances
/// more than that bounded window ahead of the committed destination prefix.
/// `consume` runs in write order on the caller thread, which lets the semantic
/// engine retain its ordinary contiguous hashing and checkpoint boundaries.
///
/// INVARIANT for checkpoint callers: `consume` (hashing) runs *before* the
/// corresponding write is confirmed, so the in-flight digest may be ahead of
/// the durable destination prefix mid-segment. A checkpoint recorded from the
/// live hasher inside a segment would attest bytes that may never have been
/// written — checkpoints must only be appended after this function returns
/// `Ok` for the whole segment ending at the boundary.
pub(crate) fn transfer_pipelined<R, W, F>(
    source: &mut R,
    destination: &mut W,
    length: u64,
    request_bytes: usize,
    canceled: &dyn CancelProbe,
    mut consume: F,
) -> Result<PipelinedTransfer, PipelinedFailure>
where
    R: Read + Send,
    W: Write,
    F: FnMut(&[u8]),
{
    let request_bytes = request_bytes.max(1);
    let mut buffers = Vec::with_capacity(PIPELINE_BUFFERS);
    for _ in 0..PIPELINE_BUFFERS {
        let mut buffer = Vec::new();
        if let Err(error) = buffer.try_reserve_exact(request_bytes) {
            return Err(PipelinedFailure {
                kind: PipelinedFailureKind::Allocate(error),
                transfer: PipelinedTransfer::default(),
            });
        }
        buffer.resize(request_bytes, 0);
        buffers.push(buffer);
    }

    let stopped = AtomicBool::new(false);
    let bytes_read = AtomicU64::new(0);
    let mut bytes_written = 0_u64;
    let (free_sender, free_receiver) = crossbeam_channel::bounded(PIPELINE_BUFFERS);
    let (filled_sender, filled_receiver) = crossbeam_channel::bounded(PIPELINE_BUFFERS);
    for buffer in buffers {
        // The receiver is live and the channel has exactly enough capacity.
        if free_sender.send(buffer).is_err() {
            return Err(PipelinedFailure {
                kind: PipelinedFailureKind::Internal(
                    "redirector free-buffer channel disconnected during setup",
                ),
                transfer: PipelinedTransfer::default(),
            });
        }
    }

    let outcome = std::thread::scope(|scope| {
        let reader_stopped = &stopped;
        let reader_bytes_read = &bytes_read;
        let reader = scope.spawn(move || {
            let mut remaining = length;
            while remaining > 0 && !reader_stopped.load(Ordering::Acquire) {
                if canceled.is_canceled() {
                    reader_stopped.store(true, Ordering::Release);
                    break;
                }
                let mut buffer = loop {
                    match free_receiver.recv_timeout(CHANNEL_POLL) {
                        Ok(buffer) => break buffer,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout)
                            if !reader_stopped.load(Ordering::Acquire) => {}
                        Err(_) => return,
                    }
                };
                let requested =
                    usize::try_from(remaining.min(request_bytes as u64)).unwrap_or(request_bytes);
                let packet = loop {
                    match source.read(&mut buffer[..requested]) {
                        Ok(0) => break ReadPacket::UnexpectedEof,
                        Ok(count) => {
                            reader_bytes_read.fetch_add(count as u64, Ordering::Relaxed);
                            remaining = remaining.saturating_sub(count as u64);
                            break ReadPacket::Data(buffer, count);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                            if canceled.is_canceled() || reader_stopped.load(Ordering::Acquire) {
                                return;
                            }
                        }
                        Err(error) => break ReadPacket::Failed(error),
                    }
                };
                let terminal = !matches!(packet, ReadPacket::Data(_, _));
                let mut pending = packet;
                loop {
                    match filled_sender.send_timeout(pending, CHANNEL_POLL) {
                        Ok(()) => break,
                        Err(crossbeam_channel::SendTimeoutError::Timeout(packet))
                            if !reader_stopped.load(Ordering::Acquire) =>
                        {
                            pending = packet;
                        }
                        Err(_) => return,
                    }
                }
                if terminal {
                    return;
                }
            }
            if !reader_stopped.load(Ordering::Acquire) {
                let _ = filled_sender.send(ReadPacket::Done);
            }
        });

        let mut result = Ok(());
        while bytes_written < length {
            if canceled.is_canceled() {
                result = Err(PipelinedFailureKind::Canceled);
                break;
            }
            let packet = loop {
                match filled_receiver.recv_timeout(CHANNEL_POLL) {
                    Ok(packet) => break Some(packet),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout)
                        if !canceled.is_canceled() => {}
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => break None,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        result = Err(PipelinedFailureKind::ReaderPanicked);
                        break None;
                    }
                }
            };
            let Some(packet) = packet else {
                if result.is_ok() {
                    result = Err(PipelinedFailureKind::Canceled);
                }
                break;
            };
            match packet {
                ReadPacket::Data(buffer, count) => {
                    consume(&buffer[..count]);
                    let mut offset = 0;
                    while offset < count {
                        if canceled.is_canceled() {
                            result = Err(PipelinedFailureKind::Canceled);
                            break;
                        }
                        match destination.write(&buffer[offset..count]) {
                            Ok(0) => {
                                result = Err(PipelinedFailureKind::Write(io::Error::new(
                                    io::ErrorKind::WriteZero,
                                    "destination made no progress",
                                )));
                                break;
                            }
                            Ok(written) => {
                                offset = offset.saturating_add(written);
                                bytes_written = bytes_written.saturating_add(written as u64);
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Err(error) => {
                                result = Err(PipelinedFailureKind::Write(error));
                                break;
                            }
                        }
                    }
                    if result.is_ok() {
                        // The reader may have finished after sending the final
                        // packet, in which case returning its buffer is moot.
                        let _ = free_sender.send(buffer);
                    }
                }
                ReadPacket::Failed(error) => result = Err(PipelinedFailureKind::Read(error)),
                ReadPacket::UnexpectedEof => result = Err(PipelinedFailureKind::UnexpectedEof),
                ReadPacket::Done => {
                    if bytes_written != length {
                        result = Err(PipelinedFailureKind::UnexpectedEof);
                    }
                    break;
                }
            }
            if result.is_err() {
                break;
            }
        }
        stopped.store(true, Ordering::Release);
        if reader.join().is_err() && result.is_ok() {
            result = Err(PipelinedFailureKind::ReaderPanicked);
        }
        result
    });

    let transfer = PipelinedTransfer {
        bytes_read: bytes_read.load(Ordering::Relaxed),
        bytes_written,
    };
    outcome
        .map(|()| transfer)
        .map_err(|kind| PipelinedFailure { kind, transfer })
}

/// Progress retained when a phased read or write is interrupted.
pub(crate) struct TransferFailure {
    pub kind: TransferFailureKind,
    pub transferred: usize,
}

/// Cause of an interrupted phased operation.
pub(crate) enum TransferFailureKind {
    Canceled,
    Io(io::Error),
}

/// Fallibly allocated buffer that performs request-sized I/O inside one burst.
pub(crate) struct BurstBuffer {
    bytes: Vec<u8>,
    request_bytes: usize,
}

impl BurstBuffer {
    /// Allocates at most the stream bytes still needing transfer.
    pub fn new(
        profile: TransportProfile,
        request_bytes: usize,
        remaining: u64,
    ) -> Result<Self, std::collections::TryReserveError> {
        let remaining = usize::try_from(remaining).unwrap_or(usize::MAX);
        let capacity = profile.burst_bytes.min(remaining).max(1);
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity)?;
        bytes.resize(capacity, 0);
        Ok(Self {
            bytes,
            request_bytes: request_bytes.min(capacity).max(1),
        })
    }

    /// Fills up to `limit` bytes, polling cancellation between OS requests.
    /// A short source returns successfully with fewer bytes so the engine can
    /// classify it as a source change with the correct actual-I/O counters.
    pub fn read_from<R: Read>(
        &mut self,
        source: &mut R,
        limit: usize,
        canceled: &dyn CancelProbe,
    ) -> Result<usize, TransferFailure> {
        debug_assert!(limit <= self.bytes.len());
        let mut filled = 0;
        while filled < limit {
            if canceled.is_canceled() {
                return Err(TransferFailure {
                    kind: TransferFailureKind::Canceled,
                    transferred: filled,
                });
            }
            let end = filled.saturating_add(self.request_bytes).min(limit);
            match source.read(&mut self.bytes[filled..end]) {
                Ok(0) => break,
                Ok(count) => filled = filled.saturating_add(count),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(TransferFailure {
                        kind: TransferFailureKind::Io(error),
                        transferred: filled,
                    });
                }
            }
        }
        Ok(filled)
    }

    /// Writes a filled prefix, polling cancellation between OS requests.
    pub fn write_to<W: Write>(
        &self,
        destination: &mut W,
        length: usize,
        canceled: &dyn CancelProbe,
    ) -> Result<usize, TransferFailure> {
        debug_assert!(length <= self.bytes.len());
        let mut written = 0;
        while written < length {
            if canceled.is_canceled() {
                return Err(TransferFailure {
                    kind: TransferFailureKind::Canceled,
                    transferred: written,
                });
            }
            let end = written.saturating_add(self.request_bytes).min(length);
            match destination.write(&self.bytes[written..end]) {
                Ok(0) => {
                    return Err(TransferFailure {
                        kind: TransferFailureKind::Io(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "destination made no progress",
                        )),
                        transferred: written,
                    });
                }
                Ok(count) => written = written.saturating_add(count),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(TransferFailure {
                        kind: TransferFailureKind::Io(error),
                        transferred: written,
                    });
                }
            }
        }
        Ok(written)
    }

    /// Returns the filled prefix for hashing.
    pub fn prefix(&self, length: usize) -> &[u8] {
        &self.bytes[..length]
    }

    /// Maximum bytes in one read phase.
    pub const fn capacity(&self) -> usize {
        self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::{
        BurstBuffer, PipelinedFailureKind, TransferFailureKind, TransportProfile,
        transfer_pipelined,
    };

    struct ShortReader {
        inner: Cursor<Vec<u8>>,
        maximum: usize,
    }

    impl Read for ShortReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let limit = buffer.len().min(self.maximum);
            self.inner.read(&mut buffer[..limit])
        }
    }

    #[derive(Default)]
    struct ShortWriter {
        bytes: Vec<u8>,
        maximum: usize,
    }

    impl Write for ShortWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let count = buffer.len().min(self.maximum);
            self.bytes.extend_from_slice(&buffer[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct SignalingReader {
        inner: Cursor<Vec<u8>>,
        reads: Arc<(Mutex<usize>, Condvar)>,
    }

    impl Read for SignalingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.inner.read(buffer)?;
            if count > 0 {
                let (counter_lock, signal) = &*self.reads;
                let mut count_guard = counter_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *count_guard += 1;
                signal.notify_all();
            }
            Ok(count)
        }
    }

    struct ReadAheadWriter {
        bytes: Vec<u8>,
        reads: Arc<(Mutex<usize>, Condvar)>,
        checked: bool,
    }

    impl Write for ReadAheadWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if !self.checked {
                let (counter_lock, signal) = &*self.reads;
                let count_guard = counter_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (count_guard, _) = signal
                    .wait_timeout_while(count_guard, Duration::from_secs(1), |count| *count < 2)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *count_guard < 2 {
                    return Err(io::Error::other(
                        "second read did not overlap the first write",
                    ));
                }
                self.checked = true;
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn same_spindle_buffer_phases_multiple_requests() {
        let profile = TransportProfile::same_spindle(12);
        let Ok(mut buffer) = BurstBuffer::new(profile, 4, 12) else {
            return;
        };
        let mut source = Cursor::new((0_u8..12).collect::<Vec<_>>());
        let read = buffer.read_from(&mut source, 12, &|| false);
        assert_eq!(read.ok(), Some(12));

        let mut destination = Vec::new();
        let written = buffer.write_to(&mut destination, 12, &|| false);
        assert_eq!(written.ok(), Some(12));
        assert_eq!(destination, (0_u8..12).collect::<Vec<_>>());
    }

    #[test]
    fn cancellation_reports_completed_prefix_without_writing_more() {
        let profile = TransportProfile::same_spindle(12);
        let Ok(mut buffer) = BurstBuffer::new(profile, 4, 12) else {
            return;
        };
        let mut source = Cursor::new((0_u8..12).collect::<Vec<_>>());
        let polls = std::sync::atomic::AtomicU8::new(0);
        let result = buffer.read_from(&mut source, 12, &|| {
            let next = polls.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            next >= 3
        });
        assert!(result.is_err_and(|failure| {
            matches!(failure.kind, TransferFailureKind::Canceled) && failure.transferred == 8
        }));
    }

    #[test]
    fn redirector_pipeline_preserves_order_across_short_reads_and_writes() {
        let expected = (0_u8..31).collect::<Vec<_>>();
        let mut source = ShortReader {
            inner: Cursor::new(expected.clone()),
            maximum: 3,
        };
        let mut destination = ShortWriter {
            maximum: 2,
            ..ShortWriter::default()
        };
        let mut consumed = Vec::new();
        let result = transfer_pipelined(
            &mut source,
            &mut destination,
            expected.len() as u64,
            8,
            &|| false,
            |bytes| consumed.extend_from_slice(bytes),
        );
        assert_eq!(
            result.ok(),
            Some(super::PipelinedTransfer {
                bytes_read: expected.len() as u64,
                bytes_written: expected.len() as u64,
            })
        );
        assert_eq!(destination.bytes, expected);
        assert_eq!(consumed, expected);
    }

    #[test]
    fn redirector_pipeline_actually_overlaps_read_ahead_with_writes() {
        let reads = Arc::new((Mutex::new(0), Condvar::new()));
        let mut source = SignalingReader {
            inner: Cursor::new((0_u8..8).collect()),
            reads: Arc::clone(&reads),
        };
        let mut destination = ReadAheadWriter {
            bytes: Vec::new(),
            reads,
            checked: false,
        };
        let result = transfer_pipelined(&mut source, &mut destination, 8, 4, &|| false, |_| {});
        assert!(result.is_ok());
        assert_eq!(destination.bytes, (0_u8..8).collect::<Vec<_>>());
    }

    #[test]
    fn redirector_pipeline_reports_short_source_and_actual_io() {
        let mut source = Cursor::new(vec![1_u8; 5]);
        let mut destination = Vec::new();
        let result = transfer_pipelined(&mut source, &mut destination, 9, 4, &|| false, |_| {});
        assert!(result.is_err_and(|failure| {
            matches!(failure.kind, PipelinedFailureKind::UnexpectedEof)
                && failure.transfer.bytes_read == 5
                && failure.transfer.bytes_written == 5
        }));
    }

    #[test]
    fn redirector_pipeline_retries_interrupted_writes_like_write_all() {
        struct InterruptedOnceWriter {
            interrupted: bool,
            bytes: Vec<u8>,
        }
        impl Write for InterruptedOnceWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                self.bytes.extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let expected = (0_u8..16).collect::<Vec<_>>();
        let mut source = Cursor::new(expected.clone());
        let mut destination = InterruptedOnceWriter {
            interrupted: false,
            bytes: Vec::new(),
        };
        let result = transfer_pipelined(
            &mut source,
            &mut destination,
            expected.len() as u64,
            8,
            &|| false,
            |_| {},
        );
        assert!(result.is_ok());
        assert_eq!(destination.bytes, expected);
    }

    #[test]
    fn transports_retry_interrupted_reads_and_phased_writes() {
        struct InterruptedReader {
            interrupted: bool,
            inner: Cursor<Vec<u8>>,
        }
        impl Read for InterruptedReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.inner.read(buffer)
            }
        }
        struct InterruptedWriter {
            interrupted: bool,
            bytes: Vec<u8>,
        }
        impl Write for InterruptedWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let expected = (0_u8..12).collect::<Vec<_>>();
        let mut pipeline_source = InterruptedReader {
            interrupted: false,
            inner: Cursor::new(expected.clone()),
        };
        let mut pipeline_destination = Vec::new();
        assert!(
            transfer_pipelined(
                &mut pipeline_source,
                &mut pipeline_destination,
                expected.len() as u64,
                4,
                &|| false,
                |_| {},
            )
            .is_ok()
        );
        assert_eq!(pipeline_destination, expected);

        let profile = TransportProfile::same_spindle(expected.len());
        let Ok(mut buffer) = BurstBuffer::new(profile, 4, expected.len() as u64) else {
            return;
        };
        let mut phased_source = InterruptedReader {
            interrupted: false,
            inner: Cursor::new(expected.clone()),
        };
        assert_eq!(
            buffer
                .read_from(&mut phased_source, expected.len(), &|| false)
                .ok(),
            Some(expected.len())
        );
        let mut phased_destination = InterruptedWriter {
            interrupted: false,
            bytes: Vec::new(),
        };
        assert_eq!(
            buffer
                .write_to(&mut phased_destination, expected.len(), &|| false)
                .ok(),
            Some(expected.len())
        );
        assert_eq!(phased_destination.bytes, expected);
    }

    #[test]
    fn redirector_pipeline_preserves_stage_specific_io_errors() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "read fault",
                ))
            }
        }
        struct FailingWriter {
            first: bool,
        }
        impl Write for FailingWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    return Ok(buffer.len().min(2));
                }
                Err(io::Error::new(io::ErrorKind::StorageFull, "write fault"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut source = FailingReader;
        let mut destination = Vec::new();
        let read_result =
            transfer_pipelined(&mut source, &mut destination, 8, 4, &|| false, |_| {});
        assert!(read_result.is_err_and(|failure| {
            matches!(failure.kind, PipelinedFailureKind::Read(_))
                && failure.transfer == super::PipelinedTransfer::default()
        }));

        let mut source = Cursor::new(vec![1_u8; 8]);
        let mut destination = FailingWriter { first: true };
        let write_result =
            transfer_pipelined(&mut source, &mut destination, 8, 4, &|| false, |_| {});
        assert!(write_result.is_err_and(|failure| {
            matches!(failure.kind, PipelinedFailureKind::Write(_))
                && failure.transfer.bytes_read >= 4
                && failure.transfer.bytes_written == 2
        }));
    }

    #[test]
    fn redirector_pipeline_honors_shared_cancellation() {
        struct CancelingWriter {
            cancel: Arc<AtomicBool>,
            bytes: Vec<u8>,
        }
        impl Write for CancelingWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(buffer);
                self.cancel.store(true, Ordering::Release);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let mut source = Cursor::new(vec![7_u8; 32]);
        let mut destination = CancelingWriter {
            cancel: Arc::clone(&cancel),
            bytes: Vec::new(),
        };
        let result = transfer_pipelined(
            &mut source,
            &mut destination,
            32,
            8,
            cancel.as_ref(),
            |_| {},
        );
        assert!(result.is_err_and(|failure| {
            matches!(failure.kind, PipelinedFailureKind::Canceled)
                && failure.transfer.bytes_written == 8
                && failure.transfer.bytes_read >= failure.transfer.bytes_written
        }));
    }

    #[test]
    #[allow(clippy::panic)]
    fn redirector_pipeline_turns_reader_panic_into_a_bounded_failure() {
        struct PanickingReader;
        impl Read for PanickingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                panic!("injected reader panic");
            }
        }

        let mut source = PanickingReader;
        let mut destination = Vec::new();
        let result = transfer_pipelined(&mut source, &mut destination, 8, 4, &|| false, |_| {});
        assert!(result.is_err_and(|failure| {
            matches!(failure.kind, PipelinedFailureKind::ReaderPanicked)
                && failure.transfer.bytes_read == 0
                && failure.transfer.bytes_written == 0
        }));
    }
}
