# Plan deviations — registry clear (2026-07-29)

Every recorded deviation and open item has now been resolved. Final
dispositions of the last eight (full history: `docs/REVIEW_2026-07-29.md`,
ADRs 0027–0031, BENCHMARKS.md, git):

- **R1, R2, R4, R5** (chaos/kill-convergence harness, adversarial edge-case
  set, sentinel + schema honesty checks, certified median-of-5 performance
  protocol) → moved to **PLAN §12.10: final production validations, executed
  only on explicit owner request; otherwise out of scope**. An initial code
  review of the paths they cover found no critical defects (§12.10 records
  its scope).
- **R3** (exhaustive ~40-site fault-injection matrix) → descoped to targeted
  injection of the top error classes, folded into the §12.10 block.
- **R6** (internal NVMe drives misclassified as Unknown) → **fixed**: the
  Intel VMD/RST controller reports `BusTypeRAID`; classification now trusts
  a positive no-seek-penalty answer as solid-state. Regression-tested;
  confirmed live (BENCHMARKS.md).
- **R7** (close-finalizer stage) → **measured, no advantage** at current
  worker depths (BENCHMARKS.md R7 experiment); stays retired with its
  revisit trigger recorded.
- **R8** (robocopy ProcMon trace) → removed; the gap it would have explained
  no longer exists.

Maintenance rule: new deviations get an entry here *before* the deviating
code merges, and each entry must name its verdict target — a normative
PLAN.md change (whereupon the entry is removed once the plan text lands) or
a row here with its verification.
