# Implementation testing and drive-safety summary

Latest review: 2026-07-31

## 2026-07-31 WSL Plan 9 throughput specialization

WSL now has a distinct `wsl` transport/profile identity while reusing the
correctness-tested ordered two-buffer engine. Its independently owned Auto row
uses 8 MiB requests and up to 16 workers. A WSL destination stripes plain-small
creates across the bounded worker pool, opens direct/temp/resumed handles with
the sequential cache hint, skips the redundant metadata query after a
successful `CREATE_NEW`, and applies projected last-write time only in the
authoritative post-write stamp. Generic UNC keeps its `redirector` identity and
directory affinity; local standard and same-spindle policy are unchanged.

Focused tests pin WSL profile selection, stable `"wsl"` audit serialization,
WSL-destination striping versus local/generic-UNC affinity, and deferred final
stamping after sequential writes. All 147 confined workspace tests passed with
zero failures: 8 CLI, 60 core unit, 16 core end-to-end, 11 testkit safety, 3
TUI, and 49 Windows-boundary tests. Formatting, workspace/all-target
warning-denied Clippy, warning-denied rustdoc, doc tests, the locked release
build, and test-storage safety checks passed. Cargo-deny reported only the
allowed duplicate-version warnings for `hashbrown` and `syn`; RustSec found no
vulnerability among 146 locked dependencies. PLAN, LIMITATIONS, schemas,
operator/maintainer docs, changelog, BENCHMARKS, and ADR 0046 distinguish the
correctness-tested mechanisms from the unmeasured H7 speed hypothesis.
`VISION.md` was not modified and remains at SHA-256
`B970F59B791A53584FB57698B26CB70A7E7E9D80982B9118F4EF5A4199BE6C28`.

No WSL/UNC write, distribution restart, physical-drive, VHDX, performance,
endurance, large-scale, or forced-disconnect test ran. A live speed claim still
requires an exact operator-approved disposable WSL path, file/byte budget,
duration, and storage-impact record under `docs/TESTING.md`.

## 2026-07-31 bounded UNC/WSL redirector throughput refactor

In this earlier pass, UNC, mapped-drive, and WSL profiles selected one isolated
`redirector` transport; ADR 0046 above later splits WSL policy from generic
UNC. Streamed dense data, sparse allocated ranges, and named streams use
two fallibly allocated buffers: a scoped reader fills the next request while
the caller writes the prior request, with hashing and writes still strictly
ordered and every pipeline segment capped at a checkpoint boundary.
Independent non-sparse files that do not require persistent checkpoints can use
the existing bounded worker pool; round-robin sharding lets several streamed
files in one directory occupy separate workers. Checkpoint-eligible or sparse
work remains coordinator-owned, and worker transfers share graceful user and
breaker cancellation. `mem=` now accounts for two chunks per active
redirector stream. Local standard and same-spindle selection are unchanged.

Nine focused pipeline tests prove ordered bytes under forced short reads and
writes, interrupted-write retry, stage-specific I/O failures, actual
read-ahead/write overlap, short-source and actual-I/O accounting, shared
cancellation, bounded reader-panic handling, and existing phased-buffer
behavior. Two dispatch tests pin remote-only parallel streaming and local
small-file-only behavior; profile tests pin redirector selection and exact
two-buffer memory caps. During integration validation, removing an unused
same-spindle scratch allocation exposed one trailing sparse-hole loop that had
still used the scratch length for progress. The exact sparse/ADS/same-spindle
test caught the resulting stall; the loop now uses the configured chunk and the
test completes normally.

All 144 confined workspace tests passed with zero failures: 8 CLI, 58 core
unit, 16 core end-to-end, 11 testkit safety, 3 TUI, and 48 Windows-boundary
tests. Formatting, workspace/all-target warning-denied Clippy, warning-denied
rustdoc, doc tests, the locked release build, governing-input and test-storage
safety checks, cargo-deny, and cargo-audit passed. Cargo-deny reported only the
allowed duplicate-version warnings for `hashbrown` and `syn`; RustSec found no
vulnerability among 146 locked dependencies. PLAN, LIMITATIONS, schemas,
operator/maintainer docs, changelog, BENCHMARKS, and ADR 0045 now distinguish
the correctness-tested implementation from the unmeasured H6 speed hypothesis.
`VISION.md` was not modified and remains at SHA-256
`B970F59B791A53584FB57698B26CB70A7E7E9D80982B9118F4EF5A4199BE6C28`.

No UNC/WSL write, physical-drive, VHDX, performance, endurance, large-scale, or
forced-disconnect test ran. A live speed claim still requires an exact
operator-approved disposable remote path, file/byte budget, duration, and
storage-impact record under `docs/TESTING.md`.

## 2026-07-31 output, handle-completion, and test-cap review

This whole-repository pass fixed three concrete safety/correctness gaps.
Line-oriented progress, messages, final summaries, standalone verification,
preflight warnings, and prompts now use fallible writes: a closed redirected
output stream can no longer panic-abort an active release copy, and command
output failures map to the documented invariant/output exit. Direct
destination files now close explicitly before their engine result can report
success, so a `CloseHandle` failure is observable just as it is for the
transactional path. Finally, testkit planning counts all distinct implicit
parent directories before creating a scenario root; deep file-only manifests
can no longer bypass the VISION entry caps. Duplicate declarations and exact
file/directory path conflicts also fail during that no-write preflight.

