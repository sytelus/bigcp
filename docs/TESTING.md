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

Generator entry caps count every distinct implicit parent directory that
`create_dir_all` can materialize, not only paths explicitly listed in a
scenario. Exact duplicate files, file/directory path conflicts, invalid
relative paths, and over-depth plans are rejected before the scenario root is
created.

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

## Optional elevated filesystem compatibility matrix

Optional FAT32/exFAT and ReFS compatibility exercises use only newly created,
uniquely named VHDX files in a separately approved scratch directory. The
operator must be an administrator and must validate the VHD path and the
mounted disk identity before initializing or formatting it. Existing disks,
partitions, drive letters, and user data are never targets. Each virtual disk
is mounted, tested, cleanly dismounted, and then the test-owned VHDX may be
removed.

The FAT-family cells cover acceptance bypass, direct-small and transactional
large paths, timestamp comparison boundaries, `READONLY`/`HIDDEN`/`SYSTEM`/
`ARCHIVE`, ADS/EA drops, dense sparse expansion, link rejection, FAT's
4,294,967,295-byte limit (primarily through a synthetic boundary test), rerun
convergence, projected verification, and the identity/enumeration/rename
fallbacks selected by the mounted driver. This matrix is not routine CI; when
run, it must be recorded in `TESTING_SUMMARY.md`. ReFS/FAT/exFAT are
best-effort regardless of coverage, so missing cells are not release blockers
and passing cells do not certify those filesystems (ADR 0042).

## Optional remote endpoint compatibility matrix

Routine `cargo test` and `bigcp-testkit` remain local-only by construction; UNC
support does not weaken the sandbox whitelist above. Pure tests cover ordinary
and extended UNC normalization, WSL alias canonicalization, local-vs-UNC
classification, mapped-drive effective policy, exact WSL name keys, projected
metadata, remote profiles, redirector error categories, two-buffer ordering and
actual stage overlap, short-I/O accounting, cancellation, memory budgets, and
the checkpoint/sparse worker-dispatch boundary. WSL-specific pure tests also
pin its distinct transport/audit value, independent 8 MiB/32-worker row,
either-side small-file striping without local-affinity regression, the
segmented-transfer planner and its identity-proven segment writers (local
files under a WSL transport profile — no real endpoint needed), sequential
handle path, and deferred final stamp (ADRs 0045/0046/0052).

Live generic-SMB, mapped-drive, or WSL source/destination tests need an
operator-approved scratch endpoint whose exact share/distribution path is named
in advance. They may create only a unique test-owned subtree, must use the same
small routine write budget unless separately approved as heavy, and must never
reuse an existing user-data tree. A read-only WSL source dry-run may establish
provider query/enumeration compatibility but does not establish write-path or
performance coverage. Remote performance measurements are heavy-tier and
require the full approval record (paths, files, bytes, duration, and storage
impact). An ADR 0045 generic-UNC run must also record SMB provider, link
topology, signing/encryption/compression state, bigcp transport/chunk/workers,
verification mode, and the exact competitor command. An ADR 0046 WSL run must
record `wsl --version`, WSL 1 versus 2, distribution/version, source and
destination filesystem/path direction, VHDX backing location when known,
transport/chunk/workers, verification mode, cold/warm state, and the exact
native-Linux/robocopy comparison commands. Never stop or restart WSL merely to
manufacture a cold measurement.

Disconnect behavior is fault-injection-only: do not stop WSL, disconnect a
share, remove a mapping, or manipulate a server during a test. The approved
matrix covers direct/extended/legacy aliases, `--accept-remote-paths`, projected
metadata, WSL exact-case names, unsupported link failure, rerun, and both
verification forms. Record every executed cell in `TESTING_SUMMARY.md`.
Missing cells are provider-specific evidence gaps, not release blockers or
certification debt.

## Adding a test

1. Pure parser/policy tests should not touch the filesystem. For a mutating
   test, state its fresh-write ceiling in the test or scenario.
2. Integration and testkit tests obtain the base with
   `validated_system_temp()`, create a unique marked child, and resolve every
   path through `SandboxRoot::child`.
3. A crate-local Win32 wrapper test cannot depend on `bigcp-testkit` (that
   would create a dependency cycle). It may use `tempfile::TempDir`, but every
   path must derive from that owned directory. CI redirects `TEMP` and `TMP`
   to its unique validated C: run root before executing any test.
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
release-blocking before a 1.0 claim and run only on explicit owner request.

By owner decision, NTFS is the only filesystem certification target. Approved
ReFS/FAT32/exFAT VHDX exercises and generic-UNC/mapped-drive/WSL endpoint
exercises are optional compatibility evidence under ADR 0042; they are not
silently promoted into the v1 gate and cannot certify those best-effort paths.
Differential OS-copy comparisons likewise remain optional. See PLAN §12.10,
§13.2, and `BENCHMARKS.md`.

ADR 0048's distinct-drive NTFS relative-create speed hypothesis remains a
hardware gate, not a routine test. An approved run must name both disposable
NTFS roots and their physical disks, cap files/bytes/duration, use the
quiesced and rotated repetition protocol, compare an otherwise-identical
absolute-open baseline plus robocopy, and report directory shapes as well as
logical/physical writes. Correctness-only temporary-directory tests do not
authorize or substitute for that measurement.

Hours-long soaks, million-entry real trees, and forced-disconnect tests are not
deferred: they are prohibited by VISION and will never run.
