# Working on bigcp

Entry point for any maintainer — human engineer or AI agent. This file only
points; the linked documents are authoritative.

## What this project is

`bigcp` is a reliability-first, high-throughput tree copier for Windows 11.
The owner's immutable specification is [VISION.md](VISION.md) — **never edit
it**. [PLAN.md](PLAN.md) is the governing engineering record (§13.1 defines
the human-or-AI execution model). The reliability promise dominates every
other concern: a completed run's reported successes and failures must be
exactly true, the source is strictly read-only, and destination-only files
are never deleted.

## Build and test

Rust 1.97.1 (MSVC), VS 2022 Build Tools (x64 C++), Windows 10/11 SDK. Use the
repository launcher — it builds the exact MSVC environment without evaluating
interactive `cmd.exe` AutoRun hooks:

```powershell
# cmd.exe form (README) or PowerShell form; both take ordinary cargo args
cmd.exe /d /c scripts\cargo-msvc.cmd build --release --locked
powershell -File scripts\cargo-msvc.ps1 test --workspace --locked
powershell -File scripts\cargo-msvc.ps1 clippy --workspace --all-targets --locked
```

CI also runs `cargo fmt --all -- --check`, `cargo-deny check`,
`scripts/check-test-safety.ps1`, and `scripts/check-frozen-inputs.ps1` on
every push. All of them must stay green; clippy pedantic plus
`unwrap_used`/`expect_used`/`panic` are denied workspace-wide.

## Non-negotiable rules

- **Test safety (PLAN §12.0, VISION line 43, [docs/TESTING.md](docs/TESTING.md)):**
  every test writes only inside a fresh sandbox under a whitelisted drive
  (system drive or the code checkout drive), with tiny data. Never add tests
  that create huge file counts, stress drives, delete outside their sandbox,
  or destabilize the machine. `scripts/check-test-safety.ps1` is a backstop,
  not the rule.
- **Frozen inputs:** `PLAN.md`, `VISION.md`, and `LIMITATIONS.md` are
  SHA-256-pinned by `scripts/check-frozen-inputs.ps1`. Editing PLAN.md or
  LIMITATIONS.md requires explicit owner approval; the re-pin (updating the
  hash in that script, with a dated comment explaining why) is the final step
  of the approved change. VISION.md is never edited.
- **Unsafe code lives only in `crates/win`** (every `unsafe` block carries a
  `// SAFETY:` comment discharging its obligations); `bigcp-core` and above
  deny `unsafe_code`.
- **One copy engine.** Alternative engines exist only inside the test
  harness's oracle, never as product features (ADR 0043).

## Code map (details in [docs/MAINTENANCE.md](docs/MAINTENANCE.md))

| Crate | Role |
|---|---|
| `crates/win` | Narrow safe Win32 wrappers; the only unsafe boundary |
| `crates/core` | Copy semantics, scheduling, journal/resume, audit, verify, report |
| `crates/tui` | Live dashboard, plain progress stream, report browser |
| `crates/cli` | Argument surface, preflight prompt, exit-code policy |
| `crates/testkit` | Sandbox, bounded fixture generator, independent oracle |

Path/modality seams (NTFS vs FAT vs UNC vs WSL, same-spindle vs distinct
drives) are deliberately isolated; [docs/DESIGN.md](docs/DESIGN.md)
"Extension seams" names the owning file for each axis. Do not spread
endpoint- or filesystem-specific behavior outside its seam.

## Which document to update for which change

| Change | Update |
|---|---|
| User-visible behavior, flags, exit codes | `README.md` (+ `CHANGELOG.md`) |
| The copy contract (§4 semantics) | ADR + `docs/SEMANTICS.md` + `LIMITATIONS.md` (owner-approved) + schema review |
| Architecture / module seams | `docs/DESIGN.md` + ADR if a decision changed |
| Error categories or hints (`crates/core/src/error.rs`) | `docs/ERRORS.md` in the same commit (hand-maintained mirror) |
| On-disk formats (journal/log/report), invariants, default tuning | ADR (`docs/adr/`, append-only; index in `docs/adr/README.md`) |
| Tests and their write budgets | `docs/TESTING.md`, `TESTING_SUMMARY.md` |
| Performance claims | `BENCHMARKS.md` (dated, with methodology) |
| Release-gate status | `docs/PRODUCTION_READINESS.md` (the single live gate list) |

PLAN §14.6's bar applies to all of it: a fresh implementer with no prior
context must be able to proceed without asking anyone questions.
