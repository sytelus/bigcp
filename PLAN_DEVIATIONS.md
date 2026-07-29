# Plan deviations — open items (updated 2026-07-29, complexity-control pass)

All deviations recorded during implementation have been dispositioned; the 2026-07-29
complexity-control pass then resolved the accumulated open items decisively (full history:
`docs/REVIEW_2026-07-29.md`, ADR 0027, git history of this file). Resolution summary:

- **Built now:** device/disk-full circuit breaker (exit 4), mid-file graceful cancellation,
  ETA display, verify error-vs-mismatch distinction.
- **Deleted from the plan** (recorded inline at each former PLAN section; user-visible
  consequences stated plainly in LIMITATIONS.md): IOCP overlapped ring (the target design is
  now the §5.9 sequential unbuffered pipeline), queue-depth knobs and the runtime governor,
  free-space forecast, Restart Manager lock-owner naming, profiler extras, handle-based ADS
  discovery, deferred-close finalizer pool, per-device scheduler, parallel enumerators,
  decorative TUI widgets, persisted verify-report kind, modeled audit-drain state,
  orphan-scan/retention cleanup, differential-copier release gates.

After the owner clarified that robocopy-`/J` unbuffered semantics were never a VISION
mandate, the unbuffered pipeline (formerly R1) was **deleted too** (ADR 0028): the shipped
buffered sequential engine is the final 1.0 design, and `--no-unbuffered` is gone. **No
product code remains to build for 1.0 — only verification and evidence:**

| # | Item | Lands with |
|---|---|---|
| R1 | Verification matrices: wrapper-boundary fault injection, exhaustive deterministic kill-point simulation, bounded chaos binary + mutator mode, adversarial §12.8 E-case suite, destination sentinel snapshots, emitted-instance schema validation | These are themselves the verification; all bounded and sandbox-confined per §12.0 |
| R2 | Elevated graceful-VHDX ReFS matrix (§12.5) pinning ReFS publication | Requires elevation and an explicit safety-script exemption for sandboxed VHDX operations — **operator permission required** |
| R3 | Real-hardware checklist + bounded performance evidence (§8.7, `[HW]`) with extent-count fragmentation evidence in BENCHMARKS.md; this evidence also arbitrates whether buffered streaming leaves anything material on the table (the only trigger that reopens unbuffered I/O, post-v1) | Operator with designated drives; bounded write budgets; heavy-tier opt-in |

Proposed order: R1 first (the net that catches everything else), then R2/R3 when elevation
and hardware are available.

Maintenance rule: new deviations get an entry here *before* the deviating code merges, and
each entry must name its verdict target — a normative PLAN.md change (whereupon the entry is
removed once the plan text lands) or an open-item row above with its verification.
