# Plan deviations — reviewed dispositions (2026-07-29)

Every deviation recorded during implementation was reviewed against VISION.md, PLAN.md, and
LIMITATIONS.md. Verdicts used below:

- **Accepted (normative)** — the deviation is the better design; PLAN.md/LIMITATIONS.md were
  updated so the plan now *specifies* the as-built behavior. The entry here is historical record.
- **Accepted (release work)** — the deferral is correct under the reliability-first rule
  (unverified complexity is worse than absent complexity); the item is tracked in PLAN.md §13.2
  and may only ship together with its specified verification.

None of the deviations violates the VISION testing guidance; several exist *because of* it.

| # | Deviation (summary) | Disposition |
|---|---|---|
| 1 | I5 metadata ordering: final name appears after all *data* complete; timestamps/attrs applied immediately after rename (tunneling) | **Accepted (normative)** — I5 restated in PLAN §7.1; §4.3 was already the normative sequence. The post-rename metadata window is recoverable (reclassified Different). |
| 2 | `--dry-run` does not create a missing destination root | **Accepted (normative)** — zero-destination-write outranks pinning; PLAN §4.5 (E42) now carries the explicit dry-run exception. |
| 3 | Large-file path is bounded buffered synchronous streaming behind the transport port; IOCP unbuffered ring deferred | **Accepted (release work)** — PLAN §13.2. Semantics (chunking, sparse ranges, offset-ordered hashing, exact checkpoints, promotion, finalizer) are transport-independent by design; `--no-unbuffered` inert until then; no §8 performance claim is made. |
| 4 | ADS discovery via path-based `FindFirstStreamW` + identity revalidation instead of handle-based `FileStreamInfo` parse | **Accepted (release work)** — PLAN §13.2. The handle-based parser requires bounded unsafe parsing plus malformed-record tests; shipping it untested would be worse. F21's zero-extra-open target noted as not yet met. |
| 5 | Checkpoint boundaries are deterministic size tiers (256 MiB → 4 GiB), not throughput-scaled | **Accepted (normative)** — determinism beats pseudo-adaptation on a synchronous transport; PLAN §5.9 updated. Revisit only with the IOCP transport. |
| 6 | QD/stream profile fields validated and reported but not active concurrency controls | **Accepted (release work)** — coupled to deviation 3; UI already avoids claiming device utilization. PLAN §13.2. |
| 7 | Coordinator-driven enumeration/copy; small-file workers close their own files (no finalizer pool) | **Accepted (release work)** — the plan's own benchmark gate (§13) forbids landing these optimizations without isolated benchmarks; the atomic finalizer remains the only publication path, so transports can change without semantic rewrite. |
| 8 | Governor, Restart Manager naming, device/space breakers, free-space forecast, beyond-target directory fallback deferred | **Accepted (release work)** — none affects semantic correctness; failures stay per-object and audited. PLAN §13.2. |
| 9 | Profiler supplies topology/bus/seek/MTL/alignment but not vendor strings, hotplug inference, cache state, RAM discovery | **Accepted (release work)** — unused facts are not guessed; confidence falls back conservatively. PLAN §13.2. |
| 10 | New-ADS open requires clearing delete-on-close for one syscall (micro-window can strand one opaque temp on kill) | **Accepted (normative)** — a Windows constraint, handled minimally; PLAN §4.3 temp-lifecycle bullet documents the window. Never a partial final name, never touched old data. |
| 11 | ReFS publication implemented but not certified by the elevated ReFS matrix | **Accepted (release work)** — PLAN §13.2/§5.2; a capability-gated legacy rename fallback is added there if the probe demands it. |
| 12 | Post-copy `--verify` covers files (all streams/EAs/attrs/times) but not run-local dirs/reparse; both verify forms use buffered reads | **Accepted (normative + release work)** — PLAN §5.17 narrowed: run-local scope is files-only by design (dirs/reparse belong to the standalone form, keeping one object-verifier per form). Buffered read-back is documented honestly (LIMITATIONS.md); unbuffered read-back lands with the transport (§13.2). |
| 13 | Standalone verify prints/returns full summary but has no persisted report kind; `VerifyOptions.report_path` reserved | **Accepted (release work)** — forcing verify results into copy-run report fields would fabricate counters; a verification-run report kind is specified for release. |
| 14 | Destination-only extra ADS on surviving objects preserved and reported as divergence, never deleted | **Accepted (normative)** — this is the no-delete contract (I2) applied to streams; PLAN §5.17 matrix row updated. |
| 15 | Compact truthful TUI instead of full §11 design | **Accepted (release work)** — an ETA/sparkline surface before real scheduler telemetry would be decorative fiction. PLAN §13.2. |
| 16 | Unrecoverable audit failure aborts immediately (no modeled drain state); orphan-scan/retention cleanup unimplemented | **Accepted (release work)** — aborting is the conservative direction; drain refinement and cleanup need deterministic log-device fault injection first. PLAN §13.2. |
| 17 | No parallel `tracing`-span stack; JSONL is the sole audit narrative | **Accepted (normative)** — one complete audit narrative beats two competing ones; PLAN §14.3 updated. Spans may return only with proven zero-cost-when-disabled. |
| 18 | I10 enforced by core call graph + capability types + tests, not inside `win` write constructors | **Accepted (normative)** — the platform crate legitimately serves the testkit; PLAN §7.1 I10 row updated with the future destination-capability note. |
| 19 | Full test/release matrix documented but not executed on this machine | **Accepted (release work, partially re-scoped)** — and the 2026-07-29 VISION hardening *permanently* re-scoped several gates: million-entry tests are simulation-only, hours-long chaos soaks are replaced by exhaustive deterministic kill-point simulation plus bounded real-process passes, and forced-disconnect testing (physical or VHDX surprise-detach) is prohibited outright. PLAN §12.0/§12.4/§12.5/§12.9 now carry the bounded scopes. |
| 20 | Competitor multipliers treated as aspirational, not release facts | **Accepted (normative)** — already PLAN §8.7's two-tier design; KPIs now measured on bounded workloads only. |
| 21 | Testkit lacks `oscopy`, `chaos`, per-E-case scenarios, destination sentinel snapshot | **Accepted (release work)** — PLAN §13.2; the shipped generator/oracle/sandbox are the correctness-critical parts and exist. |

Maintenance rule: new deviations get an entry *before* the deviating code merges, and each entry
must name its verdict target (normative plan change, or §13.2 release-work registration).