Two new testkit regressions cover implicit-directory cap enforcement and
file/directory conflicts. All 134 bounded workspace tests passed with zero
failures: 8 CLI, 48 core unit, 16 core end-to-end, 11 testkit safety, 3 TUI,
and 48 Windows-boundary tests. The serial run used this newly created
system-temporary root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-final-70ba984e59404491bad8ee15d38a7925`

Formatting, warning-denied workspace/all-target Clippy, warning-denied
rustdoc, doc tests, the locked optimized workspace build, frozen-input and
test-storage safety checks, Markdown local-link validation, cargo-deny, and
cargo-audit passed. Cargo-deny reported only the allowed duplicate-version
warnings for `hashbrown` and `syn`; RustSec found no vulnerability among 146
locked dependencies. CI now actually sets `RUSTDOCFLAGS=-D warnings`, matching
the repository's documented gate. PLAN and LIMITATIONS now describe immediate
audit-failure termination consistently, and stale two-engine wording was
removed. `VISION.md` was not modified.

The release binary copied and same-run-verified two files (13 bytes), passed a
full standalone comparison (4 objects, 0 mismatches), and reopened its plain
report under:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-smoke-2eef4c1fbe9d47d9b17039aba9de66e6`

No heavy, physical-drive, remote-write, VHDX, performance, stress,
large-scale, endurance, or forced-disconnect test ran.

## 2026-07-31 certification-boundary and failure-reporting review

The latest whole-repository review fixed two concrete error-path defects. Raw
Win32 failures now classify the complete official `ERROR_CLOUD_FILE_*` family
as `cloud`, rather than recognizing only provider-not-running. Direct
replacement no longer discards a failure to restore READONLY metadata after a
failed create retry: `restore_dst_metadata` becomes the primary reported
operation, retains the rollback error category/code, and includes both failure
contexts. Three focused unit tests cover the official constant family,
representative public classification, identity-change rollback, and ordinary
rollback I/O failure.

Active documentation now consistently describes one product copy engine with
direct and transactional completion strategies plus standard and same-spindle
transports. By owner decision, NTFS is the sole filesystem-certification
target; ReFS, FAT/FAT32, exFAT, generic UNC/provider filesystems, and WSL remain
supported best-effort. Their bounded VHDX/remote exercises are optional
compatibility evidence, not release gates or certification debt. ADRs
0042–0044 record these decisions and the rollback invariant. Stale references
to required endurance infrastructure and soak runs were removed; the VISION
prohibitions remain controlling and `VISION.md` was not modified.
The frozen-input guard now pins PLAN `84B35BE...3C66`, VISION
`B970F59B...6C28`, and LIMITATIONS `312F3176...68F4`.
GitHub Actions run `30650581584` was inspected: it stopped at the frozen-input
step because the prior VISION hash was still pinned. The current guard passes
locally against the owner-authored VISION revision; no later workflow step ran
in that failed job.

All 132 bounded workspace tests passed with zero failures: 8 CLI, 48 core unit,
16 core end-to-end, 9 testkit safety, 3 TUI, and 48 Windows-boundary tests. The
serial run used this newly created system-temporary root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-13949c13493d4807ac8db2634c9b9557`

Formatting, warning-denied workspace/all-target Clippy, warning-denied rustdoc,
the locked optimized workspace build, the test-storage safety backstop,
Markdown local-link validation, cargo-deny, and cargo-audit passed. Cargo-deny
reported only the allowed duplicate-version warnings for `hashbrown` and
`syn`; RustSec found no vulnerability among 146 locked dependencies.

The release binary then passed `--help`, both subcommand help surfaces, a
two-file local NTFS copy with same-run verification, a zero-write rerun, full
standalone verification, and plain report reopening under:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-smoke-a3d782db2f0149a9af3ba104e5c5c0a5`

No heavy, physical-drive, remote-write, VHDX, performance, stress,
large-scale, endurance, or forced-disconnect test ran.

## 2026-07-31 persistence-ownership review

The latest whole-tree pass found and fixed a state-artifact ownership race.
Report and compacted-journal writers previously closed their exclusive UUID
temporary before replacing the final path and deleted a failed temporary by
path. Another actor could therefore substitute a different object in either
close-to-rename/delete interval. Both artifacts now reuse the payload engine's
delete-pending `DestinationTemp`: the creating handle denies delete sharing,
stays live through synchronized handle-based replacement, and is the only
cleanup authority. The duplicate core UUID/open/drop implementation and the
path-based audit rename wrapper were removed. Temporary naming is shared,
uses the full UUID, and rejects empty, separator-bearing, alternate-stream, or
otherwise non-ASCII-safe run IDs before path construction. ADR 0041 records
the superseding persistence decision; PLAN, DESIGN, MAINTENANCE, production
readiness, and the changelog now match it. VISION and LIMITATIONS remain current
and were intentionally unchanged because neither the project goals nor the
user-visible copy contract changed.

