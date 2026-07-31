# bigcp

`bigcp` is a reliability-first, high-throughput tree copier for local volumes,
generic UNC shares, mapped network drives, and WSL UNC paths on Windows 11
22H2 or later. Local NTFS is the strict full-fidelity path and the only
filesystem certification target. ReFS, FAT-family, generic UNC, and WSL paths
remain supported on a documented best-effort basis; FAT-family destinations
use an explicit reduced-fidelity policy, while remote endpoints use isolated
capability and transport policies. It is optimized for both large-file
streaming and directories containing many small files. Its reliability
promise is the one that matters: **when a run completes, every reported
success and failure is exactly true.** If a run is interrupted, re-running it
detects and repairs everything unfinished — including partial plain files an
interruption may leave at their final names (plain small files write directly
for speed; an interrupted data write is shorter than its source, while a file
whose whole unnamed payload landed is already a valid logical copy). Files
with ADS/EAs, sparse files, and large files go through opaque temporaries so a
multi-part logical file publishes atomically and resumed partials are verified,
never trusted.

When source and destination volumes share one rotational physical disk, bigcp
automatically selects a separate same-spindle transport: small files are read
in bounded batches before their write phase, while large/sparse/ADS streams
stage large sequential bursts before switching direction. Same-device SSDs and
independent drives keep the normal parallel/request-at-a-time path. The choice
is recorded in the log/report and changes scheduling only—not copy semantics.

The repository is currently **pre-1.0**. The ordinary-tree engine, safety
contract, and measured performance work are implemented (bigcp leads robocopy
on every measured small-file cell with default settings; see `BENCHMARKS.md`).
A bounded fallback for exceptionally large single directories and the final
production-validation pass in PLAN §12.10 remain before a 1.0 claim. Do not
treat this build as v1.0 certified for its NTFS contract until those gates
pass. Non-NTFS filesystems are intentionally best-effort rather than
certification-gated. Current evidence and status are summarized in
[docs/PRODUCTION_READINESS.md](docs/PRODUCTION_READINESS.md).

## Safety contract

- Source handles are read-only.
- Destination-only files are reported, never deleted.
- A completed run's report is exact: every success and failure it states is
  true. Until a run reports success, treat the destination as in progress —
  an interrupted run may leave incomplete files that the next run replaces.
- Files with ADS/EAs, sparse files, and large files are written to a new
  temporary, revalidated, then atomically renamed. Plain small-file
  replacements overwrite in place (keeping the destination file's
  permissions); if interrupted, the rerun replaces them — the source is never
  touched, so nothing is ever lost.
- A real FAT/exFAT destination requires one default-no confirmation before any
  copy work, or `--accept-degraded-filesystem` for deliberate scripted use.
- A real copy involving UNC, a mapped remote drive, or WSL requires one
  default-no confirmation before copy work, or `--accept-remote-paths` for
  deliberate scripted use. If FAT/exFAT and remote warnings both apply, they
  share that one startup prompt. Dry-run and standalone verification do not
  require acceptance.
- Unsupported local filesystems and nested roots are rejected before tree
  copying.
- A machine-wide exact-destination lock prevents two writers from targeting
  the same root.
- Every terminal outcome and requested same-run verification result is written
  to versioned JSONL and reconciled in the final report.

The skip heuristic is unnamed-stream size plus last-write time at the
destination filesystem's representation: exact `FILETIME` on NTFS/ReFS,
2 seconds on FAT, and 10 milliseconds on exFAT. It deliberately avoids reading
already-matching files. Run `bigcp verify` when an authoritative comparison of
content and every destination-representable field is needed.

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

# Noninteractive copy to an intentionally reduced-fidelity FAT/exFAT target
bigcp C:\source E:\destination --plain --accept-degraded-filesystem

# Noninteractive copy from WSL or a generic share after reviewing remote limits
bigcp '\\wsl.localhost\Ubuntu\home\me\source' D:\destination --plain --accept-remote-paths
bigcp C:\source '\\server\share\destination' --plain --accept-remote-paths

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
| `--flush` | Flush each completed file after data and metadata; large files are flushed after publication. |
| `--include-system` | Include root OS artifacts excluded by default. |
| `--skip-cloud` | Skip placeholders instead of hydrating them. |
| `--no-sparse` | Write sparse source files densely. |
| `--raw-reparse` | Opt into verbatim unknown reparse buffers. |
| `--fresh` | Ignore prior partial checkpoints. |
| `--accept-degraded-filesystem` | Accept FAT/exFAT destination losses without an interactive startup confirmation. |
| `--accept-remote-paths` | Accept UNC/WSL disconnect, metadata, and remote-durability limits without an interactive startup confirmation. |
| `--profile CLASS[,CLASS]` | Force static source/destination device classes. |
| `--tune key=value,...` | Override bounded advanced settings. |
| `--analyze` | Collect bounded live-run insight (size-class timings, top-20 slowest copies, finer stat samples) into the log and report. |
| `--state-dir`, `--log`, `--report` | Select distinct audit locations outside both trees; log/report cannot replace the state directory or journal. |
| `--plain`, `--quiet`, `--no-color` | Select noninteractive output behavior. |

