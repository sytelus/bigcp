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
  exact-handle delete-on-close ownership; report and journal-compaction
  siblings remain handle-owned through synchronized atomic publication.
- Destination snapshot revalidation before replacement and metadata repair.
- Handle-bound source/destination identity checks for named streams, EAs, and
  post-order directory metadata finalization.
- File, directory, ADS, EA, sparse-data, symlink, junction, rerun, dry-run,
  cancellation, report-fallback, and both verification paths.
- Isolated NTFS/ReFS and FAT/exFAT destination policies: exact strict-volume
  comparison, FAT-family timestamp/attribute projection and file-size limit,
  capability-gated ADS/EA/sparse/EFS/ACL behavior, explicit degradation
  acceptance, and projected verification reporting.
- Exact terminal counter reconciliation and a durably synchronized `run_end`.
- Structurally confined, budgeted tests under a new validated temporary root
  on a whitelisted drive; only the system drive and the code-checkout drive
  are permitted, and prefixes, rooted children, traversal, and every other
  drive are rejected before filesystem access.
- Formatting, warning-free Clippy, unit/integration/doc tests, locked release
  builds, schema parse/version checks (full emitted-instance validation is
  release work), dependency policy, and vulnerability audit.
- Run-owned phase instrumentation, so multiple library runs in one process do
  not contaminate one another's analysis.
- Topology-gated same-spindle scheduling: bounded plain-small read/write phases,
  dense/sparse/ADS burst transport, cancellation accounting, and verified
  same-volume integration coverage without changing the SSD/independent path.
- Isolated UNC/WSL endpoint policy: extended UNC normalization, mapped-drive
  final-path classification, handle-bound remote volume queries, no remote
  local-device IOCTLs, static bounded redirector profiles, WSL exact-name and
  basic-metadata projection, combined startup acceptance, and redirector-loss
  breaker mapping without changing the local transport/profile path.
- Fail-closed Win32/provider parsing for returned lengths, record-local child
  names, sparse-range order, reparse framing, stream suffix containment, and
  embedded-NUL path input.
- Disjoint resolved log/report/state/journal roles, streaming journal replay,
  and exact standard-path concurrent-buffer accounting for `mem` overrides.
- Safe one-component temporary identifiers shared across payload, reparse, and
  state paths, with full-UUID candidate names and no path-based artifact cleanup.

## Release-blocking evidence still required

All within the VISION prohibitions (no large-scale trees, no very-long runs, no
lifespan-reducing writes, no machine-stability impact — see PLAN §12.0):

- Sans-I/O and injected-fault coverage for every completion and Win32 fault
  site, exhaustive **deterministic kill-point simulation**, and bounded
  (minutes-scale) real-process chaos passes.
- **Bounded** workloads (W1s/W2s-class) and topology-matched performance runs
  on scratch-designated storage;
  million-entry behavior via synthetic enumeration simulation, never real trees.
- The same-spindle HDD `[HW]` cell remains required before claiming a measured
  speedup or universal optimality for the new 256 MiB default.
- Generic SMB/mapped-drive source and destination cells plus a WSL destination
  cell remain required before claiming remote matrix certification. A bounded
  WSL-source-to-local smoke supplies correctness evidence only; no network/WSL
  throughput improvement is claimed without separately approved benchmarks.
- The final production-validation pass (PLAN §12.10), executed only on
  explicit owner request: chaos/kill-convergence, the adversarial set,
  sentinel/schema checks, and the certified benchmark protocol.

## Product gap before 1.0

- One directory is currently materialized as a source listing plus destination
  name map. A bounded fallback and its synthetic million-entry validation are
  required before claiming VISION's huge-directory behavior. Routine copying
  remains suitable for ordinary trees, but million-entry directories are not
  certified.

## Additional filesystem-certification evidence

Disposable VHDX coverage for NTFS, ReFS, FAT32, and exFAT uses **graceful
operations only** (create, mount, test, clean dismount of test-owned virtual
disks), including elevated publication, low-space, capability probes,
FAT-family fallbacks, degradation accounting, and projected verification.
Device-loss behavior is validated by fault injection only—never by forced
detach of any kind. ADRs 0029/0035 permit these filesystems to remain explicitly
best-effort and not matrix-certified at v1; this evidence is required before
making a matrix-certification claim, but it is not silently treated as a v1
release pass.

No release process may reinterpret an unrun gate as a pass.
`TESTING_SUMMARY.md` records dated evidence; intentional differences from
the governing plan are recorded inline in PLAN.md and in ADRs 0027–0041.