All 129 bounded workspace tests passed with zero failures: 8 CLI, 46 core unit,
16 core end-to-end, 9 testkit safety, 3 TUI, and 47 Windows-boundary tests. The
final serial run used this newly created system-temporary root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-persistence-review-80b5246472ab4547a34403f44cc76fbe`

Three new regression cases prove that a live artifact temporary cannot be
removed/replaced by path, dropping it removes the exact owned object,
publication replaces the final bytes without residue, and unsafe run IDs
create nothing. Existing report fallback, terminal audit, journal compaction,
end-to-end copy, verification, same-spindle, FAT policy, and UNC/WSL policy
coverage all passed against the refactored primitive.

Formatting, warning-denied workspace/all-target Clippy, warning-denied rustdoc,
the locked optimized workspace build, and the test-storage safety backstop all
passed. Cargo-deny passed advisory, ban, license, and source policy with only
the allowed duplicate-version warnings for `hashbrown` and `syn`; Cargo-audit
found no vulnerability among 146 locked dependencies. No physical-drive,
remote-write, VHDX, performance, stress, large-scale, endurance, or
forced-disconnect test ran.

## 2026-07-31 second repository review and structural-boundary hardening

The follow-up whole-tree review fixed four correctness/resource issues and one
reporting defect: native directory names and lengths are now confined to one
record and one child component; resolved log/report/state/journal roles cannot
collide; the standard-path `mem` override reserves the coordinator chunk that
can coexist with worker buffers; journal replay retains one record instead of
the complete history; and plain output distinguishes identical skips from
different files withheld by `--replace=false`. Win32 sizing results, empty
paths, and handle-rename NUL input now fail closed. Workspace package metadata
and public schema IDs consistently use the canonical `sytelus/bigcp` URL.

All 126 bounded workspace tests passed with zero failures: 8 CLI, 44 core unit,
16 core end-to-end, 9 testkit safety, 3 TUI, and 46 Windows-boundary tests. The
final serial run used this newly created system-temporary root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-79bfeaac447b41c988fbdfa523cc08df`

New regression coverage pins record-local provider names, empty input paths,
embedded-NUL handle rename, audit-role collision before destination creation,
concurrent-buffer accounting, canonical schema IDs, and truthful skipped-file
output. Existing journal tests revalidated every-byte truncation, invalid
interior records, compaction, and checkpoint identity after the streaming-load
refactor.

Formatting, warning-denied workspace/all-target Clippy, warning-denied rustdoc,
the locked optimized workspace build, frozen-input and test-storage safety
scripts, `cargo-deny`, and `cargo-audit` all passed. Cargo metadata confirmed
the canonical repository URL on all five packages. Cargo-deny reported only
the allowed duplicate-version warnings for `hashbrown` and `syn`; RustSec found
no vulnerability among 146 locked dependencies. No heavy, physical-drive,
remote-write, VHDX, performance, stress, large-scale, endurance, or
forced-disconnect test ran.

