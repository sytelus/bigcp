# ADR 0036: Topology-gated same-spindle phased transport

**Status:** Accepted; hardware performance evidence pending; amended by ADR
0056 (the phased gather now caps batches at 4096 files instead of the queue
depth and waits out coordinator pauses below a 64-file/burst-8 floor, so
tiny-file sweeps are sized by the burst budget rather than shattered by a
2 ms timeout; the three-phase contract is unchanged)

## Context

The standard buffered engine alternates a source read and destination write per
request. That is appropriate when devices can progress independently and on
solid-state media, where worker concurrency is valuable. When both roots map to
one rotational disk, however, request-at-a-time alternation and concurrent
small-file workers repeatedly move the mechanical head between source and
destination regions. FastCopy's documented same-HDD design instead fills a
large buffer from the source and writes it in bulk.

The project already queried volume disk extents and seek penalty, but only
reported overlap. Adding the optimization must not fork copy semantics, weaken
checkpoint integrity, or burden NTFS/ReFS/FAT policy and independent-device
hot paths with an alternative copy engine.

## Decision

Select one immutable `TransportProfile` during preflight. `SameSpindle` is
chosen only when source and destination disk-number sets intersect and either
effective device profile is HDD; intersecting SSDs and unknown topology retain
`Standard`. An explicit HDD profile can supply media class, but missing physical
overlap is never guessed.

The new `core::transport` module owns the standard/same-spindle policy record
and a fallibly allocated synchronous burst buffer. The standard dense loop is
unchanged. For same-spindle dense, sparse allocated-range, and named-stream
data, request-sized reads fill a bounded burst before any request-sized writes
begin. Checkpoint boundaries cap each burst, preserving contiguous written
watermarks and offset-ordered hashes. Cancellation is polled between requests;
actual completed read/write bytes are retained in counters on interruption.

Same-spindle plain-small work uses exactly one worker. It gathers at most one
burst budget of jobs, reads and validates their sources without destination
mutation, writes and finishes the prepared destinations, then revalidates the
still-open source handles before returning success. Coordinator-owned inline
work drains that worker first, preventing a large transfer from interleaving
with the small-file phases. Rare auxiliary-data files use the existing
transactional engine after the ordinary batch.

The default burst is 256 MiB on the VISION's 32+ GiB target machines. It is
capped by `--tune mem`; `--tune same-spindle-burst=` allows a 1 MiB–1 GiB
override and must hold at least one effective request and whole small file.
Effective kind and burst are recorded in the profile log event and report.

This topology changes performance only. It causes no fidelity loss, durability
change, or critical safety risk, so it produces one startup status message but
does not add a confirmation. ADR 0037 later combines remote/WSL acceptance
with FAT/exFAT fidelity acceptance and the Quick-removal opt-out in the same
single startup prompt.

## Consequences

Same-spindle behavior can now be improved inside `transport.rs`, the phased
worker, and the three stream loops without changing classification,
publication, verification, filesystem policy, or the standard transport.
Memory, channels, job count, and burst sizes remain bounded, and allocation
failure is reported rather than aborting through an infallible large vector.

Routine tests cover transport selection (including same-device SSD exclusion),
multi-request phase ordering, cancellation progress, CLI bounds, and a verified
same-volume copy containing batched small files plus dense, sparse, and named
stream data. A real-HDD performance test is deliberately not part of routine
CI and was not run with this change: repository policy requires separate owner
approval for bounded performance workloads. Until the PLAN §12.5 `[HW]` cell is
archived in `BENCHMARKS.md`, the implementation is correctness-tested but does
not claim a measured speedup or universally optimal 256 MiB default.
