# Testing and safety

## Binding safety rules

Every filesystem test must use a newly created, uniquely named directory under
an explicitly validated scratch root. The testkit refuses F:, G:, and H: before
access, refuses broad roots, rejects traversal, and rejects roots whose final
path reveals a junction, symlink, mount alias, or SUBST mapping. Existing and
dangling reparse intermediates are rejected. Tests never operate on source-like
user data.

Routine tests must not mount, format, dismount, fill, benchmark, or issue raw
operations to any drive. No cable-removal test is routine. VHDX and hardware
matrices are manual future gates and require a separate disposable fixture;
they were not run during initial implementation.

Write budgets:

| Suite | Maximum fresh writes per invocation |
|---|---:|
| Unit tests | 4 MiB plus journal truncation fixtures |
| End-to-end contract tests | 32 MiB |
| `testkit gen` ordinary scenario | Scenario declaration, hard-capped at 1 GiB |
| Routine CI total | 2 GiB ceiling |
| Performance/endurance | Disabled by default; separate written approval and scratch hardware required |

## Local test command

Choose a new C: directory, validate its drive root, and redirect `TEMP`/`TMP`
before running tests:

```powershell
$testRoot = Join-Path $env:LOCALAPPDATA ('Temp\bigcp-tests-' + [guid]::NewGuid().ToString('N'))
if ([IO.Path]::GetPathRoot($testRoot) -ne 'C:\') { throw 'test root must be C:' }
New-Item -ItemType Directory -Path $testRoot -ErrorAction Stop | Out-Null
$env:TEMP = $testRoot
$env:TMP = $testRoot
cmd.exe /d /c scripts\cargo-msvc.cmd test --workspace --all-targets -- --test-threads=1
```

The single test thread is conservative for filesystem timestamp behavior; the
copy engine itself still exercises its bounded workers. Keep the printed test
root in test evidence. Cleanup is optional and may only target that exact newly
created directory after its resolved path and marker are revalidated.

## Static and supply-chain checks

```powershell
cmd.exe /d /c scripts\cargo-msvc.cmd fmt --all -- --check
cmd.exe /d /c scripts\cargo-msvc.cmd clippy --workspace --all-targets -- -D warnings
cmd.exe /d /c scripts\cargo-msvc.cmd test --workspace --all-targets
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

Link integration tests create only test-owned links inside the marked sandbox.
They use Developer Mode when available and skip link creation on hosts that do
not authorize the source fixture; they never target an existing file outside
the sandbox.

## Adding a test

1. State its fresh-write ceiling in the test or scenario.
2. Obtain the base with `validated_system_temp()` before creating anything.
3. Create a unique child, initialize its marker, and resolve every path through
   `SandboxRoot::child`.
4. Snapshot or oracle-check the intended tree; assert no opaque temps remain
   after success.
5. Do not reference F:, G:, H:, physical device paths, volume GUID paths,
   `diskpart`, `format`, or destructive storage commands.
6. Run fmt, clippy, the focused test, and the whole suite.

## Suites not claimed by this initial implementation

Full IOCP adversarial completion modeling, fault-site injection, chaos nights,
elevated VHDX/ReFS cells, CopyFile2/robocopy differential runs, million-entry
performance workloads, and real-hardware throughput gates require dedicated
work and hardware. They are explicitly release-blocking before a 1.0 claim;
see `PLAN_DEVIATIONS.md` and `BENCHMARKS.md`.