The optimized binaries then ran a bounded local NTFS smoke workflow under:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-smoke-a9432b812b084d0b964207dfcc8c5860`

The testkit generated 3 directories, 5 files, and 78,848 bytes. Copy plus
same-run verification passed all five files; rerun classified all five as
identical; standalone verification passed all nine objects; the independent
oracle reported zero mismatches/extras; and the saved report reopened through
the release CLI. The root remains as auditable evidence and was not broadly or
recursively cleaned up.

## 2026-07-31 full repository review and boundary hardening

The owner-authorized full review covered the workspace architecture, copy and
verification engines, Win32 wrappers, CLI/TUI policy, testkit confinement,
scripts, CI configuration, and maintained documentation. It fixed atomic
journal compaction, fail-closed native/provider parsing, stream-suffix
containment, and a Windows rooted-child sandbox escape. ADRs 0038/0039 record
the persistence and boundary decisions; PLAN's journal examples, loader rules,
retention statement, and same-spindle status now match the implementation.

All 119 bounded workspace tests passed with zero failures: 8 CLI, 42 core unit,
15 core end-to-end, 9 testkit safety, 2 TUI, and 43 Windows-boundary tests. Nine
new regression cases cover the old predictable compaction sibling, rooted
sandbox children, embedded NUL input, stream traversal/non-data suffixes,
overlapping or invalid sparse ranges, malformed retrieval-pointer lengths,
out-of-buffer disk-extent counts, reparse framing, and remote query lengths.
The existing end-to-end run also exercised the refactored atomic report and
journal publication paths.

`cargo fmt --all -- --check`, warning-free workspace/all-target Clippy, locked
documentation, the locked release build, the test-storage safety backstop,
`cargo-deny`, and `cargo-audit` passed. `cargo-deny` retained only its allowed
duplicate-version warnings (`hashbrown` and `syn`); the RustSec scan found no
vulnerability. No physical-drive, remote-write, VHDX, performance, stress,
large-scale, endurance, or forced-disconnect test ran.

## 2026-07-30 UNC and WSL endpoint validation

The owner-authorized UNC/WSL endpoint change passed every routine gate after
the endpoint, filesystem, profile, prompt, report/schema, documentation, and
ADR 0037 changes were reconciled. All 110 workspace tests passed with zero
failures: 8 CLI, 41 core unit, 15 core end-to-end, 8 testkit safety, 2 TUI, and
36 Windows-boundary tests. New coverage pins ordinary/extended UNC and both WSL
aliases, guards extended local paths against UNC misclassification, keeps WSL
name keys exact, projects WSL/unknown-remote basic metadata, isolates known UNC
NTFS and local NTFS policy, selects mapped-drive endpoint policy, caps remote
source concurrency, and classifies redirector disconnect failures. The final
review also moved standalone verification to the same endpoint-aware name join
as copy.

The post-change gates were:

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets --locked`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --all-targets --locked -- --test-threads=1`
- warning-denied `cargo doc --workspace --no-deps --locked`
- `cargo build --workspace --release --locked`
- `cargo deny check` (advisories, bans, licenses, and sources passed; the
  existing informational `hashbrown` and `syn` duplicate-version warnings
  remain)
- `cargo audit` against 1,174 RustSec advisories (no vulnerability across 146
  locked dependencies)
- the frozen/governing-input and automated test-storage safety checks

The optimized local NTFS smoke workflow ran in:

`C:\Users\shitals\AppData\Local\Temp\bigcp-unc-wsl-smoke-8ff8522b02874147975466c14d8b3cee`

It generated 3 directories, 5 files, and 78,848 logical bytes; proved dry-run
left the destination absent; copied and same-run verified all five; converged
to five skips on rerun; passed standalone strict verification and the
independent nine-object oracle with zero mismatches/extras; reopened the saved
report; and parsed all 16 JSONL events.

The final read-only WSL interoperability workflow used `/etc/skel` from the
existing `u2` distribution and stored only local audit artifacts under:

`C:\Users\shitals\AppData\Local\Temp\bigcp-wsl-final-bf9480a9ef3940c78679e3be2695654b`

Both `\\WSL.LOCALHOST\u2\etc\skel` and legacy
`\\WSL$\u2\etc\skel` canonicalized to
`\\wsl.localhost\u2\etc\skel`, reported the `wsl` endpoint, enumerated the
same three files in dry-run, and left both prospective destinations absent. A
noninteractive real-copy invocation without `--accept-remote-paths` exited 5,
named the required flag, and left its destination absent.

No remote destination write, generic SMB/mapped-drive live cell, WSL
destination cell, disconnect injection, or remote performance test ran. Those
tests require a separately approved scratch share/distribution path under
`docs/TESTING.md`; the implemented profiles are not throughput claims.

The owner-approved governing documents were re-pinned to:

| File | SHA-256 |
|---|---|
| `PLAN.md` | `BCEBCB7216E53F8FE5EE08991F81C1AE4EEFC6D38FA9879593096C36C7EFF799` |
| `VISION.md` | `D8FAD02510CD02192D7D17571C96FAFF9C7673BA4FFC6312705731E91D93EC6B` |
| `LIMITATIONS.md` | `6860A8AF1DC21C3E6E825C2B75210234CD811E8D5BB007F532EA5F266ED7215D` |

## 2026-07-30 same-spindle transport validation

The owner-authorized same-spindle change passed every routine gate after the
transport, scheduler, report schema, documentation, and ADR 0036 were
reconciled. The final serial workspace suite ran only below this new C: system
temporary root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-same-spindle-final2-a06f177f79534a5aa22674416132cacc`

All 102 tests passed with zero failures: 7 CLI, 39 core unit, 15 core
end-to-end, 8 testkit safety, 2 TUI, and 31 Windows-boundary tests. The new
coverage pins transport selection (shared HDD extents versus shared SSD
extents), conflicting worker-override rejection, burst allocation and phased
read/write ordering, cancellation progress, same-spindle small-file batching,
and verified dense/sparse/named-stream copying under a forced-HDD profile.

The post-change gates were:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --all-targets --locked -- --test-threads=1`
- warning-denied `cargo doc --workspace --no-deps --locked`
- `cargo build --workspace --release --locked`
- `cargo deny check` (advisories, bans, licenses, and sources passed; the
  existing informational `hashbrown` and `syn` duplicate-version warnings
  remain)
- `cargo audit` against 1,174 RustSec advisories (no vulnerability across 146
  locked dependencies)
- the frozen/governing-input and automated test-storage safety checks

The optimized binaries then completed the bounded `e00-smoke.yaml` workflow
in:

`C:\Users\shitals\AppData\Local\Temp\bigcp-same-spindle-smoke-f853ce0ad8f94ad08b8b8389cd7d3eaa`

It generated 3 directories, 5 files, and 78,848 logical bytes; proved dry-run
left the destination absent; copied and same-run verified all five; converged
to five skips on rerun; passed standalone strict verification and the
independent nine-object oracle with zero mismatches/extras; reopened the saved
report; and parsed all 16 JSONL events. The host C: topology was correctly
reported as same-device SATA SSD and retained the standard 32-worker transport,
so this smoke also confirms the HDD-only optimization does not replace the SSD
path.

No performance, stress, VHDX, or real-HDD test ran. No physical drive was
mounted, formatted, dismounted, or otherwise modified, and no writes targeted
F:, G:, or H:. The implementation is correctness-tested but not yet
real-spindle performance-certified; that `[HW]` benchmark remains subject to
the separate permission protocol in `docs/TESTING.md`.

The owner-approved governing documents were re-pinned to:

| File | SHA-256 |
|---|---|
| `PLAN.md` | `DFE35C96636EBBE3E9F3A361BF169EFE567C9D64B35020CECF8A44EFF1B42BD2` |
| `VISION.md` | `739678D0602FEF55D19A988D9FD60B17E21EA4A4EC59D9094F9DDD90A38CB678` |
| `LIMITATIONS.md` | `8FA86A1283BF8B303BBC9713D1A9B61213E23333B9320CD350C3E2D15FC29F54` |

## 2026-07-30 FAT/FAT32/exFAT support validation

The owner-authorized filesystem-policy change passed every routine gate after
FAT/FAT32 and exFAT support, documentation, and ADR 0035 landed. The full
serial suite ran only below this new C: system-temporary root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-fat-exfat-cf85c24844ef4bb985ebe5ead955505b`

