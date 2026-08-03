# ADR 0057: Staged in-flight hash, local pipeline depth, and lazy worker pool

**Status:** Accepted (extends ADR 0055's pipeline for the local standard
transport; PLAN §5.11's always-hashed-in-flight rule is unchanged — this ADR
is what keeps its "~free" premise true on local hardware)

## Context

The same-drive round left one cell unwon: the same-SSD 2 GiB single stream,
where `cmd copy` measured ~15–40% ahead of bigcp within every window
(BENCHMARKS.md "2026-08-02 same-drive NTFS SSD, large-stream close-out").
Instrumented decomposition on a cache-hot 2 GiB stream found three
independent mechanisms, none of them device physics:

- **The in-flight digest sat on the write critical path.** `consume`
  (xxh3) ran on the writer thread before each write: hash ~390 ms plus
  write ~920 ms per 2 GiB serialized to a ~2.6 GB/s writer ceiling while
  the reader idled — the pipeline measured *at* the read/write alternation
  ceiling (harmonic mean ~2.0 GB/s in the same window), i.e. the overlap
  ADR 0055 built was being spent on hashing. `copy` hashes nothing; its
  kernel path also skips the user-mode read leg entirely (cache-to-cache),
  and VISION's one-engine rule rightly forbids matching that mechanism —
  but not its wall clock.
- **Two buffers cannot absorb local jitter.** With closely matched stages
  (read ~6.8 ms vs write ~7.2 ms per 16 MiB request), momentary reader
  stalls starved the writer for ~60 ms per 2 GiB (measured writer idle
  waiting on packets). On network transports one side dominates by orders
  of magnitude and two buffers remain right.
- **A fixed ~90 ms run floor, twice `cmd.exe`'s spawn cost.** The largest
  avoidable slice (~14 ms) was `FileWorkers::new` eagerly preallocating
  the bounded result ring — worker_count × queue-depth slots (64 × 1024
  `CompletedCopy`-sized entries, tens of MB zeroed) — plus thread
  spawn/join for a pool that a single-large-file run never submits one job
  to.

## Decision

- **`consume` moves off the write leg** (`core::transport`): on every
  pipelined transfer it now runs before the packet reaches the writer —
  inline on the reader thread at two-buffer depth (network transports,
  where round trips dwarf it), and on a **dedicated hash-stage thread**
  between reader and writer at local depth. Read order equals write order
  (ordered channels), so contiguous hashing and the checkpoint-boundary
  invariant are untouched; the digest may now run up to `depth` requests
  ahead of the confirmed prefix, which the existing "checkpoints only at
  segment ends after `Ok`" rule already tolerates. PLAN §5.11's rule that
  every large stream is hashed in flight stands — every streamed file
  still logs its xxh3 digest; the digest just stopped costing the row.
- **`LOCAL_PIPELINE_BUFFERS` = 3** for standard-transport unnamed large
  streams: one extra request buffer absorbs reader jitter against the
  closely matched writer; the hash stage holds it while digesting.
  Redirector and WSL streams keep `PIPELINE_BUFFERS` = 2 and their
  reader-inline consume — no network behavior change without network
  measurement. The `mem` budget reserves three coordinator chunks on the
  standard transport (two on redirectors, as before).
- **The worker pool materializes lazily** (`core::worker`): result ring,
  job queues, and threads are created on the first submitted job.
  Coordinator-inline-only runs (one large stream, dry runs, empty trees)
  never pay spawn, ring preallocation, or join. Before any spawn,
  `receive_timeout` sleeps out its timeout — the exact empty-channel
  behavior of the eager pool.

## Consequences

- Same-window interleaved result (BENCHMARKS.md): the large-stream cell
  went from losing every fresh round to winning or tying them; robocopy
  `/J` stays far behind on this cell in every window. The other two rows
  were already won and are unaffected (the small-file path does not use
  the pipeline, and its first job simply materializes the pool).
- Memory: one additional in-flight chunk (16 MiB at defaults) per
  coordinator-inline large stream on the standard transport only, honestly
  accounted by `mem`; runs that never dispatch a small job now allocate
  *less* (no result ring).
- One more short-lived thread per local coordinator-inline large stream
  (the hash stage), bounded like the reader thread it joins.
- The digest-evidence contract is unchanged everywhere: report/log `hash`
  fields, segmented WSL local-side digests (ADR 0052), checkpoint prefix
  digests, and `--verify` inputs are byte-identical to before.

## Validation

- `local_depth_pipeline_hashes_on_its_own_stage_in_order` — at local depth
  the staged consume sees every byte exactly once, in order, with the
  written stream identical, on request sizes that do not divide the length
  (`core::transport`).
- The existing pipeline suite (overlap proof, short-read/write ordering,
  interruption retries, stage-specific errors, cancellation, reader-panic
  containment, actual-I/O counters) passes unchanged at two-buffer depth,
  and `standard_memory_budget_reserves_the_pipelined_coordinator_chunks`
  pins the three-chunk standard reservation (`core::devprofile`).
- `worker_pool_bounds_are_enforced_at_construction` pins the lazy pool:
  no threads or queues before the first job, full spawn on demand,
  idempotent re-entry (`core::worker`).
- Benchmarks: BENCHMARKS.md "2026-08-02 same-drive NTFS SSD, large-stream
  close-out" records the stage decomposition, the alternation-ceiling
  finding, and the final three-tool interleaved windows. Full 230-test
  workspace suite green with fmt/clippy pedantic and both safety scripts.