Accepted profile classes are `auto`, `nvme`, `sata-ssd`, `usb-ssd`, `hdd`, and
`unknown` (the conservative fallback profile). Advanced tune keys are
`chunk`, `threads`, `mem`, `large-threshold`, `checkpoint-threshold`, and
`same-spindle-burst`; byte sizes accept `KiB`, `MiB`, or `GiB`. There are
no stream-count or queue-depth keys: large files stream through one strictly
ordered path, so those settings would not describe real work. Redirector
transfers use a fixed two-buffer pipeline and may run independent files below
the checkpoint threshold on the bounded worker pool; local standard transfers
retain the original request-at-a-time loop. WSL has a distinct transport/profile
identity even though it reuses the ordered pipeline: Auto uses 8 MiB requests
and up to 16 workers, and WSL destination creates are striped across those
workers instead of inheriting NTFS directory affinity.
Manual bounds are enforced in the core library as well as the CLI: workers are
`1..=256`, chunks `64 KiB..=64 MiB`, and thresholds
must be positive. On the standard path, a `mem` budget reserves one coordinator
chunk and must also hold at least one large-threshold worker buffer. The
redirector path reserves two coordinator chunks and
`max(large-threshold, 2 × chunk)` for each worker. Remaining bytes cap the
worker count. The same-spindle path serializes coordinator and worker I/O, so
its 256 MiB burst is instead capped directly by `mem`; a
1 MiB–1 GiB override must be at least the larger of the effective chunk and
small-file threshold.
The phased scheduler requires one worker, so a same-spindle run rejects an
explicit `threads` value other than `1` instead of silently defeating the
topology policy.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Run completed and all attempted objects succeeded. |
| 2 | One or more objects failed or verification found mismatches. |
| 3 | Graceful user cancellation; rerun to continue. Cancel takes effect between chunks, so even a huge in-flight file stops promptly and safely. |
| 4 | Stopped early by the circuit breaker: repeated device/share disconnect or disk-full failures. Reconnect the endpoint or free space, then rerun to resume. |
| 5 | Preflight, configuration, root-lock, or fatal I/O failure. |
| 6 | Audit, output/format, or internal invariant failure. |

## Fidelity and limits

On NTFS/ReFS, data, named `$DATA` streams (including those on directories and
links), EAs, creation/last-write times, and the user-owned attribute mask are
preserved where the destination advertises support.
Directory metadata is finalized post-order. Symlinks and junctions retain
their targets without traversal. Source ACLs, owner, SACL,
compression, hard-link topology, and system-managed attributes are not copied.
Existing explicitly protected destination DACLs are preserved on replacement.

FAT/exFAT cannot represent several of those features. bigcp preserves unnamed
data and `READONLY`/`HIDDEN`/`SYSTEM`/`ARCHIVE`, projects timestamps to the
destination granularity and supported date range, copies sparse files densely, and explicitly counts
and warns about dropped ADS/EAs, expanded sparse files, and EFS state. Links fail instead of being
followed or flattened. FAT also rejects files larger than 4,294,967,295 bytes
before opening a destination; timestamps outside the driver's FAT-family date
range fail rather than being silently invented. Verification on these filesystems validates the
projected contract; it does not claim unsupported metadata survived.

Generic UNC shares preserve the fields their server advertises. Remote roots
do not receive local-disk IOCTLs, same-spindle scheduling, or dense
preallocation hints. The automatic profile uses bounded buffered requests,
overlaps one source read with one destination write through two buffers, and
can run independent non-checkpointed streams on separate workers. Server-side
cache durability remains outside bigcp's control, even with `--flush`.

