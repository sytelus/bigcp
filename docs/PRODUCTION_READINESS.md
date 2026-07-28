# Production readiness

`bigcp` is an operational pre-1.0 implementation, not a certified 1.0 release.
This distinction is intentional: routine tests provide strong evidence for the
implemented semantic core without substituting a developer workstation or an
existing drive for the disposable fault and endurance infrastructure required
by the frozen plan.

## Implemented and routinely gated

- Read-only source primitives and non-following link handling.
- Uniform opaque-temp plus atomic-rename publication for files and links.
- Destination snapshot revalidation before replacement and metadata repair.
- File, directory, ADS, EA, sparse-data, symlink, junction, rerun, dry-run,
  cancellation, report-fallback, and both verification paths.
- Exact terminal counter reconciliation and a durably synchronized `run_end`.
- Structurally confined, budgeted tests under a new validated C: temporary root;
  F:, G:, and H: are rejected before filesystem access.
- Formatting, warning-free Clippy, unit/integration/doc tests, locked release
  builds, schema checks, dependency policy, and vulnerability audit.

## Release-blocking evidence still required

- Sans-I/O and injected-fault coverage for every completion and Win32 fault
  site, followed by 30-minute and eight-hour chaos runs.
- Disposable VHDX coverage for NTFS and ReFS, including elevated publication,
  crash recovery, low-space, and filesystem-specific capability probes.
- Million-entry, 20 GiB, differential OS-copy, and topology-matched performance
  suites on explicitly designated scratch storage.
- Real hardware/controller coverage and the remaining scheduler, breaker,
  restart-manager, and full TUI work listed in `PLAN_DEVIATIONS.md`.

No release process may reinterpret an unrun gate as a pass. `TESTING_SUMMARY.md`
records dated evidence; `PLAN_DEVIATIONS.md` is the authoritative list of known
differences from the frozen plan.