All 95 tests passed with zero failures: 6 CLI, 34 core unit, 14 core
end-to-end, 8 testkit safety, 2 TUI, and 31 Windows-boundary tests. New cases
pin FAT/exFAT timestamp and attribute projection, FAT's exact file-size
boundary, strict NTFS tick equality, projected-report serialization, the
default-no degradation prompt, explicit copy-only acceptance, fast routing
when an unrepresentable large ADS is dropped, and the narrow Win32 error sets
that authorize legacy identity/enumeration/rename fallback. The unchanged 14
end-to-end cases continue to exercise the strict NTFS copy/rerun/ADS/EA/sparse/
link/cancellation/audit contract.

The post-change gates were:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --all-targets --locked -- --test-threads=1`
- warning-denied `cargo doc --workspace --no-deps --locked`
- `cargo build --workspace --release --locked`
- `cargo deny check` (advisories, bans, licenses, and sources passed; the
  existing informational `hashbrown` and `syn` duplicate-version warnings
  remain)
- `cargo audit` against 1,174 RustSec advisories (no vulnerability across 146
  locked dependencies)
- the frozen/governing-input and automated test-storage safety checks

The optimized NTFS smoke workflow ran in:

`C:\Users\shitals\AppData\Local\Temp\bigcp-fat-exfat-smoke-c672d729ac794a73bf6e540b65df5b25`

It generated 3 directories, 5 files, and 78,848 logical bytes; proved dry-run
left the destination absent; copied and same-run verified all five; converged
to five skips on rerun; passed standalone verification and the independent
nine-object oracle with zero mismatches/extras; and reopened the saved report.
The report identifies NTFS and marks this strict verification as
`projected: false`.

This session was not elevated and had no `New-VHD`/`Mount-VHD` cmdlets, so the
disposable FAT32/exFAT/ReFS VHDX matrix did not run. No disk was mounted,
formatted, dismounted, or otherwise modified. The implementation is therefore
explicitly not FAT/exFAT matrix-certified; `LIMITATIONS.md`, PLAN §12.5, ADR
0035, and `docs/PRODUCTION_READINESS.md` preserve that evidence boundary.

The owner-approved governing documents were re-pinned to:

| File | SHA-256 |
|---|---|
| `PLAN.md` | `19C964E77CF5363F36F8F664CE79341E6F94CBEDB86117AFF6203BB37DE0A427` |
| `VISION.md` | `FCB948161642B8519AED534C05183713180C4405B0A8DB20A1AB122A4F898C5B` |
| `LIMITATIONS.md` | `EAD3C7E22F2F760CA4AC213E82CBFDB34EDDC249092CD18669A0D507DBC1B72D` |

## 2026-07-30 full-repository review validation

The owner-authorized full review passed every routine quality gate after the
code, tests, comments, governing documents, ADRs, and public log schema were
reconciled. The final serial workspace suite ran only below this new C: system
temporary root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-final2-4704384698344bb4b8f99242e8c42cce`

All 83 tests passed with zero failures: 4 CLI, 27 core unit, 14 core
end-to-end, 8 testkit safety, 2 TUI, and 28 Windows-boundary tests. New cases
cover direct-replacement identity mismatch without truncation, identity-checked
protected-DACL capture, read-only direct replacement, `--replace=false` audit
facts, sub-threshold sparse-file fidelity, failover-event JSON escaping, run
isolation for phase timings, and rejection of the removed non-functional
`streams` tuning key. The full-fidelity end-to-end fixture also exercises the
new transactional routing for sub-threshold ADS/EA files.

