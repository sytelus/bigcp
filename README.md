# bigcp

`bigcp` is a reliability-first, high-throughput tree copier for local NTFS and
ReFS volumes on Windows 11 22H2 or later. It is optimized for both large-file
streaming and directories containing many small files, while making partial
destination files structurally impossible: every file is completed under an
opaque sibling name and published by an atomic rename.

The repository is currently **pre-1.0**. The bounded reference implementation
and safe routine suites are operational; the IOCP transport and dedicated
fault/chaos/ReFS/performance release matrix remain open and are listed in
[PLAN_DEVIATIONS.md](PLAN_DEVIATIONS.md). Do not treat this build as v1.0
certified until those gates pass. The current evidence and remaining release
blockers are summarized in
[docs/PRODUCTION_READINESS.md](docs/PRODUCTION_READINESS.md).

## Safety contract

- Source handles are read-only.
- Destination-only files are reported, never deleted.
- Replacements are written to a new temporary, revalidated, then atomically
  renamed; an existing destination is never truncated in place.
- FAT, exFAT, remote/mapped network volumes, UNC paths, and nested roots are
  rejected before tree copying.
- A machine-wide exact-destination lock prevents two writers from targeting
  the same root.
- Every terminal outcome is written to versioned JSONL and reconciled in the
  final report.

The skip heuristic is exact unnamed-stream size plus exact last-write
`FILETIME`. It deliberately avoids reading already-matching files. Run
`bigcp verify` when an authoritative content/ADS/EA/tree comparison is needed.

## Build

Install Rust 1.97.1 (MSVC target), Visual Studio 2022 Build Tools with the x64
C++ toolset, and a Windows 10/11 SDK. The repository launcher discovers the
installed MSVC and SDK versions without evaluating interactive `cmd.exe`
AutoRun hooks:

```powershell
cmd.exe /d /c scripts\cargo-msvc.cmd build --release --locked
```

The produced binary is `target\release\bigcp.exe`. The MSVC CRT is linked
statically. See [docs/MAINTENANCE.md](docs/MAINTENANCE.md) for a reproducible
release checklist.

## Use

```powershell
# Copy, with the dashboard when stdout is a terminal
bigcp C:\source D:\destination

# Script-friendly copy and post-copy read-back of files written in this run
bigcp C:\source D:\destination --plain --verify

# Forecast destination changes without writing the destination tree
bigcp C:\source D:\destination --dry-run --plain

# Full, authoritative comparison of both trees
bigcp verify C:\source D:\destination

# Reopen a saved report
bigcp report C:\audit\run.report.json
```

The normal rerun command is the same command again. Completed files are
skipped; CRC-valid checkpoints can resume large partials only when the current
source and opaque temp identities still match and the reread prefix's
xxh3-128 digest agrees.

### Copy flags

| Flag | Meaning |
|---|---|
| `--dry-run` | Model changes; never create or mutate the destination tree. |
| `--verify` | Read back only files written by this run. |
| `--replace=false` | Report differing existing files without replacing them. |
| `--flush` | Flush each final file after rename and metadata. |
| `--include-system` | Include root OS artifacts excluded by default. |
| `--skip-cloud` | Skip placeholders instead of hydrating them. |
| `--no-sparse` | Write sparse source files densely. |
| `--raw-reparse` | Opt into verbatim unknown reparse buffers. |
| `--fresh` | Ignore prior partial checkpoints. |
| `--profile CLASS[,CLASS]` | Force static source/destination device classes. |
| `--tune key=value,...` | Override bounded advanced settings. |
| `--analyze` | Collect bounded live-run insight (size-class timings, top-20 slowest copies, finer stat samples) into the log and report. |
| `--no-unbuffered` | Reserved: pre-1.0 streaming is already buffered, so this currently changes nothing (PLAN §13.2). |
| `--state-dir`, `--log`, `--report` | Select audit locations outside both trees. |
| `--plain`, `--quiet`, `--no-color` | Select noninteractive output behavior. |

Accepted profile classes are `auto`, `nvme`, `sata-ssd`, `usb-ssd`, `hdd`, and
`unknown` (the conservative fallback profile). Advanced tune keys are
`qd-src`, `qd-dst`, `chunk`, `streams`, `threads`, `mem`,
`large-threshold`, and `checkpoint-threshold`; byte sizes accept `KiB`, `MiB`,
or `GiB`.
Manual bounds are enforced in the core library as well as the CLI: workers are
`1..=256`, concurrent streams `1..=16`, chunks `64 KiB..=64 MiB`, and thresholds
must be positive. A `mem` budget must hold at least one large-threshold buffer
and caps both chunk size and threshold-sized workers.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Run completed and all attempted objects succeeded. |
| 2 | One or more objects failed or verification found mismatches. |
| 3 | Graceful user cancellation; rerun to continue. |
| 4 | Device/capacity breaker (reserved by the report contract). |
| 5 | Preflight, configuration, root-lock, or fatal I/O failure. |
| 6 | Audit, format, or internal invariant failure. |

## Fidelity and limits

Data, named `$DATA` streams (including those on directories and links), EAs,
creation/last-write times, and the user-owned attribute mask are preserved.
Directory metadata is finalized post-order. Symlinks and junctions retain
their targets without traversal. Source ACLs, owner, SACL,
compression, hard-link topology, and system-managed attributes are not copied.
Existing explicitly protected destination DACLs are preserved on replacement.
See [LIMITATIONS.md](LIMITATIONS.md) and the normative
[docs/SEMANTICS.md](docs/SEMANTICS.md).

Safe test instructions and exact write budgets are in
[docs/TESTING.md](docs/TESTING.md). Never use unsafe removal as a routine test;
it can corrupt a volume. Device-loss behavior belongs in fault-injection
simulation only — forced disconnects, physical or virtual, are never performed.

## Removing a drive after a copy

By default a completed run guarantees *logical* completion: recently written
data can still sit in the OS or drive cache. Either run with `--flush`
(per-file flush after rename and metadata) or use Windows "Safely Remove
Hardware" before unplugging an external destination. Unplugging without
either can lose the tail of an otherwise successful copy; a rerun detects and
repairs it, but only after the drive is reconnected.

## FAQ

- **Why was my file "skipped"?** Its destination twin matched on size, exact
  last-write time, attributes, and EA size. Run `bigcp verify SRC DST` for a
  full content comparison.
- **Why did a rerun recopy a file I saw complete?** The run was interrupted
  after data landed but before metadata; the mismatch makes the rerun replace
  it with a fully finished copy. That is the crash-safety design working.
- **What are `.bigcp-…part` files?** Opaque in-flight temps. In-process kills
  remove them automatically; a resumable large-file partial persists on
  purpose and is verified before reuse. Anything the journal cannot prove
  bigcp created is reported, never auto-deleted.
- **Why is a second run on the same destination refused?** One run per exact
  destination root per machine, by design (run lock).
- **Why NTFS/ReFS only, local volumes only?** See LIMITATIONS.md — the
  restriction buys exact timestamps, stable file IDs, and atomic replaces.

Licensed under either Apache-2.0 or MIT, at your option.