`\\wsl.localhost\DISTRO\...` and legacy `\\wsl$\DISTRO\...` are supported and
share one canonical lock/state identity. WSL names are matched case-sensitively
when WSL is the destination. Regular-file bytes and last-write time are the
portable contract; Linux uid/gid/mode/xattrs, special files, Windows
creation/access times and attributes, ADS/EAs, ACLs, EFS state, sparse layout,
and reparse objects are not claimed. Unsupported reparse objects fail rather
than being followed or flattened. The WSL path uses bounded two-buffer overlap,
striped destination creates, sequential cache hints, and one authoritative
post-write metadata stamp to reduce Plan 9 calls. Windows access still crosses
that translation boundary, so native Linux tools remain faster for sustained
work entirely inside a distribution. The WSL defaults are correctness-tested
but remain performance-unmeasured until an approved disposable path is used.
See [LIMITATIONS.md](LIMITATIONS.md) and the normative
[docs/SEMANTICS.md](docs/SEMANTICS.md).

Safe test instructions and exact write budgets are in
[docs/TESTING.md](docs/TESTING.md). Never use unsafe removal as a routine test;
it can corrupt a volume. Device-loss behavior belongs in fault-injection
simulation only — forced disconnects, physical or virtual, are never performed.

## External drives: write caching and safe removal

Windows offers two policies for external drives (Device Manager → the drive
→ Policies), and the choice changes small-file copy speed dramatically —
measured at **~3.4× on a 20,000-file workload** (BENCHMARKS.md):

- **Quick removal** (Windows default): you may unplug without clicking
  anything, but every file's metadata is pushed to the drive individually —
  the slow path. bigcp detects this before a copy starts, warns, and (in an
  interactive terminal) asks once whether to continue.
- **Better performance** — the recommended setting, with its two checkboxes
  handled differently:
  - **"Enable write caching on the device": CHECK IT.** This is where the
    speedup lives (Windows batches metadata into large flushes). The risk is
    bounded: if power fails or the drive is unplugged early, recently
    written files can be lost or incomplete. A rerun repairs ordinary
    size/mtime-visible damage; follow unsafe removal with standalone
    `bigcp verify` because cache loss can occasionally preserve those fields.
    Always use **Safely Remove Hardware** before unplugging.
  - **"Turn off Windows write-cache buffer flushing": LEAVE IT UNCHECKED.**
    That setting tells Windows the device is battery-backed and suppresses
    the flush commands NTFS relies on to keep its own journal consistent.
    On power loss it can corrupt the *filesystem itself* — damage a re-run
    cannot repair. The extra speed over checkbox one is marginal for
    copying; the added risk is not.

By default a completed run guarantees *logical* completion: recently written
data can still sit in the OS or drive cache. Either run with `--flush`
(per-file flush after data, publication where applicable, and metadata) or use Windows "Safely Remove
Hardware" before unplugging an external destination. Unplugging without
either can lose acknowledged data. Reconnect the drive, rerun, and then use
standalone `bigcp verify`; if it reports a same-size/same-time mismatch, move
or remove that destination object and rerun so bigcp can recreate it.

## FAQ

- **Why was my file "skipped"?** Its destination twin matched on size,
  destination-representable last-write time, attributes, and EA size. Run
  `bigcp verify SRC DST` for a content comparison of the projected destination
  contract.
- **Why did a rerun recopy a file I saw complete?** The run was interrupted
  after data landed but before metadata; the mismatch makes the rerun replace
  it with a fully finished copy. That is the crash-safety design working.
- **What are `.bigcp-…part` files?** Opaque in-flight temps for transactional
  files (ADS/EA, sparse, or large). In-process kills remove them automatically;
  a resumable large-file partial persists on purpose and is verified before reuse. Anything the journal
  cannot prove bigcp created is reported, never auto-deleted.
- **A run was interrupted — can I trust the destination?** Not until a rerun
  completes. Plain small files write directly to their final names for speed,
  so an interruption can leave partial files there. They cannot be mistaken for a
  completed data write: a mid-write file is shorter than its source; a file
  that reached full size and exact completion metadata already contains its
  complete unnamed payload. Re-running the same command finds and replaces
  every detectable incomplete copy.
- **Why is a second run on the same destination refused?** One run per exact
  destination root per machine, by design (run lock).
- **Why does FAT/exFAT require acceptance?** Those filesystems cannot preserve
  NTFS streams, EAs, ACLs, sparse layout, EFS state, or links, and FAT has a
  4 GiB-minus-1-byte file limit. The one-time startup warning makes that loss a
  deliberate choice. NTFS retains the certified strict path; ReFS retains its
  exact capability-based mechanics but, like every non-NTFS path, is
  best-effort.
- **Why does UNC/WSL require acceptance?** A disconnected share can invalidate
  open handles, optional metadata depends on the server, and a local process
  cannot attest the server's durable cache state. WSL additionally translates
  between Windows and Linux semantics. The one-time startup warning records
  that choice; rerun after a disconnect and use `--verify` or standalone
  verification for important data.

Licensed under either Apache-2.0 or MIT, at your option.
