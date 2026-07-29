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
| R2 | Real-hardware external-drive evidence (§8.7, `[HW]`) — internal-drive cells recorded 2026-07-29; the external cell needs a healthy drive (H: disqualified by hardware CRC errors) | Operator with a designated healthy drive; bounded write budgets |

(The former elevated ReFS matrix left this registry by owner decision — ReFS is best-effort
at v1, ADR 0029, matrix post-v1. The unbuffered question is likewise parked post-v1 with
its trigger recorded in BENCHMARKS.md.)

Proposed order: R1 (the remaining engineering), then R2 when hardware is available.

Status 2026-07-29 (owner-approved one-time evidence run): the internal-drive share of R3's
bounded performance evidence is recorded in `BENCHMARKS.md` (single-run, indicative — the
certified repeated-run protocol remains open) with two honest findings registered there: the
small-file coordinator bottleneck (benchmark-backed optimization candidate) and the NVMe
buffered-vs-unbuffered gap (ADR 0028's reopening condition met on that cell — owner
decision pending). R3's external-HDD cell aborted with zero writes on a hardware CRC error
(H: needs owner investigation), and R2 awaits an elevated session with the Hyper-V module
(operator script prepared).

Maintenance rule: new deviations get an entry here *before* the deviating code merges, and
each entry must name its verdict target — a normative PLAN.md change (whereupon the entry is
removed once the plan text lands) or an open-item row above with its verification.