The post-change gates were:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --all-targets --locked -- --test-threads=1`
- warning-denied `cargo doc --workspace --no-deps --locked`
- `cargo build --workspace --release --locked`
- `cargo deny check` (advisories, bans, licenses, and sources all passed;
  informational transitive duplicate-version warnings remain for `hashbrown`
  and `syn`)
- `cargo audit` against 1,174 current RustSec advisories (no vulnerability
  finding across 146 locked dependencies)
- the repository's frozen-input, test-storage-safety, Markdown-link, JSON,
  whitespace, and unsafe-code-rationale checks

The optimized binaries then completed the bounded `e00-smoke.yaml` workflow in:

`C:\Users\shitals\AppData\Local\Temp\bigcp-release-final2-d7186f35e3cc47a494e9b9a492145917`

It generated 3 directories, 5 files, and 78,848 logical bytes; copied and
read-back-verified all five; converged to five skips on rerun; passed standalone
full verification and the independent nine-object oracle with zero mismatches
or extras; reopened the saved report; and left a parseable 29-event JSONL log.

This review intentionally updated `PLAN.md` and `LIMITATIONS.md` to describe the
shipped mixed direct/temp completion protocols and the still-open bounded
huge-directory fallback. `VISION.md` remained byte-identical. The
owner-approved review fixes then adjusted `PLAN.md` once more (same-handle-only
direct revalidation wording; a pseudocode variable fix), and the governing hash
guard was re-pinned to:

| File | SHA-256 |
|---|---|
| `PLAN.md` | `BAA627C4626D6C7B793E327E9E085495D20209B6183800B1C014D0DA75899C86` |
| `VISION.md` | `C6446CDF4485E4D0D17118B34BBA1D0E44140FA45F674F6889ACD8374C417FDC` |
| `LIMITATIONS.md` | `DA26CD222BECEF0B6ED4F175FB6298ECD96C0D5FE60062DAF4926BAFBC426863` |

## 2026-07-28 initial implementation outcome

The implementation passed its routine Windows quality, unit, integration,
release-build, supply-chain, and executable smoke gates. All filesystem tests
were confined to newly created, uniquely named children of
`C:\Users\shitals\AppData\Local\Temp`. Immediately before the final automated
suite, C: was confirmed as a healthy, fixed NTFS volume.

The final serial workspace suite used:

`C:\Users\shitals\AppData\Local\Temp\bigcp-final-contract-bd25ffecf14844beb151e2f3e236506a`

PLAN.md, VISION.md, and LIMITATIONS.md were treated as frozen inputs. The final
SHA-256 verification produced:

| File | SHA-256 |
|---|---|
| `PLAN.md` | `E85B5AB9ABD335C9F277600416C296A320D35C2B41DB369A8E361E5E9B018C45` |
| `VISION.md` | `1563557009A73096125F40BD0FFBB8C406E0F392D8FB121B147C46FDFBED99B8` |
| `LIMITATIONS.md` | `B66D610848E5BFD35ABD7C5B30EBF3E9311CFE393AF6563945F69BBF5673ECCE` |

## Tooling installed

- Rust MSVC toolchain 1.97.1 (`rustc 1.97.1`, 2026-07-14), including
  `rustfmt` and Clippy.
- Visual Studio 2022 Build Tools 17.14.37516.0 with the x64 C++ toolchain and
  Windows SDK needed by the `windows-sys` boundary.
- `cargo-deny` 0.20.2 and `cargo-audit` 0.22.2.

The repository's `scripts/cargo-msvc` launcher discovers these tools and avoids
interactive `cmd.exe` AutoRun hooks. `.gitattributes` explicitly keeps source,
documentation, configuration, and Windows scripts in LF form, matching the
repository's check-in policy.

## Quality and automated tests

The following gates passed:

- `scripts/check-frozen-inputs.ps1`
- `scripts/check-test-safety.ps1`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --all-targets --locked`
- `cargo build --workspace --release --locked`
- `cargo doc --workspace --no-deps --locked`
- `cargo deny check`
- `cargo audit`

The final workspace test run passed 33 tests with zero failures:

| Area | Tests | Important coverage |
|---|---:|---|
| CLI grammar and pre-I/O validation | 3 | command grammar, invalid tuning, standalone-verify flag rejection |
| Core unit tests | 13 | exact classification and EA repair, exit/folder/phase accounting, case-insensitive lock identity, schemas, journal CRC/torn-tail/truncation safety |
| Core end-to-end tests | 4 | dry-run, atomic replace, sparse data, ADS/EAs, large/small paths, cancellation, unsafe audit-path preflight, rerun, both verification forms |
| Testkit safety | 2 | F:/G:/H: rejection before access and validated system-temp acceptance |
| TUI | 1 | every live tab renders in a bounded terminal |
| Windows boundary | 10 | paths, streams, enumeration, volume validation, read-only source handles, EAs, exact-destination mutex exclusion, atomic publication |

`cargo-deny` reported all advisory, ban, license, and source checks as passing.
It emitted informational duplicate-version warnings for transitive `hashbrown`
and `syn` versions. `cargo-audit` scanned 146 locked dependencies and exited
successfully with no vulnerability finding.

## Release-executable smoke workflow

The optimized `bigcp.exe` and `bigcp-testkit.exe` binaries were exercised in:

`C:\Users\shitals\AppData\Local\Temp\bigcp-cli-smoke-final-eea8b8516c71465bb2aa7527387ef9a6`

The bounded `e00-smoke.yaml` scenario generated 3 directories, 5 files, and
78,848 logical bytes. The workflow then established all of the following:

1. Dry-run discovered five new files and left the destination path absent.
2. Copy plus post-copy verification copied all five files with zero failures.
3. The independent testkit oracle checked nine objects and found zero
   mismatches and zero extras.
4. Standalone full verification passed all nine objects with zero mismatches.
5. An unchanged rerun copied zero files and skipped all five files.
6. The saved JSON report reopened successfully through `bigcp report --plain`.
7. The report contained its semantic configuration, reconciled top-level folder
   summaries, fastest/slowest active phase summary, and actual report path.
8. No `.bigcp-*.part` or `.bigcp.*.tmp` opaque temporary remained after
   successful completion.

The oracle separately reports last-access-time observations because reading a
file can change that system-managed timestamp. Last-access time is not part of
the promised copy fidelity and did not count as a content or metadata mismatch.

## How drive and existing-file safety was enforced

- No unit, integration, smoke, stress, performance, or endurance test used F:,
  G:, or H:. The only appearances of those drive letters in a test are inert
  path values passed to a rejection function; the guard rejects them before an
  existence query, directory creation, or file open.
