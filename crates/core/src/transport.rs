//! Static copy-transport policy selected from preflight topology.
//!
//! The normal path retains one request-sized buffer. A rotational source and
//! destination that share a physical disk use a much larger bounded staging
//! buffer so reads and writes occur in coarse phases instead of forcing a disk
//! head seek after every request. This module owns only transport mechanics;
//! file semantics, hashing, checkpoints, and publication remain in `engine`.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Effective data-transport strategy recorded in logs and reports.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Existing request-at-a-time buffered streaming for independent devices
    /// and solid-state same-device copies.
    #[default]
    Standard,
    /// Phased reads and writes for intersecting rotational media.
    SameSpindle,
}

/// Immutable transport settings selected before copy I/O starts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TransportProfile {
    /// Selected strategy.
    pub kind: TransportKind,
    /// Maximum bytes staged before switching from reads to writes.
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

    /// Whether reads and writes must be serialized into coarse phases.
    #[must_use]
    pub const fn is_same_spindle(self) -> bool {
        matches!(self.kind, TransportKind::SameSpindle)
    }
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
        canceled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferFailure> {
        debug_assert!(limit <= self.bytes.len());
        let mut filled = 0;
        while filled < limit {
            if canceled() {
                return Err(TransferFailure {
                    kind: TransferFailureKind::Canceled,
                    transferred: filled,
                });
            }
            let end = filled.saturating_add(self.request_bytes).min(limit);
            match source.read(&mut self.bytes[filled..end]) {
                Ok(0) => break,
                Ok(count) => filled = filled.saturating_add(count),
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
        canceled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferFailure> {
        debug_assert!(length <= self.bytes.len());
        let mut written = 0;
        while written < length {
            if canceled() {
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
    use std::io::Cursor;

    use super::{BurstBuffer, TransferFailureKind, TransportProfile};

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
        let polls = std::cell::Cell::new(0_u8);
        let result = buffer.read_from(&mut source, 12, &|| {
            let next = polls.get().saturating_add(1);
            polls.set(next);
            next >= 3
        });
        assert!(result.is_err_and(|failure| {
            matches!(failure.kind, TransferFailureKind::Canceled) && failure.transferred == 8
        }));
    }
}
