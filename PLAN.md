# bigcp — Engineering Plan

> **Status:** Approved design, ready for implementation.
> **Companion documents:** `VISION.md` (product vision, requirements source), this file (complete engineering design).
> **Audience:** The implementing engineer(s) and all future maintainers. This document is written so that implementation can proceed *without asking anyone questions*. Where a decision could go multiple ways, the decision is made here and the reasoning recorded.

---

## Table of contents

1. [Executive summary](#1-executive-summary)
2. [Requirements and scope](#2-requirements-and-scope)
3. [Background research](#3-background-research)
4. [Copy semantics specification (the contract)](#4-copy-semantics-specification-the-contract)
5. [Architecture](#5-architecture)
6. [Key algorithms](#6-key-algorithms)
7. [Reliability and failure-mode design](#7-reliability-and-failure-mode-design)
8. [Performance engineering](#8-performance-engineering)
9. [Edge-case catalog](#9-edge-case-catalog)
10. [CLI, log format, report format](#10-cli-log-format-report-format)
11. [Terminal UI design](#11-terminal-ui-design)
12. [Test plan](#12-test-plan)
13. [Implementation roadmap](#13-implementation-roadmap)
14. [Documentation and maintainability](#14-documentation-and-maintainability)
15. [Risks and mitigations](#15-risks-and-mitigations)
16. [Appendices](#16-appendices)

---

## 1. Executive summary

**bigcp** is a Windows-only command-line copy tool whose two ranked goals are:

1. **Reliability** — when bigcp reports a file as copied, the destination file's path, content, and copied metadata are correct; bigcp never deletes or corrupts anything it did not create, and treats the source as strictly read-only.
2. **Throughput** — saturate modern storage (NVMe/SATA SSDs and HDDs, especially external drives on USB-C) on modern machines (many cores, 32–64 GB RAM), across the workloads where robocopy falls down: millions of tiny files, very large files, deep directory trees, and mismatched source/destination speeds.

### Headline design decisions (each justified later in the document)

| Decision | Choice | Section |
|---|---|---|
| Language | **Rust** (stable toolchain, MSRV pinned) | §5.1 |
| Win32 access | `windows-sys` bindings wrapped in one dedicated crate holding 100 % of the `unsafe` code | §5.2, §14 |
| I/O strategy | **Two engines**: parallel buffered engine for small files, IOCP-based unbuffered overlapped streaming engine for large files; per-device queue depths and request sizes chosen by a device profiler | §5.8–§5.9 |
| Enumeration | Parallel work-stealing directory walk with large-fetch enumeration; destination compared via **per-directory join** (one dir listing instead of N per-file stats) | §5.6 |
| Skip heuristic | robocopy-compatible size + mtime (with per-filesystem tolerance), plus cheap *attribute repair* without data rewrite | §4.1 |
| Overwrite safety | Existing destination files are **never truncated in place**; replacement is always written to a temp name and atomically renamed over | §4.3 |
| Resume | Idempotent re-run via the skip heuristic, plus a journal that resumes *partially copied large files* from a watermark | §5.12 |
| Verification | Optional hash-during-read (xxHash3-128 default, BLAKE3 optional) + unbuffered read-back verify pass; standalone `bigcp verify` subcommand | §5.17 |
| UX | Full-screen dashboard TUI (ratatui) with tabs for errors, devices, performance, hints; `--plain` mode for scripts; `bigcp report` re-opens a saved report | §11 |
| Machine-readable output | JSONL event log + JSON report, both versioned with published schemas | §10 |
| Deletion capability | **Absent by design.** bigcp contains no code path that deletes or overwrites destination files except (a) intentional replacement of a differing file via temp+rename and (b) cleanup of its own journaled temp files | §7.1 |

### Why not just fix robocopy usage with better flags?

Robocopy's problems are structural, not configurational (details in §3.3): one thread per file with a fixed pipeline, `/J` unbuffered mode applied indiscriminately (which *hurts* small files), no destination-directory join (it stats per file), timestamps written in ways that leave torn files on kill, no partial-file resume that doesn't cost a re-read (`/Z` roughly halves throughput), and no visibility into *why* a run was slow. bigcp is designed around those gaps.

---

## 2. Requirements and scope

### 2.1 Functional requirements (from VISION.md)

| # | Requirement | Where designed |
|---|---|---|
| F1 | Copy a directory tree `SRC` → `DST` recursively, including empty directories (robocopy `/E`) | §4, §5.6 |
| F2 | Defaults equivalent to robocopy `/E /J /COPY:DTA /DCOPY:DAT /R:0 /W:0 /V /FP /ETA /SJ /SL` (see mapping, Appendix A) | §4, Appendix A |
| F3 | Skip files already present and identical at destination, using fast metadata heuristics | §4.1 |
| F4 | Resume close to the interruption point after intentional or abrupt termination | §5.12 |
| F5 | Copy symbolic links and junctions **as links** — never follow them | §4.6 |
| F6 | Errors: no retries by default; log every error with cause; running tally; actionable hints | §5.13 |
| F7 | Machine-parseable log file (JSONL) and a re-openable report file (JSON) | §10 |
| F8 | Verify mode that efficiently checks copies are correct | §5.17 |
| F9 | Dashboard TUI: progress, ETA, throughput, errors, navigable detail; `bigcp report <file>` to re-examine later | §11 |
| F10 | Summary: copied/failed/skipped counts, failure breakdown by top-level folder and reason, achieved vs. maximum possible throughput, start/end time, fastest/slowest portions, bottleneck analysis, improvement hints | §5.14, §10.3 |
| F11 | Avoid unnecessary disk I/O (no gratuitous stats, re-reads, or re-writes) | §5.6, §8.4 |

### 2.2 Non-functional requirements, ranked

1. **Reliability / correctness** — outranks everything, including throughput. Concretely: the *reported* outcome is always true; source is opened read-only always; destination files that bigcp did not create are never deleted, truncated, or overwritten except intentional replacement of a same-relative-path file that differs from source; a crash at any instant never leaves the destination in a state that a re-run cannot repair; counters must reconcile exactly (§7.3).
2. **Throughput** on the primary scenario: SSD/HDD, local + USB-C attached, big RAM, many cores.
3. **Usability** — the dashboard must make state, progress, and problems obvious; errors must explain themselves.
4. **Maintainability** — a new engineer can build, test, and modify the tool from the docs alone (§14).

### 2.3 Supported environment (deliberately narrow)

- **OS:** Windows 10 1809+ and Windows 11, x64 (ARM64 build is a stretch goal; no code may preclude it).
- **Filesystems:** NTFS and ReFS as first-class; exFAT and FAT32 supported with documented degradation (§4.4). Anything else: best-effort via the same degradation rules.
- **Transports:** internal NVMe/SATA, and USB-C (USB 3.x / USB4 / Thunderbolt) mass storage — this is the *primary* optimization target.
- **Elevation:** not required. If elevated, bigcp can optionally use backup semantics (`--backup-mode`).

### 2.4 Non-goals (explicitly out of scope for v1)

These are *decisions*, not omissions. Each cuts complexity that would endanger the reliability goal.

- **No network-copy optimization** (SMB works but is not tuned for; no compression, no delta transfer — see §3 research: delta transfer is CPU-bound and useless for local copies).
- **No mirror/purge mode** (`/MIR`, `/PURGE`): bigcp must never delete user files; the capability is intentionally absent from the codebase.
- **No ACL/owner/auditing copy** (`/COPY:S,O,U`): matches the required `/COPY:DTA` default. Destination files get default inherited ACLs.
- **No VSS snapshot integration** (locked files fail with a hint naming the locking process instead).
- **No move/rename mode** (`/MOV`) — moving implies deleting from source; source is read-only, period.
- **No hard-link preservation**: hard-linked source files are copied as independent files (robocopy default behaves the same). Counted and noted in the report so users are informed.
- **No EFS raw copy** (`/EFSRAW`): encrypted files are copied as readable plaintext content and re-encrypted at destination when possible (§4.2).
- **No 32-bit builds, no Windows 7/8 support.**
- **No config files** in v1 — flags only, with excellent defaults. (Revisit only if flag count grows past ~20.)
- **No MFT raw parsing** for enumeration (WizTree-style). Rejected: requires admin + NTFS-only + a second uncorrelated code path for the most correctness-critical stage; parallel `NtQueryDirectoryFileEx` enumeration is within striking distance at far lower risk. (§3.5)

### 2.5 Glossary

See §14.5 — the glossary is part of the maintainability contract. Terms used heavily below: **QD** (queue depth: I/Os in flight per device), **MTL** (`MaximumTransferLength` from the storage adapter), **VDL** (NTFS valid data length), **UASP** (USB Attached SCSI protocol), **engine** (a copy execution strategy: *small-file engine* or *streaming engine*), **join** (matching a source directory listing against the corresponding destination listing), **oracle** (the independent tree-comparison checker used by tests, §12.2).

## 3. Background research

Findings from a structured survey (July 2026) of Microsoft documentation, OS-internals write-ups, tool source/documentation (FastCopy, robocopy, rclone, rsync, TeraCopy, fcp/xcp), and storage-hardware deep dives. Bracketed tags cite Appendix E. Facts marked *(uncertain)* could not be pinned to an authoritative source and must be verified by measurement during implementation (they are all benchmark-verifiable; §12.6).

### 3.1 Why robocopy underperforms — the concrete mechanisms

1. **Per-file parallelism only, default 8 threads.** `/MT:n` (1–128) parallelizes whole files; a single large file is never split, and `/MT` gives little on large-file workloads [MS-robocopy][Andys-MT]. Small-file workloads peak around `/MT:16` in community benchmarks — well short of what modern SSD queue depths accept.
2. **`/J` is a blunt instrument.** It applies unbuffered I/O to *every* file; unbuffered small-file writes lose cache-manager write coalescing and get slower, compounding under `/MT` [MS-robocopy-QA]. The right policy is size-dependent — which is exactly what Windows' own copy engine learned in the Vista SP1 rework (cached I/O below ~256 KiB, pipelined larger I/O above) [Russinovich-SP1].
3. **Restartable mode `/Z` costs ~3×.** Measured 950 → 270 Mbps on 1 GbE when enabled [Stoomkracht-Z]; where its restart state lives is undocumented. Resume must be cheap or nobody uses it — bigcp's watermark design (§5.12) has no steady-state cost.
4. **Retry defaults are a trap:** `/R:1000000 /W:30` means one locked file can stall a job for weeks [MS-robocopy]. (The required `/R:0 /W:0` defaults avoid this; bigcp adopts them.)
5. **Per-file destination stats, per-file attribute reopens** *(uncertain, inferred from behavior)* — no destination-directory join; every file pays extra opens.
6. **No diagnosis.** Robocopy reports what it did, never *why it was slow* — no device awareness, no bottleneck attribution, no hints. This is a product gap as much as a perf gap.
7. Robocopy *has* quietly gained modern features worth matching or noting: `/SPARSE`, ReFS block-clone by default (`/NOCLONE` to opt out), `/NOOFFLOAD`, SMB compression [MS-robocopy]. bigcp matches sparse + block-clone (§5.9) and ignores the SMB-only ones (non-goal).

### 3.2 Windows I/O stack facts the design relies on

- **Kernel-mode copy exists now.** Windows 11 22H2+ `CopyFileEx`/`CopyFile2` use `NtCopyFileChunk`: kernel-requestor reads/writes with copy intent signaled at create time so minifilters (Defender) can skip double-scanning [MS-km-copy]. A hand-rolled engine forgoes that filter-skip — one reason bigcp keeps a `--engine os` backend (§5.8) for A/B honesty and as a fallback. 22H2 also shipped (then fixed) a large-copy regression [Neowin-22H2] — a reminder to benchmark per-OS-build in CI.
- **What `CopyFileEx` handles automatically** (and a custom engine must reimplement): ADS, EAs, attributes, sparse/compression flags, EFS re-encryption, preallocation; it does *not* copy DACLs [MS-CopyFileEx]. Modern `CopyFile2` flags cover most of our semantics à la carte (`COPY_FILE_NO_BUFFERING`, `ENABLE_SPARSE_COPY`, `OPEN_AND_COPY_REPARSE_POINT`, `SKIP_ALTERNATE_STREAMS`, `DIRECTORY`, `DISABLE_PRE_ALLOCATION` — the last confirming the OS engine preallocates by default) [MS-CopyFile2]. This makes the OS backend nearly semantics-complete on 19041+, strengthening its role as differential-test oracle #2 (§12.6).
- **Unbuffered I/O rules:** offsets/lengths must be multiples of the volume's logical sector size; buffers aligned to physical sector size (query via `StorageAccessAlignmentProperty`; VirtualAlloc's page alignment satisfies it; align to `max(4096, physical)` and be done) [MS-buffering]. `WRITE_THROUGH` is orthogonal (FUA; some USB bridges ignore it — `FlushFileBuffers` issues a real SYNCHRONIZE CACHE that drives near-universally honor [ONT-flush]). Unaligned end-of-file: the pad-then-trim technique (§4.3) works on modern NTFS via `FileEndOfFileInfo` on the same handle; historical reports of failures on other stacks *(uncertain)* mandate the buffered-reopen fallback in `win::file`.
- **Valid Data Length:** writes landing beyond VDL zero-fill the gap first; strictly-increasing completion avoids it; `SetFileValidData` avoids it too but requires admin + `SeManageVolumePrivilege` and exposes stale disk data on crash — bigcp's bounded in-order window (§5.9) gets ~all the benefit with none of the risk. rclone hit this exact issue with out-of-order multi-thread chunks and works around it by marking the destination *sparse* on Windows [rclone-local] — a valid alternative rejected here because it changes the file's allocation semantics (§3.5).
- **Enumeration:** all fast paths sit on `NtQueryDirectoryFile(Ex)`; the wins are big buffers and skipping 8.3-name retrieval. `FileIdExtdDirectoryInfo` returns size, all timestamps, attributes, EA size, **reparse tag**, and 128-bit file ID per entry — everything the classifier needs, plus hard-link detection via ID, with zero per-file opens [MS-IdExtd]. `FIND_FIRST_EX_LARGE_FETCH`-style large buffers measured ~4× on cold HDD metadata (128 s → 29 s) [Schoener-fetch].
- **Small-file cost lives in `CreateFile` and `CloseHandle`, amplified by filter drivers.** Defender scans synchronously in the post-write close path — >100 ms worst case per file; Mercurial/rustup obtained **>3×** small-file throughput by moving `CloseHandle` to a thread pool [Szorc-slow][Rustup-close]. Adopted: bigcp's small-file engine has a dedicated closer/finalizer stage (§5.8).
- **Name tunneling** re-applies a replaced file's creation time for 15 s after delete/rename [Chen-tunnel] — the reason timestamps are set *after* the final rename (§4.3).
- **`FileRenameInfoEx` (`POSIX_SEMANTICS | REPLACE_IF_EXISTS | IGNORE_READONLY_ATTRIBUTE`)** is available from Win10 1607, NTFS-only — with classic-rename fallback [MS-rename]. `FileBasicInfo` sets all timestamps + attributes in one call on the open handle; setting them as the last op before close prevents close-time clobber [MS-basicinfo].
- **Block cloning is ReFS-only** (incl. Dev Drive); in-box copy engines block-clone automatically since Win11 24H2; calls must be cluster-aligned and <4 GiB per extent [MS-clone].
- **Storage introspection needs no admin**: volumes/physical drives opened with `dwDesiredAccess=0` accept the metadata IOCTLs (`STORAGE_QUERY_PROPERTY`, `VOLUME_GET_VOLUME_DISK_EXTENTS`) [MS-ioctl-prop]. Seek-penalty and alignment queries **frequently fail on USB bridges** — the low-confidence fallback profile (§5.5) is mandatory, not defensive gold-plating. Removal policy is detectable via `IOCTL_STORAGE_GET_HOTPLUG_INFO`; device write-cache state via `IOCTL_DISK_GET_CACHE_INFORMATION` [MS-hotplug].
- **Cloud placeholders:** `RECALL_ON_DATA_ACCESS`/`RECALL_ON_OPEN`/`OFFLINE` attributes arrive in enumeration; normal reads hydrate (download); `FILE_FLAG_OPEN_NO_RECALL` opens without triggering recall; copying cloud reparse points verbatim to another volume corrupts them (they belong to the cloud filter) [MS-placeholders][MS-attrs]. Robocopy mass-hydrates silently. bigcp policy: §4.6.

### 3.3 Tool survey — what the proven tools actually do

| Tool | Architecture insight adopted / rejected |
|---|---|
| **FastCopy** [FastCopy-help] | The reference design. Different physical drives → separate reader/writer threads pipelined; **same drive → fill a big buffer with reads, then write in bulk** (alternating bursts). All I/O unbuffered (blanket policy; default request unit 2 MiB since v5.11). Verify = hash source during read + **re-read destination** (unbuffered by construction, so read-back genuinely hits the device); xxHash3 default in v5. bigcp adopts: topology-aware split, alternating bursts, hash+unbuffered read-back, xxh3. bigcp improves: size-dependent buffering policy (FastCopy is unbuffered even for tiny files), destination join, journal resume, diagnosis. |
| **robocopy** | §3.1. Also: classification vocabulary (Same/Newer/Older/Changed/Tweaked/Extra/Lonely) — bigcp's classifier (§4.1) is a cleaned-up version; `/DCOPY` letters confirmed as D/A/T/E/X with default `DA` (so plain robocopy does *not* preserve dir timestamps; the required `/DCOPY:DATE` default reads as D+A+T+E — Appendix A note). |
| **rclone** [rclone-local][rclone-docs] | size+modtime skip with per-backend `--modify-window`; `name.XXXXXX.partial` + rename; preallocation via `NtSetInformationFile`; multi-thread single-file chunks ≥256 MiB (sparse-marking to dodge VDL); **no partial-content resume** (restarts file). bigcp adopts: partial suffix + rename, preallocation, modify-window concept (as per-FS tolerance). bigcp improves: watermark resume, in-order writes instead of sparse-marking. |
| **rsync** [rsync-man] | `--whole-file` is default for local↔local (delta transfer is a *loss* locally — CPU for absent network savings; validates our no-delta non-goal). Temp-file + atomic rename; postorder directory-mtime fixup; `--modify-window` for FAT. All adopted (independently arrived at, now confirmed as the convergent design). |
| **TeraCopy** [TeraCopy-doc] | Hash-during-copy then read-back verify phase; per-file skip-and-continue with "retry failed only"; persisted job list = resume. Confirms our verify shape. Caution: no evidence its read-back defeats the OS cache — bigcp's unbuffered read-back closes that hole. |
| **Explorer / Vista SP1 engine** [Russinovich-SP1] | The canonical study in copy-engine tuning: cached I/O below 256 KiB, pipelined 1–2 MiB async I/O above, read-ahead at 2× I/O size; essentially serial per file. Validates the two-engine split and the size threshold's existence (exact crossover re-benchmarked in M3). |
| **fcp / xcp (Rust, Linux)** [fcp][xcp] | Work-stealing parallel tree walk feeding per-file parallel copies; block-level parallelism reserved for large files; explicit "not tuned for HDDs" (parallelism inverts to a penalty there — confirming our HDD clamps). Same convergent structure as bigcp's enumeration/scheduler split. |

**Skip-heuristic convergence:** every serious tool lands on size + mtime with a filesystem-granularity tolerance (robocopy `/FFT` 2 s, rsync/rclone `--modify-window`), with optional hash mode. FAT stores local time → DST shifts apparent mtimes by exactly 1 h (robocopy `/DST`) [MS-filetimes]. bigcp's §4.1 is this consensus plus per-FS automatic tolerance and attribute repair.

**Resume convergence:** no surveyed tool journals chunk state during normal copies; the universal pattern is per-file atomicity + source size/mtime revalidation. bigcp's two-layer design (§5.12) keeps that as layer 1 and adds cheap watermarks only for large files — the one place restart cost is real.

### 3.4 Hardware realities (USB-C, SSDs, HDDs)

- **UASP vs BOT:** BOT serializes one command at a time; UASP provides tagged queueing (practical concurrency ~16, bridge-firmware-bound *(uncertain)*). Windows loads `uaspstor.sys` for UAS-capable devices, `usbstor.sys` (historically 64 KiB max transfer) for BOT [MS-usb-classes][ED-uasp]. Measured: QD1 leaves throughput on the table even on a SATA dock; a Samsung T9 (20 Gbps) hit 1355 MB/s single-stream vs 1944 MB/s with 4 threads [SR-T9] — **QD/parallelism is required to saturate ≥10 Gbps links**, motivating the streaming engine's QD 4–8 and 2+ streams on fast USB.
- **Realistic link ceilings:** 5 Gbps ≈ 420–460 MB/s; 10 Gbps ≈ 1.0–1.1 GB/s; 20 Gbps ≈ 2.0–2.1 GB/s; USB4/TB 40 Gbps ≈ 3.2–3.8 GB/s [IPlus-USB][Danchar-chipsets]. These feed the report's sanity expectations, not hard-coded limits.
- **Bridge-chip reality:** RTL9210B / ASM2362 / JMS583 (10 Gbps ≈ 875 MB/s class) have documented firmware-dependent dropouts under sustained writes and thermal issues; ASM2464PD (USB4) throttles without active cooling [Danchar-chipsets][AT-bridges]. Consequences designed in: device-gone circuit breaker + resumable exit (§5.13), auto-tuner latency backoff (§6.5), no infinite max-QD hammering.
- **Portable-SSD write cliffs are normal:** pSLC caches (e.g. T9 ≈ 180 GB class) then sustained rates drop — some drives hold ~1 GB/s (T7 Shield, X10 Pro), others fall to ~500 MB/s (SanDisk Extreme class) [SR-T9][Shutter-sustained]. DRAM-less drives lose HMB over USB (the bridge is the NVMe host) → weaker sustained/random behavior *(uncertain quantitatively)*. Consequence: the bottleneck analyzer *detects and explains* the cliff (burst vs sustained rates reported separately; ETA switches to sustained rate) instead of "fighting" it — backing off cannot help; the cache drains at its folding rate regardless (§5.14).
- **HDDs:** DM-SMR external drives collapse from ~130–190 MB/s to ~10–30 MB/s once their CMR media-cache fills [STH-SMR]; **not detectable in software** (DM-SMR reports as ordinary; the only reliable route is model lists) — bigcp detects the *behavioral signature* and hints honestly. Pure sequential HDD wants large requests at QD 1–2 (NCQ helps random, not sequential); same-spindle copy alternates 64–256 MiB bursts — at 150–250 MB/s media rate a 256 MiB burst amortizes the seek pair to ~1–2 % (derived; §8.3).
- **Removal policy:** Windows ≥1809 defaults external drives to "Quick removal" (OS write caching off) [MS-removal] — per-file latency dominates small files there; detectable via hotplug IOCTL (§5.5); bigcp explains rather than overrides (§8.6). Even "completed" writes can sit in the *drive's* DRAM; only `FlushFileBuffers` (real cache-flush command) is universally honored — the basis of `--flush` (§8.6).
- **exFAT on external drives:** no metadata journal (interrupted metadata updates can corrupt the volume — flush more aggressively, §8.6); slower than NTFS for many-small-file metadata; large default clusters. Feeds the FS matrix (§4.4) and hints.

### 3.5 Techniques adopted / improved / rejected

| Technique | Verdict | Reasoning |
|---|---|---|
| Two engines, size threshold | **Adopt** (FastCopy would apply unbuffered everywhere; Explorer's history proves the split) | §8.1 |
| Topology-aware same-spindle burst alternation | **Adopt** (FastCopy-proven) | §8.3 |
| Destination per-directory join | **Improve** — none of the surveyed tools do it; eliminates per-file dest stats | §5.6 |
| Deferred CloseHandle pool | **Adopt** (Mercurial/rustup-proven, >3×) | §5.8 |
| Hash-during-read + unbuffered read-back verify | **Adopt** (FastCopy/TeraCopy shape, cache-defeat made explicit) | §5.17 |
| Temp + atomic rename; timestamps after rename | **Adopt/Improve** (rsync/rclone shape + tunneling fix) | §4.3 |
| Watermark partial resume | **Improve** — no surveyed tool has cheap large-file resume (`/Z` ≈ 3× cost; rclone restarts) | §5.12 |
| Attribute repair without data rewrite | **Improve** (robocopy needs `/IT` and recopies) | §4.1 |
| Per-FS timestamp tolerance (auto) | **Improve** (others need manual flags) | §4.1 |
| Bottleneck attribution + hints | **New** — no surveyed tool explains its own performance | §5.14 |
| Counter reconciliation as hard invariant | **New** (borrowed from storage-system design, not copy tools) | §7.3 |
| `SetFileValidData` by default | **Reject** — admin-only + stale-data exposure; in-order window is nearly as good | §5.9 |
| Sparse-marking dest to dodge VDL (rclone) | **Reject** — mutates allocation semantics of the destination file | §3.2 |
| Delta transfer (rsync) | **Reject** for local — CPU spent to save absent network | §3.3 |
| MFT raw parse enumeration | **Reject** — admin + NTFS-only + parallel walk is fast enough | §2.4 |
| Kernel copy (`--engine os`) as *default* | **Reject as default, keep as backend** — gives up scheduling, watermarks, unbuffered control, per-chunk hashing; but kept for differential testing, troubleshooting, and its AV filter-skip advantage | §5.8, §12.6 |


## 4. Copy semantics specification (the contract)

This section is normative. The implementation, tests, and user documentation (`SEMANTICS.md`, §14) must all agree with it. Any change here requires an ADR (§14.4).

### 4.1 The skip heuristic ("is the destination file already correct?")

For every source file with a corresponding destination entry (matched by relative path, compared case-insensitively; §4.5), classify:

| Classification | Condition | Action |
|---|---|---|
| **Same** | size equal AND mtime equal within tolerance (below) AND both are files (not reparse points on either side, or matching reparse type) | Skip. No data I/O. |
| **Same, attrs differ** | Same as above but copied attributes (§4.2) differ | *Attribute repair*: rewrite attributes/timestamps via one metadata operation. No data I/O. Counted as `meta_fixed` (a sub-class of skipped). |
| **Different** | size differs, or mtime outside tolerance, or type mismatch (file vs. directory vs. reparse point) | Copy with safe replacement (§4.3). Type conflicts are **errors**, not silent replacements (§9, E31). |
| **New** | no destination entry | Copy. |
| **Extra** | destination entry with no source counterpart | Never touched. Counted, sampled into the report. |

**Timestamp tolerance.** The tolerance is `max(granularity(srcFS), granularity(dstFS))`:

| Filesystem | Last-write granularity | Tolerance contribution |
|---|---|---|
| NTFS, ReFS | 100 ns | 0 (exact 64-bit FILETIME compare) |
| exFAT | 10 ms | 10 ms |
| FAT32 | 2 s | 2 s |

With `--dst-tolerance`, a difference of exactly 1 h (± the base tolerance) is *also* treated as equal — this handles FAT's local-time storage across DST changes (robocopy `/DST` equivalent). Default **off**, mirroring robocopy.

**Why size+mtime and not hashes:** it requires zero additional I/O (both values arrive in the directory enumeration record), it is the proven industry heuristic (robocopy, rsync `--whole-file`, rclone all default to it), and its false-negative mode (same size, same mtime, different content) requires either deliberate tampering or a broken program that rewrites content while restoring timestamps. Users who need stronger guarantees run `--verify` or `bigcp verify` (§5.17). We *improve* on robocopy by: (a) per-FS tolerance instead of a blanket 2 s, so NTFS→NTFS comparisons are exact; (b) attribute repair without data rewrite; (c) the hash cache (§5.12) letting a later `verify` check content without re-reading the source.

**Direction rule:** source always wins. A "different" file is replaced even if the destination is newer (robocopy's default also copies older files). bigcp is a copier, not a synchronizer; the report calls out how many replaced files were newer at destination so the user can notice a mistake.

### 4.2 What is copied, exactly

Per the required `/COPY:DTA /DCOPY:DAT` defaults:

| Item | Copied? | Mechanism | Notes |
|---|---|---|---|
| File data (default `$DATA` stream) | ✔ | engines §5.8/§5.9 | |
| Alternate data streams | ✔ | `FindFirstStreamW` enumeration; each `:name:$DATA` copied like file data | Only attempted when source volume is NTFS/ReFS. Destination without ADS support (exFAT/FAT32): streams are **dropped with a per-file warning** and counted (`streams_dropped`) — never silently (robocopy silently drops them). |
| Timestamps (create, last-write, last-access) | ✔ | `SetFileInformationByHandle(FileBasicInfo)` on the write handle, **after** rename (§4.3 ordering) | |
| Attributes (`READONLY, HIDDEN, SYSTEM, ARCHIVE, NOT_CONTENT_INDEXED, TEMPORARY, OFFLINE`) | ✔ | same `FileBasicInfo` call as timestamps (one syscall for both) | |
| `FILE_ATTRIBUTE_COMPRESSED` | best effort | `FSCTL_SET_COMPRESSION` on the new file when destination is NTFS | Failure → warning, not error. |
| `FILE_ATTRIBUTE_ENCRYPTED` (EFS) | best effort | request `FILE_ATTRIBUTE_ENCRYPTED` at destination create | Content is copied decrypted (we read plaintext); if destination cannot encrypt → warning `efs_downgrade`, file still copied. |
| Sparse allocation | ✔ | `FSCTL_QUERY_ALLOCATED_RANGES` on source; `FSCTL_SET_SPARSE` + write only allocated ranges | `--no-sparse` disables. Destination without sparse support: full expansion, with an up-front free-space check for the *logical* size. |
| Extended attributes (EAs) | ✖ v1 | — | Rare on user data; documented limitation. Revisit if demanded. |
| DACL/SACL/owner | ✖ by requirement | — | `/COPY:DTA` excludes them. |
| Directories: existence, attributes, timestamps | ✔ | create → attrs at creation; timestamps set in **post-order pass** (§5.10) | Children creation bumps parent mtime; hence timestamps must be re-set after a directory's subtree is complete. |
| Symlinks (file & dir) and junctions | ✔ as links | §4.6 | Never followed. |
| Hard links | file content duplicated per link | — | Counted via `nNumberOfLinks > 1` when cheaply available; report notes "N source files were hard links (copied as independent files)". |

### 4.3 Replacement and completion protocol (crash safety)

**New files** (no destination entry):

- **Small engine** (< `large_threshold`, default 4 MiB): write directly to the final name. A crash mid-write leaves a file whose mtime (current time) and/or size do not match the source, so a re-run reclassifies it as *Different* and re-copies. Timestamps are set **only after** all data is written — this is the invariant that makes torn files detectable.
- **Streaming engine** (≥ threshold): write to a temp name in the same directory: `«final-name».«runid8».bigcp-part`. Rationale: a 100 GB partial must never be confusable with a finished file, and temp identity enables watermark resume (§5.12).

**Replacements** (destination exists and is *Different*): **always** temp+rename, regardless of size. The old destination file remains intact and openable until the atomic step. This is a deliberate improvement over robocopy, which truncates the destination in place — a mid-copy crash under robocopy leaves *neither* the old nor the new content.

**Completion sequence** (streaming engine; small engine is the same minus steps 2–3):

1. All data writes completed (and, for unbuffered handles, EOF corrected: final tail is written padded to a sector multiple, then `FileEndOfFileInfo` trims to the exact size — works on a `FILE_FLAG_NO_BUFFERING` handle on modern NTFS; `win::file` carries a reopen-buffered fallback for stacks that refuse it, §3.2).
2. Optional `FlushFileBuffers` (only with `--flush`; see §8.6 on device caches).
3. Rename temp → final via `FileRenameInfoEx` (`ReplaceIfExists | POSIX_SEMANTICS | IGNORE_READONLY_ATTRIBUTE`, fallback chain §5.2). On the fallback path (no `Ex` support), a read-only/hidden/system rename target gets its blocking attributes cleared first — but only at this final step, so the destructive action is maximally deferred.
4. Set timestamps + attributes via `FileBasicInfo` **after** the rename. Ordering rationale: NTFS *name tunneling* can re-apply a prior file's creation time when a new name appears shortly after an old one disappears; setting timestamps after the rename overrides any tunneled value.
5. Close handle. Only now emit `file_done{action:copied}` to log/journal and increment the `copied` counter.

A crash between any two steps is recoverable: before step 3 the destination still has the old file (or nothing) plus a `.bigcp-part` temp that the journal owns; after step 3 but before step 4/5, the file has correct content and a wrong mtime → re-run reclassifies as Different → re-copies (correct, merely wasteful — and rare). The full crash matrix is in §7.2.

### 4.4 Filesystem feature matrix and degradation rules

Detected once per volume via `GetVolumeInformationW` + `GetDiskFreeSpaceExW`:

| Capability | NTFS | ReFS | exFAT | FAT32 | On unsupported destination |
|---|---|---|---|---|---|
| ADS | ✔ | ✔ | ✖ | ✖ | warn `streams_dropped`, continue |
| Sparse | ✔ | ✔ | ✖ | ✖ | expand; pre-check free space vs. logical size |
| Compression attr | ✔ | ✖ | ✖ | ✖ | silently uncompressed (attribute is storage detail) |
| EFS | ✔ | ✖ | ✖ | ✖ | warn `efs_downgrade` |
| Reparse points (links) | ✔ | ✔ | ✖ | ✖ | error per link (`links_unsupported`) with hint |
| Max file size | 16 EB | 16 EB | 16 EB | **4 GiB − 1** | files too large for FAT32 fail *pre-flight* (before any I/O) with a clear error |
| Timestamp granularity | 100 ns | 100 ns | 10 ms | 2 s (mtime) | drives tolerance, §4.1 |
| Block cloning | ✖ | ✔ (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) | ✖ | ✖ | same-volume ReFS copies become instant clones (§5.9) |

### 4.5 Path handling

- All user input paths are canonicalized once at the boundary: `GetFullPathNameW` → verify existence class → convert to extended-length form (`\\?\C:\…`, `\\?\UNC\server\share\…`). **Every** Win32 call uses the extended form; display strips it. This buys: >260-char paths, trailing dots/spaces, and reserved device names (`CON`, `NUL`, `COM1`…) all handled without special cases.
- Internal representation: `Vec<u16>`/`OsString` (native UTF-16, unpaired surrogates preserved). Conversion to UTF-8 happens only for display/log, lossy with `U+FFFD`, and the log additionally records a hex form for non-roundtrippable names (`path_raw`) so the log remains unambiguous.
- Relative paths (source-root-relative) are the tool's universal file identifier — used in logs, reports, the journal, and the destination join. Stored in an arena to keep per-file memory ~2× path length.
- Destination path length is pre-checked: `len(dst_root) + len(rel)` vs. ~32,760 UTF-16 units and per-component 255 vs. the destination FS's max component; violations fail pre-flight with hint `path_too_long`.
- Case: Windows-standard case-insensitive matching for the join (simple Unicode uppercase fold, `CompareStringOrdinal(ignoreCase)` semantics); source case is preserved when creating. Per-directory case-sensitive NTFS dirs (WSL) are a documented edge (§9, E31): the join uses case-insensitive matching regardless, which can merge two source files differing only by case — detected (duplicate join key) and reported as an error rather than silently last-writer-wins.

### 4.6 Symlinks, junctions, mount points (`/SJ /SL` semantics)

- Detected during enumeration via the reparse tag returned in the directory record — **no extra syscall**, and reparse-point directories are never recursed into (this also makes traversal cycles impossible).
- Copy mechanism: open with `FILE_FLAG_OPEN_REPARSE_POINT`, read `FSCTL_GET_REPARSE_POINT`. Then, by tag: **symlinks** are recreated via `CreateSymbolicLinkW` with `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE` (the documented path that honors Developer Mode; raw `FSCTL_SET_REPARSE_POINT` for the symlink tag has unclear Dev-Mode behavior — §3.2), reconstructing target + relative/absolute flag from the reparse buffer; **junctions/mount points and unknown third-party tags** are applied verbatim via `FSCTL_SET_REPARSE_POINT` on the new (empty) file/dir; **cloud-filter tags are never copied as reparse points** (they belong to the cloud minifilter and corrupt off-volume; those files follow the placeholder policy below). Targets are **not** rewritten (robocopy `/SL /SJ` behavior): relative links stay relative; absolute links keep pointing at their original absolute target — documented loudly in README, since a junction copied to another machine may dangle. Dangling sources are fine (we never dereference).
- Symlink creation without Developer Mode requires `SeCreateSymbolicLinkPrivilege`; when neither is available, each symlink fails with hint `enable developer mode or run elevated`. Junctions/mount points need no privilege.
- Volume mount points are treated as junctions (copied as a junction, not recursed) — matches robocopy `/SJ`.
- Cloud placeholders (OneDrive etc., `IO_REPARSE_TAG_CLOUD_*`): these are *data* files, not links. Default: copy (which hydrates/downloads on read) but count them and surface prominently ("N files were cloud placeholders and were downloaded to copy"); `--skip-cloud` excludes them instead. Rationale: VISION requires not missing files; silent mass-download requires informing the user.

### 4.7 Default exclusions (root-level OS artifacts)

When the source is a volume root (only then), the following are excluded by default, each logged as `excluded{reason:system}`: `$RECYCLE.BIN`, `System Volume Information`, `pagefile.sys`, `swapfile.sys`, `hiberfil.sys`, `DumpStack.log.tmp`. Robocopy instead spews access-denied errors on these. `--include-system` restores robocopy behavior. The summary always shows the excluded count so nothing is silently missed. User exclusions: repeatable `--exclude <glob>` matched against relative paths.

### 4.8 Source mutation during the run

The source is enumerated once; files can change before or while being copied. Policy:

- Vanished between enumeration and open (`ERROR_FILE_NOT_FOUND`): counted as `failed{category:vanished}` — distinct from real errors in the report, with hint "source changed during the run".
- Size or mtime at open time differs from the enumerated values: copy proceeds using the **open-time** values (they are re-read from the handle), and the file is flagged `changed_during_run` in the log; the run summary counts these. The copy is of a consistent open-time snapshot only if the writer cooperates — bigcp opens with `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` (maximum compatibility, robocopy-like); a concurrently-written file may be torn *as it would be with any copier without VSS*. This is documented, and `--verify` will catch mismatches (hash is computed from what was actually read).
- Grew/shrank during read: read until EOF-at-open-size; short reads → file truncated to actual data with `changed_during_run` flag.

## 5. Architecture

### 5.1 Language and platform choice

**Rust.** Reasons, in priority order:

1. **Reliability leverage.** The #1 requirement is "no bugs that corrupt/miss files". Rust eliminates whole classes (UAF, data races, buffer overruns) at compile time, and its `Result`-based error handling makes *ignored error paths* visible in review — the classic copy-tool bug (unchecked `WriteFile` return) becomes unrepresentable under our lint policy (§14.3).
2. **Zero-compromise access to Win32**: `windows-sys` provides complete, machine-generated, officially maintained (Microsoft) bindings; IOCP/overlapped I/O is fully expressible.
3. **Ecosystem fit**: `ratatui` (TUI), `blake3`/`xxhash-rust` (SIMD hashing), `crossbeam` (queues/work-stealing), `proptest`/`loom` (testing) are mature, widely deployed crates.
4. C++ was the runner-up (equal API access, but every reliability property must be earned by discipline rather than the compiler). Python is disqualified for the throughput goal (per-file syscall overhead through the interpreter, GIL vs. many-core, no clean IOCP story).

Toolchain: latest stable Rust, **MSRV pinned in `rust-toolchain.toml`** and CI-enforced; `Cargo.lock` committed; x64 target `x86_64-pc-windows-msvc`, CRT statically linked (`+crt-static`) → single self-contained `bigcp.exe`.

### 5.2 Crate layout (Cargo workspace)

```
bigcp/
├── Cargo.toml                # workspace
├── crates/
│   ├── win/                  # ALL unsafe code lives here. Thin, tested wrappers over Win32.
│   │   └── src/{handle.rs, file.rs, dir.rs, ioctl.rs, iocp.rs, path.rs,
│   │           privileges.rs, restart_mgr.rs, volume.rs, errors.rs}
│   ├── core/                 # #![deny(unsafe_code)]. All logic. No UI. Testable headless.
│   │   └── src/{model.rs, enumerate.rs, join.rs, classify.rs, schedule.rs,
│   │           engine_small.rs, engine_stream.rs, engine_os.rs, engine_clone.rs,
│   │           meta.rs, hashpipe.rs, journal.rs, resume.rs, verify.rs,
│   │           devprofile.rs, stats.rs, bottleneck.rs, hints.rs,
│   │           logsink.rs, report.rs, faults.rs, options.rs}
│   ├── tui/                  # ratatui dashboard + report browser. Renders core's state snapshots.
│   ├── cli/                  # binary crate `bigcp`: arg parsing (clap), wiring, --plain output.
│   └── testkit/              # binaries: gen (tree generator), check (the oracle), chaos (kill-loop harness).
└── docs/                     # §14: SEMANTICS.md, DESIGN.md, TESTING.md, MAINTENANCE.md, ERRORS.md,
                              #      adr/, schemas/log.v1.schema.json, schemas/report.v1.schema.json
```

Dependency rules (CI-enforced via `cargo-deny`): `cli → {core,tui}`, `tui → core` (read-only state types), `core → win`, `testkit → {core,win}` *only for the fault-injection driver; the oracle (`check`) must not link `core`'s copy logic* — it is an independent implementation (§12.2).

The `win` crate exposes only safe, documented functions returning `io::Result<T>` with the Win32 error preserved (`raw_os_error`). Every wrapper has a `# Safety`-discharging comment and a unit test. The rest of the workspace is `#![deny(unsafe_code)]`.

Key fallback chains implemented in `win` (feature-detect at runtime, cache the answer):
- Rename: `FileRenameInfoEx(POSIX | ReplaceIfExists | IgnoreReadonly)` → `FileRenameInfo(ReplaceIfExists)` with manual attr-clear → `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`.
- Enumeration info class: `FileIdExtdDirectoryInfo` (has reparse tag) → `FileFullDirectoryInfo` (reparse tag via attribute + `FindFirstFileExW` fallback).

### 5.3 Process overview and threading model

```
                         ┌────────────────────────────────────────────────┐
                         │                COORDINATOR (1 thread)          │
                         │  run state machine · counters · circuit        │
                         │  breakers · journal writer · tick scheduler    │
                         └───────┬──────────────────────────────┬─────────┘
        control/stats            │                              │ state snapshots (watch channel)
                                 ▼                              ▼
┌──────────────┐   dir tasks  ┌──────────────┐   copy items   ┌─────────┐
│ ENUMERATION  │─────────────▶│  SCHEDULER   │───────────────▶│   TUI   │ (or --plain printer)
│ pool (2–16   │  (bounded    │ classify ·   │  (bounded      └─────────┘
│ threads,     │   queue      │ size classes │   per-class
│ work-steal)  │   100k)      │ · locality   │   queues)
└──────────────┘              └──────┬───────┘
        each dir task: enumerate      │
        src dir + join dst dir        ├────────────► SMALL-FILE ENGINE: N workers, buffered I/O,
        (§5.6)                        │              one file per worker at a time (§5.8)
                                      │
                                      ├────────────► STREAMING ENGINE: IOCP + completion threads,
                                      │              unbuffered overlapped, ring per stream (§5.9)
                                      │
                                      └────────────► META ENGINE: links, dirs, attr-repairs,
                                                     post-order dir timestamps (§5.10)
   HASH pool (0–2 threads, only when hashing)   LOG thread (serializes JSONL writes)
```

Thread inventory (defaults; every count overridable, all bounded):

| Pool | Count (default) | Work |
|---|---|---|
| Coordinator | 1 | owns run lifecycle, journal, counters, breakers |
| Enumeration | SSD src: `min(16, cores)`; HDD src: 2 | directory walk + destination join |
| Small-file workers | SSD⇄SSD: `min(64, 4×cores)`; HDD involved: 4–8 (§8.2) | whole small-file copies, sync buffered I/O |
| Small-file finalizers | 2–4 | rename/meta/close off the hot path (§5.8 — Defender close-path scans) |
| IOCP completion | 2–4 | streaming engine reads/writes/hash-inline |
| Hash | 0 (off) / shares IOCP threads for xxh3; 2 dedicated for blake3 | §5.11 |
| Log sink | 1 | batched JSONL writes (own file handle, buffered + periodic flush) |
| TUI | 1 | 30 fps max render of state snapshots; input handling |

**Rule: no unbounded queues anywhere.** Every channel is bounded; producers block (backpressure) rather than balloon memory. Global buffer memory budget: `min(25 % of physical RAM, 8 GiB)`, floor 256 MiB, override `--mem`.

**No async runtime.** Tokio is deliberately not used: file I/O on Windows under tokio is thread-pool-simulated anyway, and the streaming engine wants raw IOCP with hand-controlled queue depths. Threads + channels + one IOCP is simpler, faster, and easier to reason about. (ADR-0003.)

### 5.4 Data flow and the work item model

`model.rs` defines the single source of truth:

```rust
struct FileEntry {           // produced by enumeration (src) and join (dst side)
    rel: RelPath,            // arena-interned UTF-16 relative path
    size: u64,
    mtime: i64, ctime: i64, atime: i64,   // FILETIME units
    attrs: u32,
    reparse_tag: Option<u32>,
    streams_hint: StreamsHint // NotChecked | None | Some (filled lazily)
}
enum CopyItem {              // scheduler output
    SmallFile { src: FileEntry, dst_state: DstState },   // DstState: New | Replace{old_attrs} | …
    LargeFile { src: FileEntry, dst_state: DstState },
    Reparse   { src: FileEntry, dst_state: DstState },
    MetaFix   { src: FileEntry },                        // attribute repair (§4.1)
    DirCreate { rel: RelPath, attrs: u32 },
}
enum Outcome { Copied{bytes,ms,hash}, SkippedSame, MetaFixed, Failed{err}, Excluded{why}, NotAttempted{why} }
```

Every `CopyItem` terminates in exactly **one** `Outcome`, delivered to the coordinator. This is the backbone of counter reconciliation (§7.3).

### 5.5 Device profiler

Runs once at startup (per distinct volume), before any copy I/O; results go into the log, the report, the Devices tab, and the tuning tables.

1. Volume: `GetVolumePathNameW` → `GetVolumeInformationW` (FS name, flags, max component) → `GetDiskFreeSpaceExW` (free space) → `GetDiskFreeSpaceW` (cluster size) → `GetDriveTypeW`.
2. Volume→disk: open `\\.\C:` with `dwDesiredAccess = 0` (query-only; works without admin) → `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` → physical disk number(s).
3. Disk: open `\\.\PhysicalDriveN` (access 0) → `IOCTL_STORAGE_QUERY_PROPERTY`:
   - `StorageDeviceSeekPenaltyProperty` → HDD vs. SSD,
   - `StorageAdapterProperty` → `BusType` (USB / NVMe / SATA / RAID…), `MaximumTransferLength`, `MaximumPhysicalPages`,
   - `StorageAccessAlignmentProperty` → logical/physical sector size (4Kn vs 512e),
   - `StorageDeviceProperty` → vendor/model strings (for the Devices tab),
   - `IOCTL_STORAGE_GET_HOTPLUG_INFO` → removal policy (Quick removal vs Better performance; §8.6) and `IOCTL_DISK_GET_CACHE_INFORMATION` → device write-cache state — both surfaced in the Devices tab and hints, never modified.
   - Bus refinement: `BusTypeUsb` + tiny `MaximumTransferLength` (~64 KiB) indicates a BOT (non-UASP) device → force QD 1 and clamp chunk to MTL (§3.4).
4. Same-spindle detection: source and destination extent lists intersect → same-physical-disk policy (§8.3).
5. **Fallbacks:** USB bridges routinely fail or lie on these IOCTLs. Any failure → conservative *"unknown"* profile: treated as SSD-like for correctness but with moderate parallelism (QD 2, 4 small-workers/side, 4 MiB chunks), logged as `device_profile{confidence:low}`, and the auto-tuner (§8.5) adjusts from measurements. Sector size fallback: `GetDiskFreeSpaceW` logical sector, alignment safety net = 4096.

The profiler also captures `GlobalMemoryStatusEx` (RAM budget) and checks free space: if `discovered_bytes_so_far − skipped_bytes > free_space` at any point, a prominent warning fires *early* (enumeration streams, so this check re-arms as discovery grows) — not first at write failure.

### 5.6 Enumeration and destination join

Design goals: saturate metadata IOPS on SSDs, avoid seek-thrash on HDDs, never stat destination files one-by-one, start copying while still discovering.

- **Unit of work = one directory.** A work-stealing pool (crossbeam deque) processes directory tasks. Each task:
  1. Opens the source dir (`FILE_LIST_DIRECTORY | SYNCHRONIZE`, `FILE_FLAG_BACKUP_SEMANTICS`) and enumerates with `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` into a 256 KiB buffer, looping until done. One handle, few syscalls, and each record carries size/times/attrs/reparse-tag plus the 128-bit file ID — everything the classifier needs, **no per-file `CreateFile`/stat**. (The file ID also feeds the hard-link counter: duplicate IDs seen across the walk are counted for the report at ~16 bytes/file of set memory, no extra I/O.)
  2. Opens the *destination* twin dir if it exists and enumerates it the same way into a per-directory hash map (case-folded name → `FileEntry`). This is the **join**: one listing replaces N per-file existence checks. Missing dst dir → all entries are *New* (and a `DirCreate` item is emitted first).
  3. Classifies every source entry (§4.1) against the join map; emits `CopyItem`s to the scheduler queue; emits `Extra` records for unmatched dst entries; pushes child directory tasks (excluding reparse-point dirs, which become `Reparse` items).
  4. Registers the directory in the **dir-completion tracker** (§5.10) with its pending-child count.
- Deep trees: purely iterative — the explicit task deque is the recursion stack; depth 10,000 is just 10,000 queued tasks.
- HDD source: pool clamped to 2 threads and enumeration is *paced* — the scheduler queue bound (100k items) plus a lower OS I/O priority on enumeration threads keeps metadata seeks from starving streaming reads (§8.3).
- Progress semantics: totals are "discovered so far"; the TUI shows a discovery ticker until enumeration drains, and the ETA model (§6.7) reports a lower bound until then.

### 5.7 Scheduler

Receives classified `CopyItem`s, dispatches to engines. Policies:

- **Size classes:** `small` < 4 MiB ≤ `large` (`--large-threshold` tunable; rationale §8.1). Reparse/meta items go to the meta engine.
- **Locality:** small-file items are batched per source directory (a batch = up to 256 files from one dir) and a batch is assigned to one worker — preserves physical locality on HDDs and keeps dst-dir handles warm.
- **Interleaving:** large-file streams and small-file batches run concurrently *only when the device profile says it is free* (SSD both sides). If either side is an HDD: one large stream at a time, small-file engine throttled to 4 workers while a stream is active (avoids seek storms), alternating-burst mode when source and destination share a spindle (§8.3).
- **Priorities:** directory creation items run ahead of their files (a file cannot be written before its parent exists — the tracker guarantees ordering); otherwise FIFO within class to keep discovery order ≈ physical order.
- **Circuit breakers** (coordinator-owned, scheduler-enforced): destination-full and device-gone conditions stop dispatch (§5.13); pause (`p` key) drains in-flight and holds dispatch.

### 5.8 Small-file engine

One file per worker at a time, synchronous buffered I/O — the OS cache manager is *good* at this (write-behind batches small writes; unbuffered I/O would force per-file alignment handling and sync flushes and is measurably slower for < ~1 MiB files; §3.2, §8.1).

Per file: `CreateFileW(src, GENERIC_READ, share RWD, SEQUENTIAL_SCAN)` → read whole file into a pooled 4 MiB-max buffer → `CreateFileW(dst)` (direct final name if New; temp if Replace) → single `WriteFile` → ADS pass if hinted (§4.2) → hand off to the **finalizer stage**. Zero-byte files skip the read/write. xxh3 (when hashing) runs inline on the buffer — it is ~10× faster than the copy itself and never the bottleneck.

**Finalizer/closer stage (2–4 threads):** completes the §4.3 protocol — rename if Replace, timestamps+attrs via handle, `CloseHandle` — and only then emits the `Outcome`. Rationale: Defender and other minifilters scan *synchronously in the post-write close path* (>100 ms worst case per file); moving close/finalize off the copy workers is a proven >3× win on many-small-file workloads (Mercurial/rustup technique, §3.2). Reliability is unaffected: `Outcome::Copied` is still emitted only after a successful close (buffered-write errors can surface at close — they are caught here and become `Failed`), preserving I4. The stage's queue is bounded; workers block when finalizers fall behind.

Syscall budget per small file (the metric this engine is optimized for): 2 × `CreateFileW`, 1 × `ReadFile`, 1 × `WriteFile`, 1 × `SetFileInformationByHandle`, 2 × `CloseHandle`, +1 `FindFirstStreamW` on NTFS sources = **8–9**, vs. robocopy's ~15+ (which re-stats and reopens for attributes). Anything added to this path needs a benchmark justification. If profiling shows the AV filter still dominating despite deferred closes, `--engine os` (kernel copy with its Defender filter-skip, §3.2) is the documented alternative — the Hints tab suggests it when the signature is detected.

### 5.9 Streaming engine (large files)

The throughput core. One **stream** = one large file being copied. Structure:

- Source handle: `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED`; destination: same + created via temp-name protocol. Both attached to the single IOCP.
- Per stream, a **ring of aligned chunk buffers** (default chunk 8 MiB, clamped to device MTL; ring 32 chunks = 256 MiB, adaptive within the global memory budget). Read side fills the ring at source-QD; each completed read (a) is hashed inline if hashing is on, (b) becomes eligible for writing; write side drains at destination-QD, **issue order = file order** (completions may reorder; a small in-order commit window bounds NTFS valid-data-length zero-fill cost, §8.1).
- Mismatched speeds are absorbed by the ring: fast NVMe source fills it during USB destination stalls (SLC-cache dips, bridge hiccups); ring-full applies backpressure to reads — RAM is used *judiciously*: enough to smooth bursts, never "read the whole file into 64 GB RAM" (which robs the page cache, delays failure discovery, and buys nothing once the ring covers the bandwidth-delay product).
- Destination preallocation: `FileAllocationInfo` (round up to cluster) at open → contiguity and no mid-write allocation stalls; exact `FileEndOfFileInfo` at completion. `SetFileValidData` is **off by default** (security: exposes stale disk contents if a crash precedes the writes; requires admin + `--fast-prealloc` to enable; with in-order writes its benefit is small anyway).
- Watermark: the highest byte offset below which every write has completed, journaled every 256 MiB (§5.12) for partial resume.
- Concurrent streams: 1 if any HDD side; 2 default SSD⇄SSD; auto-tuner may open up to 4 when both sides are NVMe-class and per-stream throughput scales (§8.5). Files 4–64 MiB share stream slots in batches so slot count, not file count, bounds concurrency.
- **ReFS fast path:** same volume + `FILE_SUPPORTS_BLOCK_REFCOUNTING` → `engine_clone.rs` issues `FSCTL_DUPLICATE_EXTENTS_TO_FILE` in ≤1 GiB extents — near-instant copies on Dev Drive; any failure falls back to streaming transparently.

### 5.10 Meta engine and directory timestamps

- Executes `DirCreate` (create + attrs), `Reparse` (§4.6), `MetaFix` (attribute repair) items.
- **Dir-completion tracker:** every directory holds a countdown of (children dirs not yet complete + own pending file items). When it hits zero, a `DirStamp` item (set the directory's timestamps+attrs from the source entry) is emitted — a rolling **post-order pass** that requires no end-of-run tree walk and works with streaming enumeration. The destination root is stamped last, at run end.
- Existing destination directories get their timestamps/attrs corrected only if they differ (no-op writes avoided, §8.4).

### 5.11 Hash pipeline

- Algorithms: `xxh3-128` default (SIMD, >20 GB/s/core — corruption detection), `blake3` optional via `--hash blake3` (cryptographic, ~1–7 GB/s/core, internally multithreaded; capped at 2 threads to protect copy CPU headroom).
- Hashing is **on** only when requested (`--verify`, `--hash-log`) — the default copy path spends zero CPU on it. When on, hashes are computed from the same buffers the engines already hold (no extra reads), stored per file in log + journal + report.
- The verify pass and `bigcp verify` (§5.17) reuse the same engines/profiler for reading — one read path in the codebase, exercised by all features.

### 5.12 Journal and resume

State directory: `%LOCALAPPDATA%\bigcp\state\<16-hex of SHA-256(norm_src|norm_dst)>\` (override `--state-dir`). Contains `journal.jsonl` plus per-run `run-<ts>.log.jsonl` / `run-<ts>.report.json` (defaults; `--log/--report` may point elsewhere, including the source drive — the *only* writes ever allowed toward source, per VISION).

**Resume model — two layers:**

1. **Idempotent re-run (the workhorse).** Completed files are *not* trusted from the journal — a re-run simply re-enumerates and the skip heuristic (§4.1) classifies them Same in microseconds per file with zero data I/O. This is robust against anything that happened to the destination between runs (user deletions, other tools) — the journal can never go stale in a harmful direction. Cost: re-enumeration, which the parallel walker makes cheap (millions of entries/minute on SSD).
2. **Partial large-file resume (the money-saver).** For streaming-engine files, the journal records `{rel, temp_name, src_size, src_mtime, watermark, hash_state?}` every 256 MiB. On start, bigcp loads the journal; when the classifier hits a file whose journal entry matches the *current* source (size+mtime) and whose temp file exists with plausible size: reopen temp, and (a) if hashing: re-read the temp's `[0, watermark)` unbuffered, rebuilding the hash state — which *simultaneously verifies* the resumed prefix; (b) if not hashing: trust the watermark. Continue from the watermark. Any mismatch (source changed, temp missing/short, journal torn) → restart that file from zero. A 900 GB file killed at 80 % costs ~20 % + (with hashing) one prefix read — not 100 %.

Journal hygiene: append-only JSONL; each record CRC-tagged; a torn final line (crash mid-append) is detected and dropped; the journal is flushed (`FlushFileBuffers`) on every watermark record and every 2 s otherwise. Temp files are deleted **only** when the journal proves ownership (name embeds run-id, entry exists); orphan `.bigcp-part` files without journal proof are *reported with a cleanup hint, never auto-deleted*. `--fresh` ignores journal and restarts (after the same safety rule for temps).

### 5.13 Error handling

- Every failure produces `Outcome::Failed{err}` carrying: Win32 code, message, the operation (open-src / read / create-dst / write / rename / set-meta / …), and the relative path. Nothing is retried by default (`/R:0 /W:0`); `--retry N --retry-wait MS` exist for flaky-bus users, default 0.
- **Classification** (`errors.rs`, table-driven — the single place error codes are interpreted; ERRORS.md is generated from it, §14):

| Category | Example codes | Hint shown |
|---|---|---|
| `permissions` | 5 `ACCESS_DENIED` | "Run elevated or use --backup-mode; check ACLs on <path>" |
| `locked` | 32 `SHARING_VIOLATION` | "In use by **<process names via Restart Manager>** — close it and re-run" |
| `path` | 3, 206, name too long | "Enable Win32 long paths / shorten destination root" |
| `space` | 39/112 `DISK_FULL` | "Destination full: need ~X GB more (from discovery)" |
| `media` | 23 `CRC`, 1117 `IO_DEVICE` | "Source device reported hardware read errors — check the drive (chkdsk / SMART)" |
| `device_gone` | 433, 1167 `DEVICE_NOT_CONNECTED` | "Device disconnected — reconnect and re-run to resume" |
| `fs_limit` | FAT32 4 GiB, no-reparse-support | per-case hint (§4.4) |
| `vanished` | 2 at open after enumeration | "Source changed during the run" |
| `cloud` | hydration failures (0x8007016A family) | "OneDrive placeholder could not be downloaded — check connectivity / --skip-cloud" |
| `internal` | anything unexpected | "This is a bigcp bug — please file the log" (and exit code 6 if an invariant broke) |

- **Restart Manager integration:** on the first `SHARING_VIOLATION` per path, `RmStartSession/RmRegisterResources/RmGetList` resolves *which process* holds the file; the hint and the report name it. Cost is paid only on error paths.
- **Circuit breakers** (prevent 100,000-error cascades): `device_gone` on N=3 consecutive ops → pause the run; TUI offers "reconnect and press r"; headless → abort with exit code 4 (resumable). `space` → stop dispatching writes; remaining items become `NotAttempted{dest_full}`; run ends with the precise shortfall in the report. Error *storms* of one category are rate-limited in the TUI (full detail always lands in the log).
- Error tally is live in the TUI (§11): counts by category × top-level folder, navigable to per-file detail.

### 5.14 Stats and bottleneck analysis

- Every I/O records `(device, kind, bytes, submit→complete latency)` into per-thread accumulators, drained by the coordinator each 500 ms tick into: per-device throughput, IOPS, mean/p99 latency, queue occupancy, and **busy fraction** (share of the tick with ≥1 op in flight).
- Verdict per window and for the whole run: `source-bound` (src busy >90 %, dst <60 %), `dest-bound` (inverse), `balanced`, `cpu-bound` (hash pool saturated), `discovery-bound` (copy queues empty while enumeration active), `breaker-paused`. The report stores the timeline (downsampled to ≤3,600 points) plus phase extremes: fastest/slowest 5-minute segments with their dominant folders.
- "**Maximum possible throughput**" is reported honestly as *the best sustained 5 s window observed on the bottleneck device during this run* (plus, when `--probe` is set, a 256 MiB sequential read probe of the source at startup — read-only, so always safe). Efficiency = run average ÷ that peak. The report explains the number's provenance — no fabricated theoretical maxima.
- Pattern detectors emit hints (§11 Hints tab): sustained dst-throughput cliff after tens of GB (typical SLC-cache exhaustion / SMR behavior → "expected on this class of drive, not a bigcp or cable problem"), high dst latency with low throughput on small files (AV filter → "consider a Defender exclusion for the destination during bulk restore"), `discovery-bound` verdict (→ "source metadata is the limit; nothing to tune"), FAT tolerance skips (→ "--dst-tolerance").

### 5.15 Logging and reporting

Split by audience: the **log** (JSONL, event per line, complete) is for machines and post-mortems; the **report** (single JSON, aggregated) is for humans via `bigcp report` and for dashboards. Formats in §10; versioned schemas shipped in `docs/schemas/` and treated as public API (additive changes only within v1).

### 5.16 CLI

Full grammar in §10.1. Subcommands: `bigcp SRC DST [flags]` (copy), `bigcp verify SRC DST [--quick]`, `bigcp report FILE`. Robocopy flag mapping in Appendix A.

### 5.17 Verify mode

- `--verify[=copied|all]` (post-copy pass, same run): waits for copy completion, then re-reads **destination** files unbuffered (defeating the OS cache — a read-back served from RAM would verify nothing) and compares against the hash computed during the copy read. `copied` (default) verifies files this run wrote; `all` additionally reads *both* sides of skipped files (they were never read this run, so both hashes are needed). Scheduling reuses the engines: parallel small reads, streamed large reads, per-device QD — verify throughput ≈ copy read throughput.
- `bigcp verify SRC DST` (standalone): enumerate+join both trees; report missing/extra/type-mismatch; `--quick` stops at metadata (size+mtime), default also hashes both sides (full read of both trees). If a prior run's journal hash cache matches a source file (size+mtime unchanged), the source re-read is skipped and the cached hash is used — an honest optimization, noted per file in the log.
- Verify results land in the same report structure (`verify` section): pass/fail counts, mismatched files (these are *serious* — flagged in red, with the guidance that a mismatch after a clean copy indicates hardware/FS problems).
- Honesty note (documented in README and report): unbuffered read-back defeats the OS cache but not the drive's internal DRAM cache entirely; verify catches bus/FS/logic corruption reliably, media decay only as well as the drive lets it. `--verify` costs one extra read of the copied bytes — the report prices it in advance in the plan line.

## 6. Key algorithms

Pseudocode is normative for control flow; error handling (every call can fail → `Outcome::Failed`) is elided for readability but mandatory in implementation.

### 6.1 Run orchestration (coordinator)

```
run(opts):
  paths   = canonicalize(opts.src, opts.dst)          # §4.5; fail fast on nonsense (dst inside src → fatal)
  devices = profile(paths)                            # §5.5
  journal = Journal::load_or_new(state_dir(paths))    # §5.12
  plan    = announce(devices, journal.resumables())   # log run_start; TUI up
  spawn enumeration(paths, opts) → scheduler → engines
  loop on events:
    Outcome(item, outcome)  → counters.apply; journal.maybe_record; log.emit
    Tick(500ms)             → stats.roll; tui.publish; eta.update; freespace.recheck
    BreakerTrip(kind)       → pause_or_abort(kind)    # §5.13
    UserKey(p/r/c/q)        → pause/resume/cancel_graceful/cancel
  until enumeration done ∧ all queues drained ∧ in-flight == 0
  meta.stamp_root_dir()
  if opts.verify: run_verify_pass()                   # §5.17
  counters.reconcile_or_die()                         # §7.3 — invariant breach ⇒ exit 6
  report.write; journal.finalize; summary.print
```

Cancellation: first `c`/Ctrl-C = graceful (stop dispatch, let in-flight chunks finish, journal watermarks, write report, exit 3 — resumable); second = hard stop (temps remain; journal already safe by construction).

### 6.2 Directory task (enumeration + join)

```
process_dir(task):                              # runs on the work-stealing pool
  src_entries = enum_dir(task.src_handle)       # FileIdExtdDirectoryInfo, 256 KiB batches
  dst_map     = if exists(dst): enum_dir(dst) into casefold-keyed map else empty
  if !exists(dst): emit DirCreate(task.rel, src_attrs)
  for e in src_entries:
    if excluded(e): count excluded; continue
    match e:
      Dir if !reparse   → tracker.add_child(task); push_task(child)
      Dir|File reparse  → emit Reparse(e, dst_state(dst_map.take(e)))
      File              → emit classify(e, dst_map.take(e))     # §4.1 → Small/Large/MetaFix/skip
  for leftover in dst_map: count extra; log extra{rel}
  tracker.dir_enumerated(task)                  # may release DirStamp §5.10
```

### 6.3 Streaming copy state machine (per stream)

```
states: Opening → Reading⇄Writing (concurrent) → Tail → Finalizing → Done|Failed
Opening:  open src (NOBUF|OVL); create dst temp (NOBUF|OVL); FileAllocationInfo(dst, roundup(size))
          if resume_candidate: validate journal vs src(size,mtime) and temp; seek watermark (§5.12)
Reading:  while ring has free chunk ∧ src_qd < QD_src: post ReadFile(offset+=chunk)
OnReadDone(c):  hash_inline(c) if hashing; mark c ready
Writing:  while next in-order ready chunk ∧ dst_qd < QD_dst: post WriteFile(c)   # in-order issue
OnWriteDone(c): advance watermark if contiguous; every 256 MiB → journal.watermark; recycle c
Tail:     last chunk padded to sector multiple; after its write: FileEndOfFileInfo(exact size)
Finalizing: [--flush → FlushFileBuffers]; rename temp→final (ReplaceIfExists, clear RO on target if needed);
            FileBasicInfo(timestamps+attrs)     # after rename — tunneling, §4.3
            close both; emit Copied{hash}
```

I/O errors in any state → cancel outstanding ops for the stream, delete nothing (temp stays for resume unless the error implies corruption of the temp itself, in which case journal entry is dropped so resume restarts it), emit `Failed`.

### 6.4 ETA model

Two online rates, EWMA over 30 s half-life: `B` bytes/s (data) and `F` files/s (per-file overhead, measured on small-engine completions). Remaining work `(bytes_r, files_r)` (known exactly once discovery drains; lower bound before). `ETA = max(bytes_r/B, files_r/F)` — the max, because the two costs largely overlap in the pipelines. While discovering: display `≥ ETA` with a "discovering…" marker. Displayed via 10 s median filter so the number doesn't flap.

### 6.5 Auto-tuner

Applies only to the streaming engine (small-file engine tuning is static per profile). Every 5 s, per device: if busy < 85 % and the ring is not starved, raise QD one step (cap 16) or chunk size one step (cap min(16 MiB, MTL)); if p99 latency > 4× the 60 s baseline, step down (bridges and HDDs congest). Changes are logged (`autotune{...}`) and visible in the Devices tab, and the report records the final settled values as the "suggested flags for next run" hint. `--qd/--chunk` pin values and disable the tuner.

## 7. Reliability and failure-mode design

### 7.1 Invariants (each maps to tests, §12)

| # | Invariant | Enforcement |
|---|---|---|
| I1 | Source is opened with `GENERIC_READ` only, always | single choke-point `win::file::open_source()`; grep-guard test asserts no other source-open site exists |
| I2 | No code path deletes a destination file bigcp did not create this/previous run | the only `delete` wrapper takes a `JournaledTemp` token type — unconstructible for arbitrary paths |
| I3 | Replacement never truncates in place | `Replace` items have no direct-write branch; type system: `DstState::Replace` yields only temp-name create handles |
| I4 | `copied` is reported only after data+EOF+rename+meta complete | `Outcome::Copied` constructed solely by the finalize step |
| I5 | Timestamps are set only after data is complete (torn files always detectable) | finalize ordering; chaos suite asserts no dest file ever has (src mtime ∧ wrong content) |
| I6 | Counters reconcile: `discovered = copied + skipped(＋meta_fixed) + failed + excluded + not_attempted`, in files and bytes | coordinator assert at run end; violation ⇒ exit code 6 + `internal` error in report |
| I7 | Every failure is logged with path + code + operation | `Outcome::Failed` carries all three by construction; log sink is infallible-by-buffering (log-write failure itself → stderr + exit 6) |
| I8 | Journal never causes a skip that metadata wouldn't also justify | journal used only for partial-resume + hash cache (§5.12), never for "done" skips |
| I9 | Bounded memory: all queues bounded, buffer pool ≤ budget | types: only bounded channels exist in `core`; pool asserts |
| I10 | The tool never writes to the source tree (log/report paths explicitly excepted when user-pointed there) | path guard in `win` write-open wrapper: refuses paths under src root unless whitelisted at startup |

### 7.2 Crash matrix (FMEA)

Process killed at any point — destination state and next-run behavior:

| Kill point | Destination state | Next run |
|---|---|---|
| during enumeration | untouched (reads only) | clean restart, trivially |
| dir created, files pending | empty/partial dir, correct attrs | dirs are idempotent (`exists` → reuse); files classified New |
| small file mid-write (New) | torn file, **current** mtime | mtime≠src → Different → recopied |
| small file mid-write (Replace) | old file intact + stale `.bigcp-part` | old file still Different → re-copy via new temp; orphan temp reported (journal owns → deleted) |
| stream mid-write | temp at watermark+ε, journal ≤ watermark | resume from watermark (validated); or restart file if source changed |
| after rename, before meta | correct content, wrong mtime | Different → recopied (wasteful, correct); window is microseconds |
| after meta, before journal/log line | fully correct file, no record | classified Same → skipped; counters correct for *this* run |
| mid-journal-append | torn last line | CRC check drops it; affected file falls back one watermark or restarts |
| during verify pass | copy already complete | re-run `--verify=all` re-verifies from scratch |

Power loss (vs. process kill) adds device-cache risk: without `--flush`, data acknowledged by the drive may sit in its volatile cache. Unbuffered writes already bypass the *OS* cache; `--flush` adds `FlushFileBuffers` per file + a volume flush at run end for the paranoid. The README states this plainly rather than pretending immunity.

### 7.3 Counter reconciliation

The coordinator is the single owner of counters; engines report only via `Outcome`. At end of run the I6 equation must hold **exactly** — files and bytes. Any imbalance means a code bug (an item dropped without an outcome, or double-counted): the run is marked `integrity: FAILED` in report+log, exit code 6, and the summary tells the user to trust the log over the summary and file a bug. This turns "the tool silently missed files" — the worst possible failure — into a loud, detectable one. The chaos suite (§12.4) hammers this invariant specifically.

### 7.4 What bigcp will never do (design-level safety recap)

No delete/mirror mode exists; no in-place truncation exists; no source-write path exists (I1/I10); no "quiet skip" exists (every non-copied source file appears in exactly one accounted category); no unverifiable success claims exist (I4–I6). These are structural properties of the code, not policies — reviewers must reject PRs that weaken them (checklist, §14.3).

## 8. Performance engineering

### 8.1 Why two engines and where the 4 MiB threshold comes from

Per-file *fixed* cost (open+create+meta+close, AV filter scans on create/close) dominates below ~1 MiB on NTFS — parallelism across files is the only lever, and buffered I/O lets the cache manager coalesce flushes. Per-byte cost dominates above a few MiB — there, eliminating cache-copy overhead (unbuffered) and keeping the device queue full (overlapped, QD>1, big requests) are the levers, and per-file setup cost is noise. 4 MiB is the crossover measured repeatedly by tools in this space (FastCopy's threshold is the same order); it is a `--large-threshold` flag and M3 includes a benchmark to confirm/adjust the default on current hardware.

NTFS VDL note: with unbuffered out-of-order writes, a write completing beyond the valid-data-length forces zero-fill of the gap. The streaming engine issues writes in file order with a bounded in-flight window (QD × chunk), so the gap never exceeds that window; measured cost ≈ 0. `SetFileValidData` (privilege-gated) exists behind `--fast-prealloc` for benchmark parity but is off by default (§5.9).

### 8.2 Default tuning table (initial values; auto-tuner adjusts; all overridable)

| Device class (per side) | Stream QD | Chunk | Concurrent streams | Small-file workers | Enum threads |
|---|---|---|---|---|---|
| NVMe (internal) | 8 | 8 MiB | 2–4 | `min(64, 4×cores)` | `min(16, cores)` |
| SATA SSD | 4 | 8 MiB | 2 | 32 | 8 |
| USB SSD (UASP) | 4 | 4–8 MiB (≤ MTL) | 2 | 16 | 8 |
| HDD (any bus) | 2 | 16 MiB | 1 | 4 (src HDD) / 8 (dst HDD) | 2 (src side) |
| Unknown/low-confidence | 2 | 4 MiB | 1 | 4 | 4 |

Effective config = min/merge of the two sides' rows; the chosen values are logged and shown in the Devices tab.

### 8.3 HDD-specific policies

- Sequential is everything: large chunks (16 MiB), QD 2 (just enough to hide host latency; NCQ adds little for pure sequential), one stream at a time, small-file batches kept directory-local (directory locality correlates with physical locality).
- **Same-spindle copy** (src and dst on one HDD): interleaved read/write is seek death. The stream engine switches to **alternating bursts**: fill the ring with ~256 MiB of reads, then drain it with writes, repeat — amortizing each head sweep across a quarter-gigabyte. Burst size = ring size; measured seek cost makes 256 MiB the sweet spot (documented benchmark in M3).
- Enumeration on an HDD source runs at 2 threads with low I/O priority and pauses while a stream burst is active on the same spindle.

### 8.4 "No unnecessary I/O" checklist (each is a review item)

- No per-file destination stat (the join, §5.6). No re-open for metadata (handle-based, §4.3). No directory re-walk for timestamps (tracker, §5.10). No hash unless requested (§5.11). No write of identical attributes (§5.10). No journal "done"-records for small files when hashing is off (the skip heuristic subsumes them; journal then holds only watermarks + run header). No log flush storm (batched, 2 s cadence). No TUI-driven I/O (render from in-memory snapshots only).

### 8.5 Auto-tuner and probes

§6.5 covers the loop. Additional probe: `--probe` performs a 256 MiB sequential *read* probe of source (safe, read-only) before starting to seed the "max possible" baseline; destination ceilings are only ever learned from real writes (no destructive write probes — a benchmark subcommand may come post-v1, ADR-0011).

### 8.6 Write caching and removal safety

Windows (≥1809) defaults external drives to "Quick removal" (OS write caching off) — small-file copies feel that as per-file latency; the profiler reads the policy via `IOCTL_STORAGE_GET_HOTPLUG_INFO` (+ `IOCTL_DISK_GET_CACHE_INFORMATION` for the device cache) and the Hints tab explains the trade-off rather than changing system settings. Durability layering (README states this plainly): unbuffered writes bypass the *OS* cache; the *drive's* DRAM cache is only emptied by a real cache-flush command — `FlushFileBuffers` is universally honored, `FILE_FLAG_WRITE_THROUGH`/FUA is ignored by some USB bridges (§3.4) — so `--flush` uses `FlushFileBuffers`, and without it the standard "Safely Remove" flow covers the device cache. On exFAT destinations (no metadata journal — interrupted metadata updates can corrupt the volume, §3.4), bigcp flushes the journal-relevant state more aggressively and the Hints tab notes the fragility.

### 8.7 Expected performance targets (gates in §12.6)

Against robocopy with its best flags per workload (`/MT:32`, `/J` where it helps) on the same hardware: ≥3× on 1M×4 KiB SSD→SSD; ≥1.3× on 10×20 GiB NVMe→USB-SSD; ≥95 % of a raw `diskspd` sequential baseline for the big-file case; enumeration ≥500 k entries/min on SATA-SSD-class metadata. These are gate *targets* validated and re-baselined in M3 (§13).

## 9. Edge-case catalog

Each case: expected behavior + the test that pins it (§12 IDs). This table is the checklist for `testkit gen` scenarios.

| ID | Case | Behavior |
|---|---|---|
| E01 | 0-byte files | created, metadata copied, no data I/O |
| E02 | file size exactly at threshold / chunk / sector boundaries (±1) | correct via tail-trim protocol (§4.3); property-tested sizes |
| E03 | file > 4 GiB onto FAT32 | pre-flight fail, `fs_limit`, hint |
| E04 | path > 260 chars; component = 255; total near 32 760 | works via `\\?\`; over-limit → pre-flight `path_too_long` |
| E05 | trailing dot/space in names; reserved names (`CON`, `NUL.txt`) | copied verbatim via `\\?\` |
| E06 | Unicode: emoji, surrogates (incl. unpaired), RTL, combining, NFC/NFD differences | byte-preserved (no normalization ever); NFC≠NFD are distinct files |
| E07 | hidden/system/readonly files and dirs | attrs copied; readonly *destination* replaced via deferred attr-clear (§4.3) |
| E08 | ADS: multiple streams, large streams, stream-only size | copied on NTFS/ReFS dst; warned+counted on exFAT/FAT |
| E09 | sparse 1 TB file, 1 % allocated | allocated-ranges copy; sparse preserved; non-sparse dst → space pre-check |
| E10 | NTFS-compressed / EFS-encrypted source | content copied; attr best-effort re-applied (§4.2) |
| E11 | symlink file/dir: relative, absolute, dangling | reparse copied verbatim; no recursion; privilege hint if needed |
| E12 | junction, volume mount point | copied as junction; never recursed |
| E13 | hard-linked pairs | copied as independent files; counted in report |
| E14 | source file vanishes / changes mid-run | §4.8 policies; `vanished` / `changed_during_run` |
| E15 | destination full mid-run | breaker → `not_attempted`, exact shortfall in report |
| E16 | USB cable yanked mid-run | breaker after 3 consecutive `device_gone`; resumable exit 4; resume continues watermark |
| E17 | kill -9 at arbitrary ms (chaos) | crash matrix §7.2 holds; oracle-clean after convergent re-runs |
| E18 | dst tree deleted between runs (stale journal) | journal partials fail validation → clean restart; no bad skips (I8) |
| E19 | same-run src == dst, dst inside src, src inside dst | fatal pre-flight error before any I/O |
| E20 | locked destination file (open exe) | replace fails `locked`; Restart Manager names the process |
| E21 | AV/indexer interference (slow closes) | correctness unaffected; detector may hint |
| E22 | OneDrive placeholders | §4.6: hydrate+count by default; `--skip-cloud` |
| E23 | FAT 2 s / exFAT 10 ms timestamp rounding | tolerance table §4.1; re-run steady-state = 100 % Same (test asserts no re-copy churn) |
| E24 | DST shift on FAT dst | `--dst-tolerance` matches ±1 h |
| E25 | dir tree depth 10 000; 1 M dirs; 3 M files in one dir | iterative walk; bounded memory; big-dir enumeration batches |
| E26 | names differing only by case (WSL case-sensitive dir) | duplicate join key → per-file error, not silent overwrite (§4.5) |
| E27 | dest has a *directory* where source has a *file* (and inverse) | error `type_conflict` (no recursive delete exists to "fix" it); hint |
| E28 | 4Kn native / 512e mixed sector sizes | alignment from per-volume profiler values; VHDX matrix test |
| E29 | run from non-console (redirected stdout) | auto `--plain`; no ANSI garbage |
| E30 | clock skew: dst FS timestamps land ≠ set values | post-set read-back *in debug builds only* asserts round-trip; tolerance rules absorb known FS truncation |
| E31 | reparse tag unknown (HSM, custom filters) | not recursed; attempted verbatim reparse copy; on failure → `fs_limit` error with tag logged |
| E32 | source root is a drive root with system artifacts | §4.7 default exclusions, visible count |

## 10. CLI, log format, report format

### 10.1 CLI grammar

```
bigcp <SRC> <DST> [flags]      # copy (the default subcommand)
bigcp verify <SRC> <DST> [--quick] [--hash xxh3|blake3]
bigcp report <REPORT.json>     # open report browser TUI

Flags (copy):
  --dry-run                enumerate+classify only; full report, zero writes
  --verify[=copied|all]    post-copy verification pass (§5.17)
  --hash <xxh3|blake3>     hash algorithm (default xxh3)
  --hash-log               record hashes even without --verify
  --exclude <GLOB>         repeatable; relative-path glob
  --include-system         include root OS artifacts (§4.7)
  --skip-cloud             skip OneDrive/cloud placeholders (§4.6)
  --dst-tolerance          FAT DST ±1 h equivalence (§4.1)
  --retry <N> --retry-wait <MS>     default 0/0
  --backup-mode            SeBackup/SeRestore semantics (requires admin)
  --flush                  FlushFileBuffers per file + volume flush at end
  --no-sparse | --no-unbuffered | --fast-prealloc | --probe
  --large-threshold <SZ>  --chunk <SZ>  --qd <N>  --streams <N>  --threads <N>  --mem <SZ>
  --engine <native|os>     os = CopyFile2 backend (A/B + troubleshooting)
  --fresh                  ignore journal/partials
  --state-dir <DIR> --log <FILE> --report <FILE>
  --plain                  line output instead of TUI (auto when not a TTY)
  --no-color --quiet -y/--yes
Exit codes: 0 ok · 2 completed-with-failures · 3 user-canceled (resumable)
            4 aborted by breaker (resumable) · 5 fatal startup · 6 internal invariant breach
```

Confirmation prompt (skippable with `-y`): shown only when replacing > 1,000 files or > 100 GB of existing destination data — a mistake-catcher for swapped SRC/DST, not a nag.

### 10.2 Log (JSONL, schema v1 — `docs/schemas/log.v1.schema.json`)

One JSON object per line; every line has `ts` (ISO-8601, ms) and `ev`. Events:

```jsonc
{"ev":"run_start","v":1,"run_id":"…","argv":[…],"src":"…","dst":"…","options":{…},
 "devices":[{"role":"src","model":"…","bus":"usb","kind":"ssd","fs":"NTFS","sector":4096,
             "mtl":1048576,"free":…,"confidence":"high"}…]}
{"ev":"dir","action":"created|exists","rel":"…"}
{"ev":"file","action":"copied","rel":"a/b.bin","size":123,"ms":4,"hash":"xxh3:9f…","streams":2}
{"ev":"file","action":"skipped","why":"same","rel":"…"}          // + "meta_fixed":true when repaired
{"ev":"file","action":"failed","rel":"…","op":"open_src","code":5,"category":"permissions",
 "msg":"Access is denied","hint":"…","locker":"WINWORD.EXE"}     // locker only when RM resolved it
{"ev":"file","action":"excluded|not_attempted","why":"…","rel":"…"}
{"ev":"warn","kind":"streams_dropped|efs_downgrade|changed_during_run|orphan_temp|…","rel":"…"}
{"ev":"extra","rel":"…"}                                          // dest-only entry (never touched)
{"ev":"watermark","rel":"…","off":268435456}
{"ev":"stat","counters":{…},"read_mbps":…,"write_mbps":…}         // every 30 s
{"ev":"autotune","dev":"dst","qd":8,"chunk":8388608}
{"ev":"run_end","counters":{"discovered_files":…,"discovered_bytes":…,"copied":…,"skipped":…,
 "meta_fixed":…,"failed":…,"excluded":…,"not_attempted":…,"extra":…},"integrity":"ok","exit":0}
```

Paths are UTF-8 lossy + `path_raw` (hex UTF-16) added when lossy (§4.5). The log is append-only, buffered, flushed every 2 s and at run end.

### 10.3 Report (JSON, schema v1)

Aggregated, self-contained (embeds config + device profiles so it's meaningful years later):

```jsonc
{"v":1,"run":{"id":…,"started":…,"ended":…,"duration_s":…,"exit":…,"resumed_from":…},
 "config":{…},"devices":[…],
 "counters":{… as run_end …},
 "folders":[{"rel":"photos","copied":…,"failed":…,"bytes":…,"mbps":…}],   // per top-level dir
 "errors":[{"category":"locked","code":32,"count":17,"hint":"…",
            "by_folder":{"docs":12,…},"samples":[{"rel":…,"msg":…,"locker":…}]}], // ≤100 samples/cat; log has all
 "warnings":{"streams_dropped":3,"cloud_hydrated":120,"hard_links":8,…},
 "extras":{"count":42,"samples":[…]},
 "timeline":[{"t":0,"read_mbps":…,"write_mbps":…,"files_s":…,"verdict":"dest-bound"}…],
 "phases":{"fastest":{"span":[…],"mbps":…,"folder":"…"},"slowest":{…}},
 "bottleneck":{"verdict":"dest-bound","evidence":"dst busy 97%, src busy 41%","peak_mbps":940,
               "avg_mbps":610,"efficiency":0.65,"provenance":"best sustained 5s window on dst"},
 "hints":[{"id":"slc_cliff","text":"…","confidence":"medium"}],
 "verify":{"mode":"copied","passed":…,"failed":…,"mismatches":[…]},
 "integrity":"ok"}
```

## 11. Terminal UI design

Stack: `ratatui` + `crossterm`. Truecolor with graceful 256/16-color fallback (terminal capability detect); honors `NO_COLOR`; full Unicode with width-aware truncation of long paths (middle-ellipsis, keeping filename visible). The TUI renders immutable state snapshots published by the coordinator (watch channel, ≤30 fps) — **the UI can never touch run data structures or block I/O threads**.

Tabs (keys `1–6`, `Tab`/`Shift-Tab`):

1. **Dashboard** — header (src → dst, run state, elapsed, ETA); bytes bar + files bar with rates; read/write sparklines (120 s window); active transfers table (file, size, %, MB/s — streaming files show per-file progress); discovery ticker ("142 512 files / 1.9 TB found…"); last-3-errors ticker; hotkey footer.
2. **Errors** — tree grouped `category → top-level folder → files`, live counts; `↑↓` navigate, `Enter` expand, `h` opens the hint panel for the selected category (full hint text + example command). Storm-safe: shows counts + first N samples; the log always has everything.
3. **Devices** — per side: model, bus/link, kind (SSD/HDD), FS, cluster, sector, free space, current QD/chunk (autotune live), utilization %, mean/p99 latency.
4. **Performance** — throughput timeline chart, verdict strip (color-coded bottleneck over time), fastest/slowest phases, per-top-folder throughput table.
5. **Hints** — actionable list with confidence tags (§5.14 detectors + static advice), each with "why we think this".
6. **Log** — tail view with `/` filter (category, folder, text).

Global keys: `p` pause/resume · `c` graceful cancel · `C` hard cancel · `r` retry breaker (reconnect flow) · `?` help overlay · `q` quit (same as `c` then exit when drained; in `report` mode just quits).

`bigcp report FILE` opens tabs 2–5 backed by the stored JSON (live-only widgets are hidden). `--plain` mode prints: startup banner (devices + plan), one status line per 5 s (`\r`-less, log-friendly), every error as it happens, and the final summary block — nothing interactive, same information.

Summary block (always printed on exit, TUI or not): counters table, failure breakdown by category × top folder, achieved vs. peak throughput + efficiency, start/end/duration, fastest/slowest phase, top 3 hints, paths of log/report, and the exact command to resume/re-verify.

## 12. Test plan

Testing is the enforcement arm of §7. Anything listed here is CI-gated (Windows runners) unless marked manual.

### 12.1 Unit tests (per crate, `cargo test`)

- `win`: every wrapper against real temp files (create/rename/meta/reparse/sparse FSCTL round-trips); error mapping; alignment helpers (property: any (offset,len) request the engines can produce is sector-aligned).
- `core::path`: the normalization table — long paths, UNC, `\\?\` passthrough, trailing dot/space, reserved names, surrogates, case-fold join keys; property: normalization is idempotent; display+`path_raw` round-trips.
- `core::classify`: the full §4.1 decision table including every tolerance row and `--dst-tolerance`; exhaustive small-case enumeration (size ±, mtime ± tolerance ± 1 unit, attr diffs, type conflicts).
- `core::schedule`: simulated-clock tests — no starvation, dir-before-file ordering, breaker stop, bounded queues (property: memory high-water < budget under adversarial item streams).
- `core::journal`: round-trip; torn-tail (truncate at every byte offset of a valid file — property test) → loader never panics, never resumes unsafely (I8).
- Ring/stream state machine: **sans-I/O** — the §6.3 machine is implemented over an abstract I/O port, driven in tests with scripted completions *in adversarial orders* (out-of-order, short reads, mid-stream errors); assertions: watermark monotone+contiguous, in-order issue, no chunk leaked/double-freed. `loom` model-checks the ring's lock-free index handoff.

### 12.2 The oracle (`testkit check`) and generator (`testkit gen`)

- `gen` builds trees from YAML specs: counts, size distributions (incl. lognormal small-file clouds and boundary sizes from E02), depth profiles, name alphabets (Unicode zoo), ADS, sparse maps, links, attrs, timestamps (incl. FAT-edge values), read-only/hidden/system mixes. Specs live in `testkit/scenarios/*.yaml` — **every E-case in §9 has a scenario file with its ID**.
- `check src dst` is the **independent oracle**: deliberately naive synchronous code (no shared logic with `core` — enforced by crate dependency rules §5.2) that walks both trees and byte-compares data + compares copied metadata per the §4 contract, emitting a machine-readable diff. The oracle is the arbiter of every integration test: *bigcp is correct iff the oracle finds no diff*.

### 12.3 Fault injection

`core::faults` wraps the `win` API surface behind a trait; the fault driver (dev builds only, `--test-faults <spec>` hidden flag) injects any Win32 error at any call site by site-name + probability + deterministic seed. Matrix job: for every injectable site (~40) × representative errors, run a mid-size scenario; assert: correct `Outcome::Failed` accounting (I6/I7), no panic, no invariant breach, resume converges. This is how "error paths that never fire in a healthy lab" get exercised on every CI run.

### 12.4 Chaos (crash/kill) harness — the flagship reliability test

`testkit chaos`: loop { spawn bigcp on scenario → `TerminateProcess` at uniform-random ms (also: suspend/resume storms, breaker-inducing device-error injections) → re-run to completion → oracle } until N clean cycles. Asserts after every cycle: crash matrix §7.2 outcomes only; I5 (no dest file with src-mtime ∧ wrong bytes — the oracle checks content of *every* file whose mtime matches); temps only ever journal-owned or reported; counters reconcile on the completing run. CI: 30 min nightly; release gate: 8 h soak. Historical note for maintainers: this harness is the reason the completion protocol (§4.3) can be trusted; never ship a change to `engine_*`/`journal` without a chaos night.

### 12.5 Filesystem & hardware matrix

- **VHDX matrix (CI-able):** PowerShell fixture creates+formats+mounts VHDXs: NTFS (4 KiB and 64 KiB clusters, compressed dirs, 512e and 4Kn `-PhysicalSectorSizeBytes`), exFAT, FAT32, ReFS (incl. same-volume clone path); full scenario suite × matrix cells; asserts include the degradation rules (§4.4) and E23 no-churn.
- **Real-hardware checklist (manual, release-gated):** USB-C NVMe enclosure (UASP), portable SSD (T7-class), USB HDD (SMR if available), internal NVMe⇄USB, same-spindle HDD copy, cable-yank during 100 GB (E16), Quick-removal vs Better-performance policies. Scripted via `testkit`, results archived in BENCHMARKS.md.

### 12.6 Performance regression + differential

- Perf CI on dedicated runner: workloads W1 (1M×4 KiB), W2 (10×20 GiB), W3 (node_modules-like mixed), W4 (1M dirs) on NVMe scratch; recorded MB/s & files/s vs. rolling baseline; gate: −10 % fails. Targets vs robocopy per §8.7 validated in M3 and pinned as absolute floors thereafter.
- **Differential testing:** same scenario copied by (a) bigcp native, (b) bigcp `--engine os` (CopyFile2), (c) robocopy — oracle-compare the three destinations; semantic deltas must be *exactly* the documented ones (Appendix A). Catches both our bugs and silent Windows behavior changes.

### 12.7 Miscellaneous suites

- TUI: `ratatui` TestBackend snapshot tests for every tab in small/large terminals + non-TTY fallback (E29).
- Schema: every log/report emitted by the whole test suite is validated against the JSON Schemas (a test-mode validator runs in CI); schema evolution is additive-only, checked by a compat test against archived v1 samples.
- Leak watch: soak runs assert flat handle count and bounded working set.
- Long-run: quarterly 10 TB+ manual soak on real hardware, checklist in TESTING.md.

### 12.8 Acceptance checklist (v1.0 ship gate)

All CI suites green 7 consecutive days · 8 h chaos clean · full VHDX matrix clean · real-hardware checklist executed · perf gates met · docs complete per §14.6 · fresh-engineer walkthrough (§14.6) performed by someone who didn't write the code.

## 13. Implementation roadmap

Strictly ordered milestones; each has a hard Definition of Done (DoD). No milestone starts until the prior DoD is met — correctness milestones deliberately precede performance ones, because a fast wrong copier is worthless.

- **M0 — Foundations.** Workspace, CI (fmt/clippy/deny/audit/test on Windows runners), `win` wrappers + tests, path layer + property tests, `testkit gen/check` skeletons, schemas drafted. *DoD: all wrappers tested; oracle can diff two trees; CI green.*
- **M1 — Correct core copier.** Single-threaded, buffered, the *entire* §4 contract (metadata, ADS, sparse, links, dir post-order, temp+rename, exclusions), JSONL log, journal skeleton, `--plain` only. *DoD: full scenario suite + oracle green on NTFS; differential-vs-robocopy deltas are exactly Appendix A; first chaos runs green.*
- **M2 — Scale-out small files.** Parallel enumeration + join, scheduler, small-file engine, counters+reconciliation, breakers. *DoD: W1 ≥ 3× robocopy `/MT:32` on SSD; chaos green at parallelism; E25 memory bound holds.*
- **M3 — Streaming engine.** IOCP engine, device profiler, auto-tuner, same-spindle mode, partial resume watermarks, ReFS clone, `--probe`. *DoD: W2 ≥ 1.3× robocopy `/J` and ≥ 95 % of diskspd baseline on USB-SSD; kill-during-100 GB resume verified; perf gates pinned.*
- **M4 — UX.** TUI all tabs, report writer + `bigcp report`, hints engine, Restart Manager, summary block. *DoD: TUI snapshots green; report schema frozen; usability pass on real 2 TB copy.*
- **M5 — Verify + hardening.** Verify modes + hash cache, fault-injection matrix complete, VHDX matrix in CI, `--engine os` differential, 24 h soak. *DoD: §12 fully implemented and green.*
- **M6 — Release.** Docs per §14, BENCHMARKS.md with real-hardware numbers, acceptance checklist (§12.8), signed binary, v1.0.0. *DoD: a engineer new to the project completes the §14.6 walkthrough unaided.*

Post-v1 backlog (explicitly deferred): `bench` subcommand, EA copy, ARM64 build, config file, `--move` (would require relaxing I1 — needs its own safety design), network tuning.

## 14. Documentation and maintainability

The bar: *a future maintainer can build, test, modify, and release without asking anyone anything.*

### 14.1 Document set (all in-repo, versioned with the code)

| Doc | Contents | Freshness rule |
|---|---|---|
| `README.md` | user guide: install, examples, flag reference, FAQ, safety model summary, removal-safety note | every user-visible change |
| `docs/SEMANTICS.md` | the §4 contract, user-facing wording; the *single* normative statement of behavior | changes require ADR + version bump |
| `docs/DESIGN.md` | §5–§8 of this plan, kept current as-built (this PLAN.md stays frozen as the original plan) | every architectural PR |
| `docs/TESTING.md` | how to run every suite, add scenarios, run chaos/VHDX/real-hardware checklists | with test changes |
| `docs/MAINTENANCE.md` | code map (crate/module → §), the invariant list I1–I10 with their enforcing tests, release checklist, toolchain/deps policy, debugging cookbook (how to read a log/journal, decode a crash) | every release |
| `docs/ERRORS.md` | generated from `errors.rs` table: code → category → hint → resolution | generated in CI, never hand-edited |
| `docs/adr/NNNN-*.md` | Architecture Decision Records; seeded with: 0001 Rust, 0002 two engines, 0003 no async runtime, 0004 temp+rename protocol, 0005 journal design, 0006 skip heuristic, 0007 default exclusions, 0008 cloud-placeholder policy, 0009 xxh3 default, 0010 no-delete design, 0011 no write-probes, 0012 TUI stack | one per contract/architecture change, forever |
| `docs/schemas/*.json` | log + report JSON Schemas, versioned | additive-only in v1 |
| `BENCHMARKS.md`, `CHANGELOG.md`, `CONTRIBUTING.md` | numbers per release · keep-a-changelog · PR checklist + dev setup | per release / per PR |

### 14.2 Code documentation rules

- Every module: header comment — purpose, invariants touched, concurrency notes, pointer to its DESIGN.md section.
- Every `pub` item in `win` and `core`: rustdoc (`#![deny(missing_docs)]`).
- Every `unsafe` block: `// SAFETY:` discharging each obligation; `unsafe` outside `win` is a compile error.
- Comments explain *constraints* ("timestamps after rename — NTFS tunneling, see §4.3"), never narrate code.

### 14.3 Code standards (CI-enforced)

`rustfmt` default · clippy with `unwrap_used`, `expect_used`, `panic` denied in `core`/`win`/`cli` runtime paths (tests exempt) · `cargo-deny` (licenses, dupes, advisories) + `cargo-audit` · no magic numbers for Win32 constants (only `windows-sys` names) · error types via `thiserror`, Win32 code always preserved · bounded channels only · `tracing` spans on every engine operation (compiled out at release unless `--log-level debug`). **PR checklist** (CONTRIBUTING.md): does this touch an invariant I1–I10? → name the test that still enforces it; does it add I/O to a hot path? → attach benchmark; does it change §4 semantics? → ADR + SEMANTICS.md + schema review; chaos night for engine/journal changes.

### 14.4 Decision process

Any change to: the §4 contract, on-disk formats (journal/log/report), safety invariants, or default tuning values ⇒ ADR with context/decision/consequences. ADRs are append-only history; MAINTENANCE.md indexes them.

### 14.5 Glossary (maintained in MAINTENANCE.md)

QD, MTL, VDL, UASP, BOT, SLC cache, SMR/CMR, 4Kn/512e, ADS, EA, reparse point, junction, tunneling, watermark, ring, join, oracle, breaker, engine, stream, FMEA — each with a two-line definition and a pointer to where it matters in the code.

### 14.6 Fresh-engineer walkthrough (the maintainability acceptance test)

A person who has never seen the repo, given only the repo: (1) build and run the test suite from TESTING.md; (2) add a new error-category hint following MAINTENANCE.md's cookbook; (3) add a new scenario YAML and make it pass; (4) produce a release candidate via the release checklist. Performed before v1.0 (§12.8) and after any major refactor; friction found = documentation bug to fix.

## 15. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| IOCP/overlapped subtleties (cancellation, handle lifetime, OVERLAPPED aliasing) | corruption/hangs | sans-I/O state machine + loom + fault injection (§12.1/§12.3); all OVERLAPPED ownership rules documented in `win::iocp` |
| USB bridges lying to IOCTLs / dropping under load | wrong tuning, mid-run dropouts | low-confidence profile fallback (§5.5), auto-tuner steps down on latency, device-gone breaker + resume (§5.13) |
| SLC-cache cliffs / SMR collapse misread as "bigcp is slow" | user mistrust | bottleneck analyzer + honest hints (§5.14); BENCHMARKS.md educates |
| AV filters serializing creates | small-file throughput ceiling | measured + hinted, never auto-tampered (§5.14); documented expectations |
| OneDrive hydration storms | surprise bandwidth/disk usage | placeholder detection, prominent count, `--skip-cloud` (§4.6) |
| Windows semantic changes (rename flags, cloud tags, new FS) | breakage on new builds | runtime feature-detect chains (§5.2), differential suite vs OS engine (§12.6) catches drift |
| `windows-sys`/`ratatui` churn | build breakage | pinned versions + lockfile; upgrade PRs run full matrix |
| xxh3 non-cryptographic | adversarial collision (not a corruption risk) | documented; `--hash blake3` for the threat model that cares |
| Scope creep toward robocopy flag parity | complexity erodes reliability | §2.4 non-goals + ADR gate; "defaults good enough to need no flags" is the product thesis |
| Fancy TUI hiding the truth | missed errors | summary block always printed; log is source of truth; TUI storm-safe (§5.13) |

## 16. Appendices

### Appendix A — robocopy flag mapping (required defaults → bigcp behavior)

| robocopy | meaning | bigcp |
|---|---|---|
| `/E` | recurse incl. empty dirs | default (only mode) |
| `/J` | unbuffered I/O | automatic: unbuffered streaming ≥ threshold, buffered below (§8.1) — *better than a blanket flag* |
| `/COPY:DTA` | data+timestamps+attrs, no ACLs | default (§4.2); ACL copy not implemented |
| `/DCOPY:DATE`¹ | dir data+attrs+timestamps+EAs | default: dir attrs at create + post-order timestamps (§5.10) |
| `/R:0 /W:0` | no retries | default (`--retry` exists, default 0) |
| `/V /FP` | verbose, full paths | JSONL log always full-detail with full relative paths (§10.2) |
| `/ETA` | show ETA | dashboard + plain status line (§6.4) |
| `/SJ /SL` | junctions/symlinks as links | default (§4.6) |
| `/MIR /PURGE /MOV` | deletion modes | **intentionally absent** (§2.4, I2) |
| `/Z` | restartable | superseded by watermark resume (§5.12) without `/Z`'s throughput cost |
| `/B` | backup mode | `--backup-mode` |
| `/DST` | DST tolerance | `--dst-tolerance` |
| `/MT:n` | thread count | automatic per device profile; `--threads` override |

¹ Robocopy's `/DCOPY` letters are `D` (directory data, i.e. dir ADS), `A` (attributes), `T` (timestamps), `E` (dir EAs), `X` (skip ADS); its default is `DA` — plain robocopy does *not* preserve directory timestamps. `/DCOPY:DATE` therefore reads as D+A+T+E. bigcp implements A+T (attributes + post-order timestamps); directory ADS and directory EAs are deferred with file EAs (§4.2, rare on user data, documented limitation).

### Appendix B — Win32 API inventory (implementation checklist for `win`)

| Area | APIs |
|---|---|
| Handles/files | `CreateFileW` (all flag combos §5.8/§5.9), `ReadFile`/`WriteFile` (+OVERLAPPED), `CloseHandle`, `FlushFileBuffers` |
| Metadata | `GetFileInformationByHandleEx` (`FileBasicInfo`, `FileStandardInfo`, `FileIdInfo`, `FileIdExtdDirectoryInfo`), `SetFileInformationByHandle` (`FileBasicInfo`, `FileEndOfFileInfo`, `FileAllocationInfo`, `FileRenameInfo(Ex)`, `FileDispositionInfo(Ex)`) |
| Enumeration | `FindFirstFileExW`(`FindExInfoBasic`, `FIND_FIRST_EX_LARGE_FETCH`) fallback path, `FindFirstStreamW`/`FindNextStreamW` |
| IOCP | `CreateIoCompletionPort`, `GetQueuedCompletionStatusEx`, `PostQueuedCompletionStatus`, `CancelIoEx`, `SetFileCompletionNotificationModes` |
| Reparse/sparse/clone | `FSCTL_GET_REPARSE_POINT`, `FSCTL_SET_REPARSE_POINT`, `FSCTL_SET_SPARSE`, `FSCTL_QUERY_ALLOCATED_RANGES`, `FSCTL_SET_COMPRESSION`, `FSCTL_DUPLICATE_EXTENTS_TO_FILE` |
| Volume/device | `GetVolumePathNameW`, `GetVolumeInformationW`(`ByHandleW`), `GetDiskFreeSpaceW/ExW`, `GetDriveTypeW`, `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, `IOCTL_STORAGE_QUERY_PROPERTY` (device/adapter/seek-penalty/access-alignment), `IOCTL_STORAGE_GET_HOTPLUG_INFO`, `IOCTL_DISK_GET_CACHE_INFORMATION`, `GetFinalPathNameByHandleW` |
| Paths | `GetFullPathNameW`, `CompareStringOrdinal` |
| Privileges/locks | `OpenProcessToken`/`AdjustTokenPrivileges` (`SeBackup`, `SeRestore`, `SeCreateSymbolicLink`, `SeManageVolume`), Restart Manager (`RmStartSession`, `RmRegisterResources`, `RmGetList`, `RmEndSession`) |
| Misc | `GlobalMemoryStatusEx`, `SetThreadPriority`/`SetThreadInformation` (I/O priority), `GetStdHandle`/console mode (TTY detect), `CreateSymbolicLinkW`, `CreateDirectoryW`, `CreateHardLinkW` (future) |

### Appendix C — journal format (v1)

JSONL, one record per line, each with `crc` (CRC-32C of the line minus the crc field):

```jsonc
{"j":1,"ev":"job","run_id":"…","src":"…","dst":"…","opts_hash":"…","ts":"…","crc":"…"}
{"j":1,"ev":"part","rel":"vm/disk.vhdx","temp":"disk.vhdx.a1b2c3d4.bigcp-part",
 "src_size":987654321098,"src_mtime":133497…,"watermark":268435456,
 "hash_state":"xxh3:…serialized…","crc":"…"}          // rewritten every 256 MiB
{"j":1,"ev":"part_done","rel":"vm/disk.vhdx","crc":"…"} // temp renamed; entry retired
{"j":1,"ev":"hash","rel":"a/b.bin","size":…,"mtime":…,"hash":"xxh3:…","crc":"…"} // cache for verify
{"j":1,"ev":"end","run_id":"…","counters":{…},"crc":"…"}
```

Loader rules: unknown `ev` → ignored (forward compat); bad CRC → drop line and everything after it (torn tail); `part` without matching current source metadata → discarded (I8).

### Appendix D — Research references

Tags as cited in §3. Retrieved July 2026; if a link rots, the claim it supports is re-verifiable by measurement (§12.6) or web archive.

**Microsoft documentation**
- [MS-robocopy] robocopy reference — https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/robocopy
- [MS-CopyFileEx] CopyFileExW (what it preserves) — https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-copyfileexw
- [MS-CopyFile2] COPYFILE2_EXTENDED_PARAMETERS (flags) — https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-copyfile2_extended_parameters
- [MS-km-copy] Kernel-mode file copy / NtCopyFileChunk (Win11 22H2) — https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/km-file-copy
- [MS-buffering] File buffering (NO_BUFFERING alignment rules) — https://learn.microsoft.com/en-us/windows/win32/fileio/file-buffering
- [MS-SFVD] SetFileValidData (privilege + security caveats) — https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfilevaliddata
- [MS-rename] FILE_RENAME_INFO / POSIX semantics flags — https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information
- [MS-basicinfo] FILE_BASIC_INFORMATION (-1/-2 timestamp semantics) — https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_basic_information
- [MS-IdExtd] FILE_ID_EXTD_DIR_INFO (per-entry reparse tag + 128-bit id) — https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_extd_dir_info
- [MS-FindFirstEx] FindFirstFileExW (FindExInfoBasic, LARGE_FETCH) — https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstfileexw
- [MS-streams] FindFirstStreamW — https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirststreamw
- [MS-sparse] FSCTL_QUERY_ALLOCATED_RANGES — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_query_allocated_ranges
- [MS-reparse] FSCTL_SET_REPARSE_POINT — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_set_reparse_point
- [MS-clone] Block cloning (ReFS, alignment, <4 GiB extents) — https://learn.microsoft.com/en-us/windows/win32/fileio/block-cloning
- [MS-ioctl-prop] IOCTL_STORAGE_QUERY_PROPERTY — https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddstor/ni-ntddstor-ioctl_storage_query_property
- [MS-hotplug] IOCTL_STORAGE_GET_HOTPLUG_INFO / STORAGE_HOTPLUG_INFO — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-ioctl_storage_get_hotplug_info
- [MS-removal] Default removal policy for external media (≥1809 Quick removal) — https://learn.microsoft.com/en-US/windows/client-management/change-default-removal-policy-external-storage-media
- [MS-attrs] File attribute constants (RECALL_ON_*, OFFLINE) — https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants
- [MS-placeholders] Minifilter guidance on cloud placeholders — https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/placeholders_guidance
- [MS-filetimes] File times (FAT local time, 2 s resolution) — https://learn.microsoft.com/en-us/windows/win32/sysinfo/file-times
- [MS-exfat] exFAT specification (10 ms timestamp increment) — https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification
- [MS-usb-classes] USB class drivers (uaspstor vs usbstor selection) — https://learn.microsoft.com/en-us/windows-hardware/drivers/usbcon/supported-usb-classes
- [MS-robocopy-QA] robocopy large-data best practices (Q&A) — https://learn.microsoft.com/en-us/answers/questions/2264686/best-practices-for-using-robocopy-with-large-data
- [ONT-flush] Old New Thing: FlushFileBuffers vs write-through — https://devblogs.microsoft.com/oldnewthing/20170510-00/?p=95505
- [Chen-tunnel] Old New Thing: NTFS name tunneling — https://devblogs.microsoft.com/oldnewthing/20050715-14/?p=34923

**Engineering write-ups & measurements**
- [Russinovich-SP1] Inside Vista SP1 file copy improvements — https://learn.microsoft.com/en-us/archive/blogs/markrussinovich/inside-vista-sp1-file-copy-improvements
- [Szorc-slow] G. Szorc, "Surprisingly slow" (Defender close-path costs) — https://gregoryszorc.com/blog/2021/04/06/surprisingly-slow/
- [Rustup-close] rust-lang internals: threaded CloseHandle >3× install speedup — https://internals.rust-lang.org/t/installing-docs-is-slow-on-windows/8917
- [Schoener-fetch] FIND_FIRST_EX_LARGE_FETCH measurement (128 s→29 s) — https://blog.s-schoener.com/2024-06-09-find-first-large-fetch/
- [Stoomkracht-Z] robocopy /Z throughput cost measurements — https://stoomkracht.wordpress.com/2016/03/17/robocopy-insights/
- [Andys-MT] robocopy /MT scaling benchmarks — https://andys-tech.blog/2020/07/robocopy-is-mt-with-more-threads-faster/
- [Neowin-22H2] Win11 22H2 copy regression + fix — https://www.neowin.net/news/microsoft-finally-fixes-windows-11-22h2-file-copy-kernel-bug-but-you-may-not-get-it/
- [libtorrent-cache] libtorrent on Windows disk cache / unbuffered quirks — https://blog.libtorrent.org/2012/05/windows-disk-cache/

**Tools**
- [FastCopy-help] FastCopy official help (architecture, buffers, verify) — https://fastcopy.jp/help/fastcopy_eng.htm ; source: https://github.com/FastCopyLab/FastCopy
- [rclone-local] rclone local backend (preallocate, sparse, .partial, multi-thread) — https://rclone.org/local/
- [rclone-docs] rclone global docs/flags — https://rclone.org/docs/ , https://rclone.org/flags/
- [rsync-man] rsync man page (whole-file local default, temp+rename, modify-window) — https://download.samba.org/pub/rsync/rsync.1
- [TeraCopy-doc] TeraCopy copy+verify documentation — https://support.codesector.com/en/articles/8789942-copying-and-verifying-files ; hash guidance — https://codesector.com/how-to/choose-checksum-file-format
- [fcp] fcp (Rust parallel copier) — https://github.com/Svetlitski/fcp
- [xcp] xcp (Rust, parfile/parblock drivers) — https://docs.rs/crate/xcp/latest

**Hardware**
- [ED-uasp] UASP vs BOT — https://www.electronicdesign.com/technologies/embedded/article/21800348/whats-the-difference-between-usb-uasp-and-bot
- [SR-T9] StorageReview Samsung T9 (QD/thread scaling, pSLC) — https://www.storagereview.com/review/samsung-t9-portable-ssd-review
- [Shutter-sustained] 15-min sustained-write tests (X9/X10 Pro, T7 Shield, SanDisk) — https://shuttermuse.com/crucial-x9-pro-and-x10-pro-ssd-tested/
- [Danchar-chipsets] NVMe-USB enclosure chipset survey (RTL9210/ASM2464 quirks) — https://dancharblog.wordpress.com/2024/01/01/list-of-ssd-enclosure-chipsets-2022/
- [AT-bridges] AnandTech UASP dock QD behavior — https://at-web1.www.anandtech.com/show/10024/startech-hard-drive-eraser-dock-capsule-review/2
- [STH-SMR] ServeTheHome DM-SMR write collapse — https://www.servethehome.com/wd-red-smr-vs-cmr-tested-avoid-red-smr/
- [IPlus-USB] USB 3.2 real-world speed comparison — https://iplususb.com/usb-3-2-speed-comparison-drive-benchmark/
- [Flexense-fs] FAT32/exFAT/NTFS USB3 performance comparison — https://www.flexense.com/fat32_exfat_ntfs_usb3_performance_comparison.html
- [BC-1809] Windows 10 1809 removal-policy default change — https://www.bleepingcomputer.com/news/microsoft/windows-10-1809-changed-the-default-removal-policy-for-external-drives/

---

*End of plan. The first implementation step is M0 (§13); the first document to split out of this plan is `docs/SEMANTICS.md` (§14.1), extracted from §4 at M1.*
