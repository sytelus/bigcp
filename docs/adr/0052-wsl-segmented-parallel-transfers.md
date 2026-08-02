# ADR 0052: Measured WSL profile and segmented parallel transfers

**Status:** Amended by ADR 0053 (network-class generic-UNC *sources* now also
stripe plain-small dispatch behind a measured-latency gate; this ADR's
"local and generic-UNC combinations keep directory affinity" statement holds
only for local sources and loopback-class redirector latencies)

## Context

ADR 0046 specialized WSL behind its own transport/profile identity but had no
operator-approved disposable endpoint, so its 8 MiB/16-worker row and
destination-only striping were bounded static hypotheses (H7). On 2026-08-02
the mechanisms were measured against a real `\\wsl.localhost` WSL 2
distribution (BENCHMARKS.md, 2026-08-02 entry). Three findings changed the
numbers and one scope decision:

- **Small-file creates kept scaling past 16 workers:** 2,062 → 3,917 files/s
  at 16 → 32, with 64 adding only ~7% more. The knee is 32.
- **A 9P source is as expensive as a 9P destination:** WSL→Win phase timing
  shows open_src 1,491 µs + read 812 µs per file — ~2.3 ms of the ~2.4 ms
  per-file cost is source-side provider round trips. ADR 0046 striped only
  WSL *destinations*; directory affinity on a WSL *source* therefore
  serialized the expensive side to protect a cheap local NTFS index.
- **One 9P handle (fid) caps near ~230–290 MB/s** at every request size from
  1 to 64 MiB, while two concurrent handles reach 408 MB/s aggregate and
  robocopy reaches 560 MB/s on a single file — the per-handle ceiling, not
  the medium, is the large-file bottleneck, and intra-file parallel I/O
  works through p9rdr.

## Decision

- Raise the WSL Auto row to 8 MiB/32 workers (`WSL_WORKERS`), superseding ADR
  0046's 16-worker value. Generic UNC keeps 16; the seams stay independent.
- Stripe plain-small jobs across the round-robin shard when **either**
  endpoint is WSL (`worker_dispatch` now takes `source_is_wsl`), superseding
  ADR 0046's destination-only striping scope. Local and generic-UNC
  combinations keep the measured directory affinity.
- Add a segmented parallel transfer for large files crossing the Plan 9
  boundary exactly once. Eligibility is a pure function (`segment_plan` in
  `core::engine`), and every condition is load-bearing: the `Wsl` transport,
  exactly one WSL side (the local side hosts the digest pass), non-sparse,
  unnamed-stream-only, unnamed size ≥ 64 MiB, not checkpoint-eligible
  (checkpoints attest a contiguous verified prefix, which out-of-order
  segment writes violate), and no live resume candidate. An eligible file's
  single opaque temp is written/read as `K = clamp(size/64 MiB, 2, 8)`
  contiguous chunk-aligned segments on scoped threads. Each segment thread
  opens its own source handle — revalidated against the enumeration snapshot
  exactly like the ordered open — and its own `SegmentWriter` over the temp:
  every path reopen is identity-proven against the temp's captured
  `FileIdentity` before any write
  (`DestinationTemp::open_segment_writer` in `bigcp-win`). The coordinator
  handle's delete-on-close disposition still owns cleanup, so any error or
  cancellation removes the partial temp, and the commit/rename/stamp tail is
  the ordered path's own code (`finish_streamed`).
- Preserve the whole-file xxh3-128 digest with a local-side pass that is
  neither copy nor verification I/O (no counter changes): Win→WSL hashes the
  local source concurrently with the segment writes; WSL→Win hashes the
  just-written local temp before publication — digesting exactly the bytes
  being published, a strictly stronger attestation than hashing the source
  stream in flight.
- Cut three measured per-file round trips: skip the pre-commit destination
  probe for NEW streamed files (the non-replacing rename already detects a
  concurrently appeared name atomically; replacements keep the probe), reuse
  the preflight root stat instead of re-stating the source root, and capture
  a checkpoint temp's identity once per temp instead of per append.
- Keep ADR 0046's remaining decisions unchanged: the 8 MiB chunk window,
  sequential cache hints, deferred projected stamp, skipped `CREATE_NEW`
  metadata query, and the one-time `--accept-remote-paths` gate.

## Consequences

Large one-sided WSL files no longer sit under the per-fid ceiling: measured
medians moved 224 → 517.7 MB/s Win→WSL and 286 → 514 MB/s WSL→Win, with
small files 2,062 → 3,704 files/s Win→WSL (BENCHMARKS.md). The costs are
bounded and deliberate: a segmented file holds K × 8 MiB of segment buffers
(≤ 64 MiB) while it runs; the live-rate display can stall during one file
exactly as it does on the ordered path; files at or above the 16 GiB default
checkpoint threshold keep the ordered, resumable path; WSL↔WSL copies keep
the ordered path (no local side for the digest pass); and sparse,
named-stream-bearing, and resume-candidate files are untouched. UNC, local,
and same-spindle transports are byte-for-byte unchanged — `copy_streamed`
remains the fallback for every other modality.

## Validation

Pure tests pin the planner and the mechanics:
`segment_plan_rejects_every_ineligible_modality` and
`segment_plan_clamps_counts_and_covers_exactly`
(eligibility, K-clamping, exact chunk-aligned coverage),
`segmented_copy_publishes_identical_content_and_digest_both_directions`
(patterned content plus whole-file digest in both directions),
`segmented_copy_cancel_leaves_no_destination_object` (mid-file cancel leaves
neither final name nor temp), and bigcp-win's
`segment_writer_requires_identity_proof_and_preserves_delete_on_close`
(a wrong identity gets `InvalidData` and no writer; delete-on-close survives
the reopen cycle). The amended
`wsl_endpoints_stripe_small_files_without_changing_other_affinity` now
also proves WSL-source striping and the non-WSL affinity counterfactual, and
`remote_source_caps_local_workers_and_wsl_is_independently_profiled` pins the
32-worker row. Real-endpoint verification (2026-08-02, `\\wsl.localhost\u2`):
standalone `bigcp verify` green on segmented copies both directions and on
the 2,021-object small tree, destination hashes byte-identical, local→local
regression unchanged, full 218-test suite green.
