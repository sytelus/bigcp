# Testing and safety

## Binding safety rules

Every filesystem test must use a newly created, uniquely named directory under
an explicitly validated scratch root. Test drives are a whitelist, not a
blacklist: only the Windows system drive and the drive holding the code
checkout (derived from the running binary) are permitted, and every other
drive letter — plus every path without one, such as UNC or volume-GUID
paths — is rejected before any filesystem access. The testkit additionally
refuses broad roots, rejects traversal, and rejects roots whose final path
reveals a junction, symlink, mount alias, or SUBST mapping. Existing and
dangling reparse intermediates are rejected. Tests never operate on
source-like user data.

Routine tests must not mount, format, dismount, fill, benchmark, or issue raw
operations to any drive. No cable-removal test is routine. VHDX and hardware
matrices are manual gates and require a separate disposable fixture; they are
never part of the default test command.

Write budgets:

| Suite | Maximum fresh writes per invocation |
|---|---:|
| Unit tests | 4 MiB plus journal truncation fixtures |
| End-to-end contract tests | 32 MiB |
| `testkit gen` routine scenario | Scenario declaration, capped at 1,000 entries and 64 MiB |
| `testkit gen` heavy scenario (`BIGCP_ALLOW_HEAVY_TESTS=1` only) | Absolute caps: 10,000 entries, 1 GiB |
| Routine CI total | 2 GiB ceiling |
| Performance (bounded workloads only) | Low-GB budgets on scratch-designated targets; endurance/TB-class writes are prohibited outright — no approval path exists (VISION) |

## Test tiers: routine by default, heavy only by explicit opt-in

The default suite is the **routine tier**: correctness tests only. They must
run in seconds, create at most a handful of files within the write budgets
below, and be harmless to the drive. Performance measurements, stress tests,
thousands-of-files scenarios, and anything long-running belong to the
**heavy tier**, which never runs by default. Both tiers obey the absolute
VISION prohibitions — no flag unlocks large-scale trees, endurance writes,
forced disconnects, or machine-stability risks.

Heavy tests are disabled by two independent mechanisms, both required:

1. The test function is marked `#[ignore = "heavy: <what it does>"]`, so
   `cargo test` skips it unless `-- --ignored` is passed explicitly.
2. The test (and any generator scenario above 1,000 entries or 64 MiB)
   checks `BIGCP_ALLOW_HEAVY_TESTS=1` and skips or refuses without it. The
   variable must be set by the operator on the command line; repository code
   never sets it, and `check-test-safety.ps1` fails if any Rust source calls
   `set_var`.

```powershell
$env:BIGCP_ALLOW_HEAVY_TESTS = '1'
cmd.exe /d /c scripts\cargo-msvc.cmd test --workspace --all-targets --locked -- --test-threads=1 --ignored
```

**Permission protocol.** Anyone — human or AI agent — making a change that
makes a disabled heavy test worth running must ask the repository owner for
permission before running it, stating exactly: which tests, how many files
and bytes they create, where they write, roughly how long they run, and any
drive-wear or stability impact. Run them only after approval, and record the
run in `TESTING_SUMMARY.md`. Never fold a heavy test into the routine tier
to avoid asking.

## Local test command

Choose a new directory on a whitelisted drive (the snippet uses the system
drive via `LOCALAPPDATA`; the code drive is equally valid), validate its
root, and redirect `TEMP`/`TMP` before running tests:

```powershell
$testRoot = Join-Path $env:LOCALAPPDATA ('Temp\bigcp-tests-' + [guid]::NewGuid().ToString('N'))
$allowed = @("$env:SystemDrive\", [IO.Path]::GetPathRoot((Get-Location).Path))
if ([IO.Path]::GetPathRoot($testRoot) -notin $allowed) { throw 'test root must be on the system or code drive' }
New-Item -ItemType Directory -Path $testRoot -ErrorAction Stop | Out-Null
$env:TEMP = $testRoot
$env:TMP = $testRoot
cmd.exe /d /c scripts\cargo-msvc.cmd test --workspace --all-targets --locked -- --test-threads=1
```

The single test thread is conservative for filesystem timestamp behavior; the
copy engine itself still exercises its bounded workers. Keep the printed test
root in test evidence. Cleanup is optional and may only target that exact newly
created directory after its resolved path and marker are revalidated.

The routine suite includes an explicitly forced-HDD same-volume case (under the
same validated temporary root) that exercises phased small-file batching plus
dense, sparse, and named-stream bursts with verification. It is a correctness
and boundedness test below the normal end-to-end write budget, not a performance
measurement. Real-HDD timing remains the permission-gated `[HW]` matrix cell.

## Static and supply-chain checks

