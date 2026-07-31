# Production readiness

`bigcp` is an operational pre-1.0 implementation, not a certified 1.0 release.
This distinction is intentional: routine tests provide strong evidence for the
implemented semantic core without substituting a developer workstation or an
existing drive for the disposable fault and endurance infrastructure required
by the governing plan.

## Implemented and routinely gated

- Read-only source primitives and non-following link handling.
- Direct, rerun-recoverable publication for plain small files; opaque-temp plus
  atomic-rename publication for ADS/EA, sparse, large, and reparse objects.
- Checkpoint source/temp identity binding, non-following resume opens, and
  exact-handle delete-on-close ownership.
- Destination snapshot revalidation before replacement and metadata repair.
- Handle-bound source/destination identity checks for named streams, EAs, and
  post-order directory metadata finalization.
- File, directory, ADS, EA, sparse-data, symlink, junction, rerun, dry-run,
  cancellation, report-fallback, and both verification paths.
- Exact terminal counter reconciliation and a durably synchronized `run_end`.
- Structurally confined, budgeted tests under a new validated temporary root
  on a whitelisted drive; only the system drive and the code-checkout drive
  are permitted, and everything else is rejected before filesystem access.
- Formatting, warning-free Clippy, unit/integration/doc tests, locked release
  builds, schema parse/version checks (full emitted-instance validation is
  release work), dependency policy, and vulnerability audit.
- Run-owned phase instrumentation, so multiple library runs in one process do
  not contaminate one another's analysis.

## Release-blocking evidence still required

All within the VISION prohibitions (no large-scale trees, no very-long runs, no
lifespan-reducing writes, no machine-stability impact — see PLAN §12.0):

- Sans-I/O and injected-fault coverage for every completion and Win32 fault
  site, exhaustive **deterministic kill-point simulation**, and bounded
  (minutes-scale) real-process chaos passes.
- Disposable VHDX coverage for NTFS and ReFS using **graceful operations only**
  (create, mount, test, clean dismount of test-owned virtual disks), including
  elevated publication, low-space, and capability probes. Device-loss behavior
  is validated by fault injection only — never by forced detach of any kind.
- **Bounded** workloads (W1s/W2s-class), differential OS-copy comparison, and
  topology-matched performance runs on scratch-designated storage;
  million-entry behavior via synthetic enumeration simulation, never real trees.
- The final production-validation pass (PLAN §12.10), executed only on
  explicit owner request: chaos/kill-convergence, the adversarial set,
  sentinel/schema checks, and the certified benchmark protocol.

## Product gap before 1.0

- One directory is currently materialized as a source listing plus destination
  name map. A bounded fallback and its synthetic million-entry validation are
  required before claiming VISION's huge-directory behavior. Routine copying
  remains suitable for ordinary trees, but million-entry directories are not
  certified.

No release process may reinterpret an unrun gate as a pass.
`TESTING_SUMMARY.md` records dated evidence; intentional differences from
the governing plan are recorded inline in PLAN.md and in ADRs 0027–0034.
