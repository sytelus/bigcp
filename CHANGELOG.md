# Changelog

All notable changes follow Keep a Changelog. The project uses semantic
versioning once its 1.0 release gates are complete.

## [Unreleased]

### Added

- Initial pre-1.0 Windows implementation: safe Win32 boundary, NTFS/ReFS
  preflight, iterative join, bounded workers, atomic temp publication,
  stream/EA/sparse/reparse handling, CRC journal, JSONL audit, JSON report,
  two verification forms, CLI, TUI, and independent sandbox testkit.
- Structurally confined unit/end-to-end tests and maintainer documentation.

### Changed

- Hardened checkpoint resume with source/temp filesystem identities,
  non-following temp opens, fail-closed journal version handling, and
  handle-bound cleanup that cannot delete a path replacement.
- Made ordinary source, ADS, EA, directory-finalization, and reparse-copy
  handles non-following and identity-checked, strengthened new-destination
  collision detection, and reject non-directory standalone verification roots
  before traversal.

### 2026-07-29 review pass

- Fixed EFS preservation: the encrypted attribute is now requested at temp
  creation (`FILE_ATTRIBUTE_ENCRYPTED` in the open), because path-based
  `EncryptFileW` can never reach a delete-pending temp; the engine detects and
  reports a per-file downgrade when creation-time encryption did not take.
- Fixed device profiling for production paths: `\\?\`-prefixed roots were
  failing the drive-letter check, silently disabling profiling everywhere.
- Made resume checkpoints survive `--dry-run` and unrelated flag changes:
  dry-run no longer opens the journal, and the option-hash was replaced by a
  resume-protocol signature; checkpoints self-validate on reuse.
- Journal hardening: interior bad records are skipped (only a genuinely torn
  tail truncates), missing trailing newlines self-heal, and clean runs compact
  the journal to its job header plus live checkpoints.
- I6 accounting de-tautologized: discovery and outcomes are counted
  independently so reconciliation genuinely detects dropped files; join
  failures account entire subtrees as not-attempted; clean cancellation is a
  warning, not an error.
- Symlink fidelity: `SubstituteName` plus the relative flag are authoritative
  (never `PrintName`); volume-GUID targets are refused. Sparse enumeration
  rejects a zero-range progress stall. Worker panics are caught and surfaced
  instead of deadlocking the run.
- CLI contract: usage errors exit 5 (never 2), `--help`/`--version` exit 0,
  `NO_COLOR` is honored, and invariant breaches still write the report before
  exiting 6.
- Added `--analyze`: bounded live-run insight (five size-class buckets, top-20
  slowest copies, 5-second stat cadence) as one log event and one report
  section (VISION live-run analysis requirement).
- Testkit generator now hard-caps scenarios at 10,000 entries and depth 32,
  enforcing the VISION scale prohibition structurally; `check-test-safety.ps1`
  rewritten with anchored assertions and a destructive-command scan.
- Documentation: PLAN §12.0 absolute test prohibitions, §13.2 implementation
  status, §13.3 extension seams; all 21 `PLAN_DEVIATIONS.md` entries
  dispositioned; LIMITATIONS/README/TESTING/BENCHMARKS aligned.

### 2026-07-29 test-policy update

- Replaced the hardcoded F:/G:/H: test-drive blacklist with a whitelist: tests
  may touch only the Windows system drive and the drive holding the code
  checkout; all other drive letters, and paths without one (UNC, volume-GUID),
  are rejected before any filesystem access. A whitelist stays correct on any
  machine; a fixed blacklist fails open when drives are lettered differently.
- Introduced two test tiers. The default suite is correctness-only: quick,
  few files, harmless. Performance, stress, thousands-of-files, and
  long-running tests are heavy-tier: marked `#[ignore = "heavy: …"]` and
  additionally gated on `BIGCP_ALLOW_HEAVY_TESTS=1`, which only the operator
  may set — the safety script fails if any repository code calls `set_var`.
- The testkit generator now enforces the tiers structurally: routine runs cap
  scenarios at 1,000 entries and 64 MiB; the opt-in raises this to the
  absolute VISION-derived caps (10,000 entries, 1 GiB), which no flag lifts.
- Documented the permission protocol (PLAN §12.0, docs/TESTING.md): before
  running any disabled heavy test, the implementer must ask the owner and
  state the exact tests, file counts, bytes, target root, duration, and drive
  impact.

