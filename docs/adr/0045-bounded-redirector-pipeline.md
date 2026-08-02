# ADR 0045: Bound and overlap redirector transfers

**Status:** Accepted

**Superseded in part by ADR 0046:** WSL now has a distinct transport identity,
8 MiB/16-worker Auto row, destination-create striping, and provider-specific
handle/metadata-call reductions. Generic UNC retains this ADR's policy.

**Extended by ADR 0055:** the two-buffer pipeline now also carries
standard-transport unnamed large streams, its constant is renamed
`PIPELINE_BUFFERS`, and standard `mem` accounting reserves the same two
coordinator chunks. This ADR's "standard local accounting remains unchanged"
statements describe the state before ADR 0055.

## Context

ADR 0037 isolated UNC, mapped-drive, and WSL policy but deliberately retained
the local standard transport: one synchronous source read followed by one
synchronous destination write. That preserved correctness and kept the local
hot path unchanged, but it also left a redirector-specific throughput ceiling.
While a streamed write was waiting, no next source read could advance. Large
files also remained coordinator-owned even when they were below the checkpoint
threshold, so a directory containing several independent large files used only
one transfer at a time.

Microsoft's SMB guidance recommends keeping multiple operations in flight to
cover per-operation latency. The project still does not have an approved live
UNC scratch endpoint, so the amount of improvement on any particular server,
network, signing/encryption configuration, or WSL distribution is unmeasured.
The architecture must therefore improve concurrency without claiming a new
universal optimum or changing local-device behavior.

## Decision

Add an immutable `Redirector` transport selected whenever either endpoint is
generic UNC, a mapped network path, or WSL:

This supersedes only ADR 0043's count of two topology transports; its one-engine
and two-completion-strategy decision remains in force.

- Keep the existing synchronous buffered handles and endpoint-specific 8 MiB
  generic-UNC / 4 MiB WSL request defaults. Do not add IOCP, unbuffered I/O,
  queue-depth flags, another OS copy engine, or mutable runtime tuning.
- For dense, sparse-range, and named-stream transfers, use exactly two
  fallibly allocated request buffers. A scoped reader thread fills the next
  buffer while the calling thread writes the previous one. Each handle still
  has at most one synchronous request active; the optimization overlaps the
  source and destination stages rather than splitting a file into out-of-order
  writes.
- Hash bytes on the calling thread in destination-write order. End every
  pipeline segment at a checkpoint boundary, so journal watermarks remain
  contiguous and retain the existing verified-prefix resume invariant.
- Dispatch non-sparse redirector files through the existing bounded worker pool
  when no stream requires coordinator-owned checkpoint persistence. Assign
  those streamed jobs round-robin so independent large files in one directory
  occupy different workers. A discovered named stream at or
  above the checkpoint threshold promotes the untouched file back to the
  coordinator.
- Mirror graceful cancellation into a shared atomic observed by worker
  transfers. The coordinator polls the front end while waiting for completions;
  a user stop or device/space breaker stops streamed workers between requests.
- Make `mem=` account for two coordinator chunks and
  `max(large_threshold, 2 * chunk)` per active redirector worker. Standard local
  and same-spindle accounting remain unchanged.
- Record `redirector` as the transport kind in audit profiles and reports. Keep
  the existing one-time `--accept-remote-paths` startup gate; the optimization
  introduces no additional user decision.

## Consequences

Local standard and same-spindle selection, buffers, worker routing, Win32
handles, metadata projection, publication, checkpoints, verification, and
default profile values are unchanged. Redirector copies can overlap source and
destination latency, and multiple non-checkpointed streamed files can progress
concurrently. The bounded cost is one extra request buffer and one scoped
reader thread per active streamed file.

This is not a claim that bigcp now saturates every SMB or WSL path. A single
handle still has synchronous I/O depth one, remote topology remains opaque,
and server storage, network latency, SMB signing/encryption/compression, DFS,
antivirus, and provider caching can dominate. At the time of this decision,
any claim against robocopy and any change to its generic-UNC 8 MiB/16-worker or
WSL 4 MiB/8-worker defaults required the approved remote benchmark protocol.
ADR 0046 later changes the WSL row without claiming a measured optimum; future
claims or tuning still require that protocol and evidence in `BENCHMARKS.md`.

## Validation

Routine local tests prove exact byte ordering under forced short reads and
writes, actual read-ahead/write overlap, short-source accounting, shared
cancellation, bounded reader-panic handling, redirector selection, two-buffer
memory accounting, and the dispatch boundary that leaves sparse and
checkpoint-eligible work on the coordinator. The ordinary standard and
same-spindle suites guard non-remote behavior. Live UNC writes and performance
measurements remain prohibited until an operator approves an exact disposable
share path, file/byte budget, duration, and expected storage impact.
