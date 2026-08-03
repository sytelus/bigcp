# ADR 0056: Same-drive scheduling and topology visibility

**Status:** Accepted (amends ADR 0048's same-physical-disk exclusion and ADR
0036's phased gather policy; extends the PLAN §5.8 directory-affinity design
with a bounded oversized-directory exception)

## Context

A dedicated same-drive optimization round examined the three topologies where
source and destination share one device: one SSD (same volume or two volumes
on one physical disk), one rotational spindle, and one file server. The
same-SSD NTFS cell was measured on this project's benchmark host (C: KIOXIA
EG6 1 TB NVMe, system volume, C:→C:); the HDD and same-server cells were
reviewed best-effort without hardware (no HDD attached; no LAN share), per
the round's instruction. Findings (BENCHMARKS.md "2026-08-02 same-drive NTFS
SSD"):

- **The flat single-directory cell was the only cell robocopy won.** Nested
  small files (10,000 × 4 KiB in 100 directories) and the large stream both
  had bigcp ahead, but a 2,000-file *single* directory ran 3,729–3,793
  files/s against robocopy `/MT:32`'s 4,271–4,364. A robocopy thread sweep
  isolated why: one NTFS directory index serializes creates, but a *small*
  interleave still wins — `/MT:2` 3,454, `/MT:4` 4,548, `/MT:8` 4,116,
  `/MT:16` 3,787 files/s. Pure one-worker affinity (PLAN §5.8 part 1) leaves
  every other worker idle when one huge directory dominates the run, and
  VISION sizes a single directory at up to ~1M entries.
- **ADR 0048's same-physical-disk exclusion had no safety rationale.** The
  relative parent-handle create is purely a destination-side open-path
  mechanism; nothing about the source sharing a physical disk affects it.
  The exclusion was scoping conservatism from the distinct-drive round.
- **The phased same-spindle gather could starve its own amplitude.** The
  batch gatherer waited at most 2 ms for the next job and capped the batch
  at the queue depth (1024 jobs). A coordinator pause above 2 ms (directory
  enumeration, classification, journal work — routine on the very HDD this
  transport serves) shattered the batch, and for 4 KiB files the count cap
  set the sweep frequency at ~4 MiB of payload against a 256 MiB burst
  budget — 64× more source↔destination head sweeps than the budget implies.
- **Reparse work could interleave with HDD phases.** Inline large/
  transactional files drain the phased worker before touching the
  destination; inline reparse creation/repair did not.
- **A same-server UNC pair pays double traversal invisibly.** Both endpoints
  remote → every byte crosses this client twice (read from the server,
  written back to it). The probe already returns the canonical share root
  and the remote volume serial; neither was consulted. VISION line 27
  prohibits using an OS server-side copy but explicitly allows informing
  the user. The same clause covers the ReFS block-clone hint, which fired
  only in the post-run report — after a potentially hours-long run.

## Decision

- **Oversized directories rotate across a bounded affine lane set**
  (`core::copy`): a directory with ≥512 joined source entries
  (`DIRECTORY_SUBSHARD_MIN`) dispatches its plain-small jobs round-robin
  across 4 lanes (`SINGLE_DIRECTORY_LANES`, compile-time capped ≤8) derived
  from the parent-directory hash; ordinary directories keep classic
  one-worker affinity. This preserves all four measured parts of PLAN §5.8's
  affinity design for typical trees while ending the idle-pool convoy on
  monster directories. Measured on the flat 2,000-file cell: 4,749–5,035
  files/s vs 3,729–3,793 single-lane — ahead of robocopy's best thread
  count. The nested cell is unchanged (its 100-entry directories sit below
  the threshold).
- **Relative NTFS creates no longer exclude same-physical-disk pairs**
  (amends ADR 0048): the gate keeps local NTFS on both sides and the
  `standard` transport (which still excludes same-spindle HDD pairs, whose
  transport is `SameSpindle`). An order-controlled same-disk A/B measured
  the change performance-neutral (within ±2.4% noise) in the cache-hot
  small-file regime; it is retained as a path un-forking — same-disk and
  distinct-disk NTFS now behave identically — with ADR 0048's
  correctness-proven / speedup-pending posture intact.