### 2026-07-29 fragmentation-evidence additions

- Documented the fragmentation stance in `docs/DESIGN.md`: dense large files
  are preallocated to full size at temp creation so parallel streams cannot
  interleave extents, small files are written in one shot, and sparse files
  are exempt by design.
- Added read-only extent measurement: `bigcp_win::extent_count`
  (`FSCTL_GET_RETRIEVAL_POINTERS`, hole-aware, merges physically adjacent
  runs) and the `bigcp-testkit extents` command, which reports per-tree
  fragmentation evidence. `BENCHMARKS.md` and PLAN §12.6 now require that
  evidence in every performance entry, so a regression that dropped
  preallocation would be caught by measurement instead of staying invisible.
- `LIMITATIONS.md` now states plainly that the same-spindle alternating-burst
  engine is pre-1.0 release work: until it lands, same-spindle HDD copies run
  under the generic HDD profile (queue depth 2, one stream, 16 MiB chunks) —
  bounded, not yet optimized.

### 2026-07-29 deviation-registry cleanup

- Verified every PLAN_DEVIATIONS.md disposition against the tree: all nine
  normative entries are present in PLAN.md, and all release-work items are
  registered in PLAN §13.2. Removed the resolved normative entries (history
  preserved in `docs/REVIEW_2026-07-29.md` and git) and rewrote the file as an
  open-items registry — 14 items, each with its required verification — plus a
  proposed five-phase execution order (verification net first, then cheap
  self-contained wins, transport, benchmark-gated orchestration, and elevated
  matrices last). Two review findings were promoted to tracked open items:
  mid-file cancellation granularity and the standalone-verify
  error-vs-divergence distinction.

### 2026-07-29 complexity-control pass (ADR 0027)

#### Added

- Device/disk-full circuit breaker: five consecutive device-gone or
  disk-full failures stop the run early and resumably with exit code 4 and
  clear reconnect/free-space guidance, instead of grinding through every
  remaining object. New Win32 codes 21 and 55 classify as device-gone.
- Mid-file graceful cancellation: `q`/Ctrl+C now takes effect between chunks
  inside a large file (the partial temp self-deletes or resumes from its
  checkpoint); a clean cancel stays a warning, never an error.
- ETA display (VISION `/ETA`): remaining-known-work estimate in the TUI
  dashboard and plain output, suppressed while writes are idle; backed by a
  new additive `bytes_enumerated` counter.
- Standalone verify now reports unreadable objects as "could not be
  verified (read failed)" instead of claiming they differ.

#### Removed (complexity control — declared limitations instead)

- The IOCP overlapped-ring engine design: the 1.0 large-file design is now a
  sequential unbuffered reader/writer pipeline (PLAN §5.9) — same robocopy
  `/J` semantics, a fraction of the complexity and test burden.
- `qd-src`/`qd-dst` tune keys, profile queue-depth fields, and the log
  profile event's queue-depth fields (pre-1.0 schema change): the sequential
  pipeline has no queue depth to tune.
- The bounded runtime governor, free-space forecast, Restart Manager
  lock-owner naming, profiler vendor/hotplug/cache extras, handle-based ADS
  discovery, deferred-close finalizer pool, per-device scheduler, decorative
  TUI widgets, `VerifyOptions.report_path`, modeled audit-drain state,
  orphan-scan cleanup, and differential-copier release gates — each deletion
  recorded inline in PLAN.md with its user-facing consequence in
  LIMITATIONS.md.

### 2026-07-29 buffered engine finalized (ADR 0028)

- The owner clarified that robocopy `/J` (unbuffered I/O) was never an
  intentional VISION mandate and removed it from the expressed defaults.
  Consequently the planned unbuffered large-file pipeline was deleted: the
  shipped buffered sequential chunk loop — with the OS cache manager's
  read-ahead and write-behind providing the overlap — is the final 1.0
  engine. Unbuffered I/O moves to post-v1 backlog, reopenable only by
  benchmark evidence of a material buffered shortfall.
- Removed the now-meaningless `--no-unbuffered` flag and
  `CopyOptions.no_unbuffered` (and its log-options entry). Same-run
  `--verify` read-back is buffered by design; standalone `bigcp verify` is
  the cold-cache authoritative form (LIMITATIONS.md).
