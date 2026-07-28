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