- **The phased gather gains amplitude and patience** (amends ADR 0036): the
  batch cap becomes `SAME_SPINDLE_BATCH_FILES` = 4096 (compile-time ≥ the
  queue depth; the burst byte budget still bounds payload memory, and the
  cap bounds open source handles and job records — a few MiB), and the
  gather wait is floor-gated: below 64 files *and* burst/8 bytes the
  gatherer waits up to 50 ms for the coordinator (patient), otherwise 2 ms
  (quick). Worst case adds one 50 ms wait to the final undersized batch of
  a run and up to 50 ms to a coordinator drain that interrupts a gather.
  The three-phase contract, carry rule, and exact counters are unchanged.
  No HDD was available; the wall-clock claim is registered as hypothesis
  H10 pending the PLAN §12.5 `[HW]` cell.
- **Reparse mutation drains the phased worker first** (`handle_reparse`),
  in symmetry with `copy_classified`'s inline-file drain, so link I/O
  cannot interleave with the source/destination phases.
- **Same-server topology becomes visible** (never a copy-path change): a
  `same_share_double_traversal` report hint plus a clause joined to the
  existing remote-topology startup notice fire when both endpoints are
  generic UNC on one server. Equal case-folded share roots are proof
  ("high" confidence); different shares with an equal server name, volume
  serial, *and* filesystem are inferred ("medium" — a bare u32 serial is
  never trusted across servers). WSL pairs are excluded (`wsl_interop`
  owns that story). The ReFS block-clone fact is additionally announced at
  preflight (message only; the report hint is unchanged).

## Consequences

- The copy engine, completion semantics, journal/audit formats, and every
  non-affine dispatch path are untouched; the same-SSD pair deliberately
  keeps the `Standard` transport and the ADR 0055 pipeline (on one device
  the pipeline's ceiling is coupled — roughly half the drive's mixed R/W
  bandwidth — but overlap still beats alternation, and the only stronger
  same-volume options are the OS engines VISION prohibits).
- PLAN §5.8's "do not regress any of the four parts" evidence is refined,
  not regressed: part 1's one-worker rule now carries a measured, bounded
  exception for oversized directories; parts 2–4 are untouched. The ADR
  0048 parent cache simply materializes once per lane (≤4 handles per
  oversized directory instead of 1).
- Same-drive benchmark methodology caveats join the register: freshly
  created fixtures are page-cache-hot (the read side runs from RAM), one
  SLC cache absorbs both sides' traffic, the theoretical ceiling is half
  the device's mixed bandwidth, and thermals accumulate ~2× faster than on
  distinct drives. Same-volume figures compare tool-vs-tool within one
  machine window only.
- HDD amplitude constants (4096 files, 64-file/burst-8 floor, 50 ms/2 ms
  waits) are reviewed defaults, not measured optima; H10 records what a
  future approved HDD session must confirm.

## Validation

- `oversized_directories_rotate_across_a_bounded_lane_set` — the lane
  policy table and thresholds (`core::copy`).
- `relative_ntfs_creates_require_local_ntfs_and_the_standard_transport` —
  the widened gate, including the same-spindle-transport and redirector
  exclusions (`core::copy`).
- `same_spindle_gather_patience_is_floor_gated` — the two-timeout policy
  and both floors (`core::worker`); compile-time guards pin the lane
  ceiling and batch-cap/queue-depth ordering.
- `same_share_detection_requires_server_proof` — the full confidence table:
  case-folded root equality, verbatim/plain prefix equivalence, the
  serial+filesystem upgrade, and the never-trust-bare-serial rule
  (`core::copy`).
- Benchmarks (BENCHMARKS.md "2026-08-02 same-drive NTFS SSD"): the required
  `copy` vs robocopy vs bigcp comparison, the robocopy thread sweep, the
  before/after lane measurement with an interleaved robocopy control, and
  the order-controlled relative-create A/B. Full workspace suite green
  (229 tests) with fmt/clippy pedantic and both safety scripts.
