# Plan deviations — remaining items with recommended dispositions (2026-07-29)

Every earlier deviation and open item has been resolved: dispositioned into
PLAN.md, built, deleted with a recorded rationale, or executed as evidence
(history: `docs/REVIEW_2026-07-29.md`, ADRs 0027–0031, BENCHMARKS.md, git).
The performance goal — meet or exceed robocopy by default — is **exceeded on
every measured small-file cell and at device-ceiling parity on large-file
cells** (BENCHMARKS.md scoreboard).

What remains is listed below with a **recommended disposition for each**, so
the owner can approve and this registry can empty:

| # | Item | Recommendation |
|---|---|---|
| R1 | Crash/kill verification: bounded chaos harness (`testkit chaos`) + deterministic kill-point coverage asserting kill-anywhere → rerun-converges (oracle-verified) | **Keep — the one remaining engineering block for 1.0.** This is the evidence behind the rerun-repair contract itself; nothing else substitutes. Bounded minutes-scale runs, sandbox-confined (§12.0/§12.4). |
| R2 | Adversarial §12.8 edge cases (E19 aliasing, E33 run-lock race, E34/E36 destination mutation, E40 lost-write orderings, …) | **Keep, slimmed:** implement as directed e2e tests (the §12.2 rule already allows tests over YAML scenarios). Fold into R1's work block. |
| R3 | Full fault-injection site matrix (~40 trait-wrapped sites, 100 % coverage) | **Recommend descope → targeted injection.** Inject the top error classes (device-gone, disk-full, access-denied, sharing) at the wrapper chokepoint to exercise breaker/accounting/resume paths; a per-site completeness matrix is heavy machinery whose marginal classes the taxonomy already routes identically. Record as ADR if approved. |
| R4 | Destination sentinel snapshots + emitted-instance schema validation | **Recommend fold into routine tests:** one e2e asserting a sentinel tree beside the destination is byte-identical after a run; one unit test validating sample emitted events/report against the shipped schemas. Small, then remove. |
| R5 | Certified performance protocol (median of ≥5, quiet machine) for the BENCHMARKS.md scoreboard | **Keep as a release *execution* step, not engineering** — run once before the 1.0 claim; single-session numbers stay labeled indicative until then. |
| R6 | D: (internal NVMe) profiles as Unknown — device-query failure on that controller | **Keep — small defect investigation.** Wrong class only costs defaults (the destination-led composition now masks most impact), but profiling should be trustworthy; diagnose the failing IOCTL and add a fallback classification (e.g. `DeviceSeekPenalty` absent but bus NVMe → Nvme). |
| R7 | Close-finalizer stage (deleted H1, now benchmark-justified: per-file `CloseHandle` ≈ 2.3 ms on write-through USB) | **Recommend post-v1 backlog.** The 32-worker overlap already beats robocopy on the affected cell; the finalizer is a further optimization with its trigger and benchmark recorded in BENCHMARKS.md. |
| R8 | ProcMon-level trace of robocopy on write-through USB (understanding, not a gap — bigcp now leads the cell) | **Recommend remove.** The motivating gap no longer exists. |

If the owner approves the recommendations: R3 descopes (ADR), R4 folds into
tests, R7 moves to the post-v1 backlog, R8 is removed — leaving **R1+R2 (one
engineering block), R5 (one evidence run), and R6 (one small fix)** as the
complete distance to 1.0.

Maintenance rule: new deviations get an entry here *before* the deviating
code merges, and each entry must name its verdict target — a normative
PLAN.md change (whereupon the entry is removed once the plan text lands) or a
row above with its verification.
