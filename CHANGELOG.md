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

### Known pre-1.0 work

- IOCP/no-buffering engine and its sans-I/O model, comprehensive fault/chaos
  harness, elevated filesystem matrix, differential copier, and performance
  evidence remain release gates. See `PLAN_DEVIATIONS.md`.