- Remaining 1.0 work is verification and evidence only: the test matrices,
  the elevated ReFS cells, and the operator hardware checklist.

### 2026-07-29 one-time evidence run (owner-approved)

- Recorded bounded benchmark evidence in `BENCHMARKS.md` (raw reports in
  `docs/evidence/2026-07-29/`): small-file 674 files/s vs robocopy /MT:32
  1,500 files/s (KPI not met — coordinator-side stream probe identified as
  the benchmark-backed optimization candidate); large-file buffered
  1,109 MB/s vs robocopy /J ~2,497 MB/s on NVMe→NVMe (ADR 0028 reopening
  condition met on that cell, owner decision pending); perfect extent
  evidence — every preallocated 8 GiB copy landed as a single extent.
- H: external-HDD evidence run aborted with zero writes: the first
  metadata operation failed with a hardware CRC error; all H: activity
  halted per the owner's no-harm instruction.
- Elevated ReFS matrix blocked (unelevated session, no Hyper-V module);
  a one-time VHDX-confined operator script was prepared outside the repo.

### 2026-07-29 investigation and scope decisions (evening)

- H: external drive definitively disqualified: with the drive awake (root
  listing instant), directory creation failed twice with hardware CRC
  errors while zero bytes were written; the owner was alerted to
  investigate the drive.
- Small-file gap investigated to root cause (BENCHMARKS.md): moved the
  coordinator's per-file revalidation and stream probe into the workers
  (rsync-style pipelining; promote-back sentinel returns hidden huge-ADS
  files to the coordinator for checkpointed, cancellable inline streaming)
  and replaced the per-directory drain barrier with a directory-exit drain.
  Single-thread floor improved 57.9→47.9 s; the dominant remainder is
  structural — the atomic-publication protocol costs ~1.6× robocopy's
  per-file work and ~2× its Defender filter evaluations, so PLAN §8.7's H2
  is restated as falsified-with-data and a `FILE_FLAG_DELETE_ON_CLOSE`
  creation candidate is registered benchmark-gated. New e2e test pins the
  promotion round-trip (72 tests total).
- ReFS descoped to best-effort at v1 by owner decision (ADR 0029): code
  paths reviewed, elevated certification matrix moved post-v1, plain-
  language guidance added to LIMITATIONS.md and the README.

### 2026-07-29 reliability-contract redesign (ADR 0030)

- The owner rescoped the reliability bar (VISION amended): the hard
  guarantee is that a completed run's reported successes and failures are
  exactly true; interrupted runs are recovered by re-running, and partial
  files at final names in that window are acceptable. VISION also now
  states the throughput goal: meet or exceed robocopy by default,
  automatically.
- Small files now write directly to their final names via the new
  `DestinationFinal` primitive (in-place truncate for replacements,
  preserving the destination ACL; read-only attributes cleared when known;
  timestamps stamped strictly last as the completion marker). Large files
  keep temp+rename and checkpointed resume. Invariants I3/I5 rescoped; the
  chaos assertion becomes kill-anywhere → rerun converges.
- Large-threshold default raised 4→16 MiB by measurement (direct path
  1.85× faster at 8 MiB; worker buffering bounded ≤1 GiB); further
  decoupling of buffering from destination strategy registered.
- Result: small-file throughput ~0.45× → ~0.8–0.87× robocopy `/MT:32`
  (BENCHMARKS.md); the ≥1×-by-default release gate is now explicit in
  PLAN §8.7 and remains open. README and LIMITATIONS state the new
  contract in plain user language.

### 2026-07-29 phase instrumentation and gap analysis

- Added process-wide per-phase timing accumulators (`core::phase`);
  `--analyze` runs emit a `phase_timing` log event and status line showing
  where worker time went. First capture: destination creation is 72 % of
  all small-file worker time (~2.5 ms/file); the GENERIC_READ-on-create
  hypothesis was tested and falsified (handle is now write-only regardless
  — least access). Analysis, environmental-noise caveat, and ranked
  remaining levers (binary signing first) recorded in BENCHMARKS.md.

### Known pre-1.0 work

- IOCP/no-buffering engine and its sans-I/O model, comprehensive fault/chaos
  harness, elevated filesystem matrix, differential copier, and performance
  evidence remain release gates. See `PLAN_DEVIATIONS.md`.
