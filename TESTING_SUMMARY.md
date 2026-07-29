# Implementation testing and drive-safety summary

Date: 2026-07-28

## Outcome

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
and scale/device-loss behavior remains validated by simulation and fault
injection only.

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