- Test fixture writes occurred only below new GUID-named C: temporary roots.
  Tests never selected a drive root, existing user directory, repository source
  tree, junction, symlink, mount alias, SUBST path, or unmarked scratch folder
  as a mutable fixture.
- The testkit requires an empty directory to be explicitly marked, resolves its
  final path, rejects traversal and reparse aliases, and bounds declared writes.
- Routine tests never opened a physical drive, issued a raw-volume write,
  mounted/dismounted or formatted a volume, changed partitions, filled a drive,
  removed existing files, or simulated cable/device removal.
- Source files were opened read-only. Destination tests created their own new
  trees; replacement tests replaced only files the same test had just created.
  Temporary-file cleanup is restricted to implementation-owned opaque names.
- The final smoke fixture occupied 184,382 bytes including sources,
  destinations, journals, logs, reports, and the sandbox marker. Automated test
  fixtures stayed within the budgets documented in `docs/TESTING.md`.
- Where automatic cleanup occurred, it targeted only RAII-owned inner
  temporary directories. The printed top-level evidence roots were left in
  place, including the small final smoke fixture, so no broad or recursive
  cleanup command was run against C:, D:, or any user directory.

The long-running chaos, disposable VHDX/ReFS, million-entry, differential,
hardware-loss, and performance/endurance matrices were deliberately not run.
They require dedicated disposable fixtures or designated scratch hardware and
would have violated the harmless routine-test boundary on this machine. These
unclaimed gates are recorded in `PLAN_DEVIATIONS.md`.

## 2026-07-28 comprehensive review validation

A subsequent whole-repository review added correctness and safety coverage for
symbolic-link publication, reparse-object ADS, dangling-link sandbox escape,
checkpoint routing below the large-file threshold, destination-root type
validation, metadata-repair revalidation, bounded library tuning, raw error
classification, report fallback, and terminal audit closure.