```powershell
cmd.exe /d /c scripts\cargo-msvc.cmd fmt --all -- --check
cmd.exe /d /c scripts\cargo-msvc.cmd clippy --workspace --all-targets --locked -- -D warnings
cmd.exe /d /c scripts\cargo-msvc.cmd test --workspace --all-targets --locked
cargo deny check
cargo audit
```

`cargo-deny` and `cargo-audit` need network access to update their advisory
data. CI runs the locked build and static checks on Windows.

## Testkit

Create the candidate directory yourself, then mark it. `init` deliberately does
not create missing paths.

```powershell
New-Item -ItemType Directory C:\scratch\bigcp-case-001
bigcp-testkit init C:\scratch\bigcp-case-001
bigcp-testkit gen C:\scratch\bigcp-case-001 source testkit\scenarios\e00-smoke.yaml
bigcp C:\scratch\bigcp-case-001\source C:\scratch\bigcp-case-001\destination --plain
bigcp-testkit check C:\scratch\bigcp-case-001 source destination
```

A scenario declares `write_budget_bytes`; generation sums file sizes with
checked arithmetic and refuses any declaration above 1 GiB. Paths are relative
and may not traverse reparse points.

`bigcp-testkit extents <sandbox> <relative-tree>` reports physical extent
counts for a sandboxed tree (read-only, reparse points never followed) — the
fragmentation evidence benchmark entries record per `BENCHMARKS.md`.

Link integration tests create only test-owned links inside the marked sandbox.
They use Developer Mode when available and skip link creation on hosts that do
not authorize the source fixture; they never target an existing file outside
the sandbox.

## Elevated filesystem matrix

FAT32/exFAT and ReFS certification uses only newly created, uniquely named
VHDX files in a separately approved scratch directory. The operator must be
an administrator and must validate the VHD path and the mounted disk identity
before initializing or formatting it. Existing disks, partitions, drive
letters, and user data are never matrix targets. Each virtual disk is mounted,
tested, cleanly dismounted, and then the test-owned VHDX may be removed.

The FAT-family cells cover acceptance bypass, direct-small and transactional
large paths, timestamp comparison boundaries, `READONLY`/`HIDDEN`/`SYSTEM`/
`ARCHIVE`, ADS/EA drops, dense sparse expansion, link rejection, FAT's
4,294,967,295-byte limit (primarily through a synthetic boundary test), rerun
convergence, projected verification, and the identity/enumeration/rename
fallbacks selected by the mounted driver. This matrix is not routine CI and
must be recorded in `TESTING_SUMMARY.md`; absence of that evidence means the
filesystem is implemented but not matrix-certified.

## Adding a test

1. State its fresh-write ceiling in the test or scenario.
2. Obtain the base with `validated_system_temp()` before creating anything.
3. Create a unique child, initialize its marker, and resolve every path through
   `SandboxRoot::child`.
4. Snapshot or oracle-check the intended tree; assert no opaque temps remain
   after success.
5. Do not reference any drive outside the system/code whitelist, physical
   device paths, volume GUID paths, `diskpart`, `format`, or destructive
   storage commands.
6. Keep it in the routine tier: correctness-focused, seconds to run, few
   files. If it measures performance, stresses, or needs many files or much
   time, mark it `#[ignore = "heavy: …"]`, gate it on
   `BIGCP_ALLOW_HEAVY_TESTS=1`, and follow the permission protocol above.
7. Run fmt, clippy, the focused test, and the whole suite.

## Registered next tests (safe, sandboxed, small — add in this order)

Identified by the 2026-07-29 review as the highest-value coverage within the
VISION guidance, none requiring scale, hardware, or elevation: E19 root
aliasing pre-flight; E20 locked destination; E12 junction copied-not-recursed; E13/E35 hard
links; E04/E38 long paths; E05 reserved/trailing-dot names; E06 Unicode
NFC/NFD + unpaired surrogates; E18 stale journal after destination deletion;
E33 run-lock refusal at the run level.

## Suites not claimed by this initial implementation

Fault-site injection, exhaustive deterministic kill-point simulation plus
bounded real-process chaos passes, emitted-instance schema validation,
synthetic-enumeration scale simulation, and real-hardware throughput gates
within bounded write budgets require dedicated work and hardware. They are
release-blocking before a 1.0 claim and run only on explicit owner request;
the elevated NTFS/ReFS/FAT32/exFAT VHDX matrix and differential OS-copy
comparisons are post-v1/matrix-certification evidence under ADRs 0029/0035.
See PLAN §12.10, §13.2, and `BENCHMARKS.md`.
Hours-long
soaks, million-entry real trees, and forced-disconnect tests are not deferred —
they are prohibited (VISION) and will never run.