The final serial workspace run used the newly created root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-final-review-8959731fa70e4a738a388acddff87386`

It passed 43 tests with zero failures: 3 CLI, 16 core unit, 7 core
end-to-end, 3 testkit safety, 1 TUI, and 13 Windows-boundary tests. The
following gates also passed after the review changes:

- Frozen-input hash and automated test-storage safety scripts.
- `cargo fmt --all -- --check`.
- Clippy across all workspace targets with warnings denied.
- Locked full workspace tests, documentation, and optimized release build.
- `cargo deny check` and `cargo audit`; only informational duplicate transitive
  versions were reported, with no advisory, ban, license, source, or
  vulnerability failure.

The optimized executables were then exercised under:

`C:\Users\shitals\AppData\Local\Temp\bigcp-review-smoke-5b222d21c7154398ae10f8c505fa01e7`

That smoke workflow generated exactly 78,848 source bytes, confirmed dry-run
left the destination absent, copied and post-copy verified five files, found
zero oracle mismatches/extras across nine objects, passed standalone full-tree
verification, skipped all five files on rerun, and reopened the saved report.

Every new filesystem test created only GUID-named children of the validated C:
system temporary directory. Link and ADS tests targeted only files created by
the same test inside its marked sandbox. The destination-file rejection test
verified its test-owned sentinel bytes were unchanged. No test mounted,
formatted, filled, dismounted, benchmarked, or issued raw operations to any
drive; no existing user file was used as source or destination. F:, G:, and H:
were not accessed—their only test appearances remained inert path values sent
to the guard that rejects them before any filesystem query. Long-running,
destructive, external-drive, VHDX/ReFS, chaos, and endurance gates remained
unrun for the safety reasons documented above.

## 2026-07-28 second independent safety review

The follow-up whole-repository review hardened resume ownership, final-component
reparse handling, destination-race detection, reparse finalization, named-stream
accounting, and failed-subtree audit completeness. It added ADR 0026 for the
additive journal identity fields and recorded newly confirmed performance and
architecture differences in `PLAN_DEVIATIONS.md`.

The final serial workspace run used:

`C:\Users\shitals\AppData\Local\Temp\bigcp-final-second-review-v7-b18003f8d7a0429787a795703c7c83a1`

It passed 54 tests with zero failures: 3 CLI, 19 core unit, 9 core end-to-end,
3 testkit safety, 1 TUI, and 19 Windows-boundary tests. New cases proved that:

- a reused opaque temp name cannot authorize mutation or deletion when its
  filesystem identity differs;
- legacy identity-less and future-version journal records fail safe without
  trusting or truncating unsupported state;
- ordinary source opens and directory ADS handles do not follow a final
  reparse point;
- an opened named stream belongs to the enumerated source file identity and
  the default stream reader does not follow a substituted link;
- checked ADS/EA access refuses a mismatched object identity without changing
  the test-owned stream or file, while directory ADS/EA copying still converges;
- a dangling-link new-name collision remains intact;
- dry-run and failure accounting include discovered named-stream bytes;
- standalone verification rejects file roots without changing them; and
- every discoverable descendant of a failed parent subtree receives an audit
  outcome while the conflicting destination sentinel remains unchanged.

The following final gates also passed: frozen-input hashes, automated
test-storage checks, rustfmt, warning-denied Clippy, locked workspace tests,
rustdoc, optimized release build, `cargo deny check`, and `cargo audit` over
146 locked dependencies. Cargo-deny reported only the existing informational
transitive `hashbrown` and `syn` duplicates; every advisory, ban, license, and
source policy passed, and cargo-audit found no vulnerability.

The optimized executable smoke workflow used:

`C:\Users\shitals\AppData\Local\Temp\bigcp-final-second-review-smoke-v7-526028d5543a4a46afd4e68b75497d11`

It generated 78,848 source bytes, proved dry-run left the destination absent,
copied and post-copy verified five files, passed the independent nine-object
oracle and standalone verification with zero mismatch or extra, skipped all
five files on rerun, reopened the saved report, and left no opaque temp. The
entire retained smoke fixture occupied 184,712 bytes.

Both roots were newly created on C: beneath the system temporary directory and
were left as evidence; no broad cleanup command ran. Tests changed only their
own files, links, streams, journals, reports, and sentinels. F:, G:, and H:
were never queried or opened; their only appearances remained inert arguments
to the pre-access rejection test. No routine test mounted, formatted, filled,
dismounted, benchmarked, or issued raw writes to any drive, and no existing
user file or external/removable drive was used. The unrun destructive,
long-running, VHDX/ReFS, chaos, differential, million-entry, and endurance
gates remain explicitly release-blocking in `PLAN_DEVIATIONS.md`.

## 2026-07-29 deviation-disposition and hardening review

This review dispositioned all 21 `PLAN_DEVIATIONS.md` entries, aligned PLAN.md
with the owner's hardened VISION test guidance (which was not modified),
implemented the `--analyze` live-run insight flag, and fixed the review's
confirmed findings — most notably creation-time EFS preservation, the
extended-prefix device-profiling defect, dry-run/flag-change checkpoint
destruction, interior-journal-record tolerance with clean-end compaction,
de-tautologized I6 discovery accounting, SubstituteName-authoritative symlink
fidelity, worker-panic containment, and the exit-code contract for usage
errors. Details and rationale are in `docs/REVIEW_2026-07-29.md` and
`CHANGELOG.md`.

The final serial workspace run used the newly created root:

`C:\Users\shitals\AppData\Local\Temp\bigcp-tests-75a8bec1a4a045aa9992ff68ecf167c7`

It passed 63 tests with zero failures: 3 CLI, 22 core unit, 10 core
end-to-end, 3 testkit safety, 1 TUI, and 24 Windows-boundary tests. New or
strengthened cases cover: extended-prefix and drive-letter device-path
resolution; symlink SubstituteName-over-PrintName selection, relative-flag
round-trip, and volume-GUID refusal; interior-bad-journal-record skip without
truncating later records; clean-end journal compaction keeping the job header
and live checkpoints; outcome-without-discovery loudness; opaque temp-name
shape enforcement on resume (including the ADS-colon rejection); and the
`--analyze` contract (five fixed buckets summing to copied files, a bounded
slowest table, exactly one `analysis` log event, and full absence without the
flag).

The following gates also passed after all changes: `cargo fmt --all -- --check`,
warning-denied Clippy across all workspace targets, the locked serial workspace
suite above, `cargo build --workspace --release --locked`, the rewritten
`scripts/check-test-safety.ps1` (substring scan for destructive storage
commands across crates, scripts, and CI plus anchored guard assertions), and
`scripts/check-frozen-inputs.ps1` re-pinned to the post-review PLAN.md,
owner-updated VISION.md, and LIMITATIONS.md hashes recorded in that script.

Safety posture of this pass: the testkit generator now structurally caps any
scenario at 10,000 entries and depth 32, so the VISION prohibition on
large-scale trees is enforced by code, not convention. Every new test writes
only inside GUID-named children of the validated C: system temporary
directory; the historical `.bigcp.*.tmp` pattern mentioned in earlier sections
predates the current single opaque `.bigcp-….part` scheme. No test in this
pass — added, changed, or retained — mounts, formats, fills, dismounts,
benchmarks, forces disconnects, or issues raw operations against any drive,
and scale/device-loss behavior still requires the planned simulation and fault
injection harnesses; it was not reproduced on real devices.

## 2026-07-29 owner-approved one-time evidence run

The owner granted one-time permission for the heavy-tier benchmarks, the
elevated ReFS matrix, and a real-hardware run confined to a new directory on
H:. Outcomes:

- **Heavy benchmarks (executed):** fresh GUID sandboxes on D: (source) and
  the C: user temp (destination); 20,000×4 KiB and 2×8 GiB workloads with
  one robocopy reference point each; results and honest findings recorded in
  `BENCHMARKS.md`, raw reports in `docs/evidence/2026-07-29/`. Total writes
  ≈ 40 GiB across the two internal NVMe volumes; every fixture was deleted
  after evidence capture (only the session's own GUID-named directories were
  removed; an empty `D:\bigcp-bench` shell folder remains).
- **H: hardware run (aborted, zero writes):** the first directory-creation
  on H: returned Win32 error 23 (CRC data error). Nothing was created,
  nothing was written, no existing file was touched, and all H: activity
  stopped immediately per the owner's no-harm instruction. The drive needs
  owner investigation (cable/bridge/media) before any future attempt.
- **Elevated ReFS matrix (not executable in this session):** the session is
  unelevated and the Hyper-V PowerShell module is absent. A one-time
  operator script (VHDX-file-confined, graceful dismount only) was prepared
  and handed to the owner; no VHD command ran and none was committed to the
  repository.

F: and G: were never touched — the whitelist rejects them and the owner's
permission covered only H:.
