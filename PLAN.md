# bigcp — Engineering Plan

> **Status:** Implemented pre-1.0 design; routine gates pass. Release evidence and the bounded huge-directory fallback remain in §12.10/§13.2.
> **Companion documents:** `VISION.md` (product vision, requirements source), this file (complete engineering design).
> **Audience:** whoever implements and maintains bigcp — **human engineers or AI agents, interchangeably**. This document is written so that implementation can proceed *without asking anyone questions*: where a decision could go multiple ways, the decision is made here and the reasoning recorded, and every completion criterion is machine-checkable (§13.1). If genuine ambiguity is found anyway, the rule is: take the more reliability-conservative reading, record it as an ADR (§14.4), and keep going.
> **Companion:** `LIMITATIONS.md` catalogs every deliberate limitation with its rationale — the user-facing mirror of this plan's scope decisions.

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
13. [Implementation order and technical gates](#13-implementation-order-and-technical-gates)
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
| I/O strategy | **One product engine, two paths**: parallel whole-buffer workers write plain small files directly; auxiliary-data, sparse, and large files use a sequential buffered temp path (OS read-ahead/write-behind provide the overlap; queue depth 1 by construction); request sizes are chosen **statically from device-class profiles** (no adaptive tuning, no alternate OS-copy engines — VISION) | §5.8–§5.9, §8.2 |
| Enumeration | Iterative coordinator-owned directory walk with large-fetch enumeration; destination compared via **per-directory join** (one dir listing instead of N per-file stats) | §5.6 |
| Skip heuristic | robocopy-compatible size + mtime, compared **exactly** (100 ns — NTFS/ReFS only, no tolerance machinery), plus cheap *metadata repair* (attributes + creation time) without data rewrite | §4.1 |
| Overwrite safety | Plain small replacements truncate in place under the explicit rerun-repair contract (ADR 0030); auxiliary-data, sparse, and large replacements use an opaque temp and atomic rename (ADR 0034). Every replacement is logged with the old file's metadata and reason | §4.3 |
| Commit safety | Plain small New files use `CREATE_NEW`; plain small replacements validate the classification snapshot on the exact opened handle before truncation; transactional New files publish with a non-replacing rename; transactional replacements revalidate immediately before atomic rename; a second run on the same destination root is refused machine-wide | §4.3, §5.12 |
| Resume | Idempotent re-run via the skip heuristic, plus **verified checkpoints** for partially copied large files: tentative watermarks, and the temp prefix is always re-read and digest-verified before any continuation — verification, not flush ordering, is the integrity guarantee | §5.12 |
| Verification | Large files are always hashed in flight (xxHash3-128 — powers checkpoint integrity, ~free); optional verify pass re-reads the destination files this run wrote; standalone `bigcp verify` reads both trees | §5.11, §5.17 |
| OS baseline | Windows 11 22H2+ (VISION) — modern APIs assumed present; remaining runtime fallbacks are filesystem-dependent, never OS-version-dependent | §2.3, §5.2 |
| UX | Full-screen dashboard TUI (ratatui) with tabs for errors, devices, performance, hints; `--plain` mode for scripts; `bigcp report` re-opens a saved report | §11 |
| Machine-readable output | JSONL event log + JSON report, both versioned with published schemas | §10 |
| Deletion capability | **No mirror/purge path.** bigcp changes only classified source counterparts: direct plain-small replacement, transactional replacement, metadata repair, and exact-handle cleanup of a temp it owns | §7.1 |

### Why not just fix robocopy usage with better flags?

Robocopy's problems are structural, not configurational (details in §3.3): one thread per file with a fixed pipeline, `/J` unbuffered mode applied indiscriminately (which *hurts* small files), no destination-directory join (it stats per file), timestamps written in ways that leave torn files on kill, no partial-file resume that doesn't cost a re-read (`/Z` roughly halves throughput), and no visibility into *why* a run was slow. bigcp is designed around those gaps.

---

## 2. Requirements and scope

### 2.1 Functional requirements (from VISION.md)

| # | Requirement | Where designed |
|---|---|---|
| F1 | Copy a directory tree `SRC` → `DST` recursively, including empty directories (robocopy `/E`) | §4, §5.6 |
| F2 | Defaults equivalent to robocopy `/E /J /COPY:DTA /DCOPY:DATE /R:0 /W:0 /V /FP /ETA /SJ /SL` (see mapping, Appendix A) | §4, Appendix A |
| F3 | Skip files already present and identical at destination, using fast metadata heuristics | §4.1 |
| F4 | Resume close to the interruption point after intentional or abrupt termination | §5.12 |
| F5 | Copy symbolic links and junctions **as links** — never follow them | §4.6 |
| F6 | Errors: no retries by default; log every error with cause; running tally; actionable hints | §5.13 |
| F7 | Machine-parseable log file (JSONL) and a re-openable report file (JSON) | §10 |
| F8 | Verify mode that efficiently checks copies are correct | §5.17 |
| F9 | Dashboard TUI: progress, ETA, throughput, errors, navigable detail; `bigcp report <file>` to re-examine later | §11 |
| F10 | Summary: copied/failed/skipped counts, failure breakdown by top-level folder and reason, achieved vs. **best observed sustained throughput** (VISION's term — exactly what §5.14 measures), start/end time, fastest/slowest portions, bottleneck analysis, improvement hints | §5.14, §10.3 |
| F11 | Avoid unnecessary disk I/O (no gratuitous stats, re-reads, or re-writes) | §5.6, §8.4 |
| F12 | Very large files (40 GB+): partial resume must use a scheme (temp names) that can never be mistaken for a completed file, and resumed content must be **verified** for integrity | §4.3, §5.12 |
| F13 | Every overwrite decision is logged with enough information for later analysis, and summarized statistically | §4.1, §10.2, §10.3 |
| F14 | Baseline Windows 11 22H2+ — use modern APIs and simplify accordingly | §2.3, §5.2 |
| F15 | **NTFS and ReFS volumes only**, source and destination; all other filesystems rejected pre-flight with a clear error; every advantage NTFS/ReFS offer is taken | §4.4 |
| F16 | Source **and destination** trees assumed exclusive and stable; violations detected when detection adds no copy I/O (CPU/RAM cost acceptable, within a measured budget) and treated as errors | §4.8 |
| F17 | System files (System Volume Information, page files, …) excluded by default; exclusions always reported (both would-be and actual); flag to disable | §4.7 |
| F18 | Simple preallocation only; `SetFileValidData` and similar privilege-requiring / stale-data-exposing mechanisms **must not be used** | §5.9 |
| F19 | No interactive prompts during a run — behavior is controlled by arguments | §10.1, §5.13 |
| F20 | `--replace` argument controls replacement of differing destination files; default **true** | §4.1, §10.1 |
| F21 | ADS and EAs are usually absent — their handling must cost ~nothing in the common no-ADS/no-EA case | §4.2, §5.8 |
| F22 | NTFS compression state is **not** carried to the destination (performance); sparse layout **is** maintained where supported (storage cost) | §4.2 |
| F23 | All tests confined to designated folders and harmless — and, absolutely: no large-scale tests (~100 k+ files), no very-long tests, no lifespan-reducing writes, no machine-stability impact (reboot/shutdown/forced disconnect/crash induction); scale and device-loss validation is simulation-only | §12.0 |
| F24 | Tune for *classes* of drives (generations of HDD/SSD, internal/USB-C), never for this specific PC; perf gates expressed accordingly | §8, §8.7, §12.6 |
| F25 | *Withdrawn by VISION:* no disk-temperature monitoring; observed throughput behavior (incl. slowdowns) is the reporting basis | §5.14 |
| F26 | Exactly one run per exact destination root per machine — assumed and enforced | §5.12 |
| F27 | Local volumes only; network (UNC) paths rejected pre-flight with a clear error | §2.3, §4.5 |
| F28 | Exactly one product copy engine; OS-API engines are test-harness differential baselines only; no same-volume clone acceleration (informing the user when the OS could clone faster is allowed) | §5.9, §12.6 |
| F29 | Performance settings chosen statically from drive-class profiles with manual override; **no runtime adaptation of any kind** (a bounded passive governor was specified in an earlier revision and deleted — the device breaker handles misbehaving hardware by stopping resumably, §5.13) | §6.5, §8.2 |
| F30 | One fast hash algorithm everywhere (xxh3-128); accidental corruption/omission detection is the goal, tamper-proofing is not | §5.11 |
| F31 | Abort-and-rerun is the only recovery model; no in-run recovery interactions | §5.13 |
| F32 | Exactly two verification forms: post-copy verification of this run's copies, and standalone full tree-vs-tree verification | §5.17 |
| F33 | Optimize single directories up to ~1 M entries; larger must stay correct but may be slower; retry/probe/hash-log/hard-link-stat/volume-flush options are not needed; argument surface stays minimal | §5.6, §10.1 |
| F34 | Last-access time is best-effort/informational: captured and set on copy, never a skip-equality input (reads and external activity change it) | §4.1, §5.17 |
| F35 | A flag collects judiciously minimal live-run insight for later analysis — no significant slowdown, no unreasonable log growth | §5.14 (`--analyze`), §10.1 |

### 2.2 Non-functional requirements, ranked

1. **Reliability / correctness** — outranks everything, including throughput. Concretely: the *reported* outcome is always true; source is opened read-only always; destination files that bigcp did not create are never deleted, truncated, or overwritten except intentional replacement of a same-relative-path file that differs from source; a crash at any instant never leaves the destination in a state that a re-run cannot repair; counters must reconcile exactly (§7.3).
2. **Throughput** on the primary scenario: SSD/HDD, local + USB-C attached, big RAM, many cores.
3. **Usability** — the dashboard must make state, progress, and problems obvious; errors must explain themselves.
4. **Maintainability** — a new engineer can build, test, and modify the tool from the docs alone (§14).

### 2.3 Supported environment (deliberately narrow)

- **OS:** Windows 11 22H2 or later, x64 (ARM64 build is a stretch goal; no code may preclude it). All 22H2-era APIs (kernel-mode copy in `CopyFile2`, all `COPY_FILE_*` flags, `FileRenameInfoEx`/`FileDispositionInfoEx`, `FileIdExtdDirectoryInfo`) may be assumed present — runtime fallbacks exist only for *filesystem* differences (e.g., POSIX rename is NTFS-only), never for OS versions (§5.2).
- **Filesystems:** NTFS and ReFS only, source and destination (VISION). Any other filesystem — exFAT, FAT32, UDF, third-party — is rejected pre-flight with a clear error (§4.4). This buys exact timestamp semantics, guaranteed file IDs, POSIX rename, and one code path.
- **Transports:** internal NVMe/SATA, and USB-C (USB 3.x / USB4 / Thunderbolt) mass storage — this is the *primary* optimization target.
- **Elevation:** never required and confers no special modes (no backup-privilege mode — VISION; permission failures are reported with a hint).
- **Locations:** local volumes only. Network (UNC) paths are rejected pre-flight with a clear error (VISION).

### 2.4 Non-goals (explicitly out of scope for v1)

These are *decisions*, not omissions. Each cuts complexity that would endanger the reliability goal.

- **No network copying at all** (VISION): UNC/network paths fail pre-flight; no SMB tuning, no compression, no delta transfer (see §3 research: delta transfer is CPU-bound and useless for local copies).
- **No mirror/purge mode** (`/MIR`, `/PURGE`): bigcp must never delete user files; the capability is intentionally absent from the codebase.
- **No ACL/owner/auditing copy** (`/COPY:S,O,U`): matches the required `/COPY:DTA` default. Destination files get default inherited ACLs.
- **No VSS snapshot integration** (locked files fail with a hint naming the locking process instead).
- **No move/rename mode** (`/MOV`) — moving implies deleting from source; source is read-only, period.
- **No hard-link preservation, detection, or reporting** (VISION): hard-linked source files are copied as independent files (robocopy default behaves the same); bigcp does not track file IDs to notice them.
- **De-scoped by VISION** (each was designed in earlier drafts and deliberately removed): **exFAT/FAT32 and all non-NTFS/ReFS filesystems** (rejected pre-flight — this deleted timestamp projection/tolerances, the DST flag, the FAT32 4 GiB limit, and all rename/enumeration fallback chains); alternate OS-copy engines as product features and same-volume ReFS clone acceleration (test-harness use only; a hint tells the user when the OS could clone faster); adaptive auto-tuning (static class profiles + override flags instead); backup-privilege mode; `SetFileValidData`; multiple/cryptographic hash options; in-run recovery interactions (abort-and-rerun only); disk-temperature monitoring; retry arguments; device probing/benchmarking options; hash-recording options beyond verification and resume integrity; volume-level flushing.
- **No EFS raw copy** (`/EFSRAW`): encrypted files are copied as readable plaintext content and re-encrypted at destination when possible (§4.2).
- **No 32-bit builds, no Windows 7/8 support.**
- **No config files** in v1 — flags only, with excellent defaults. (Revisit only if flag count grows past ~20.)
- **No interactive prompts during a run** (F19): every behavior choice is an argument; the tool never blocks on a question.
- **No NTFS-compression propagation** (F22): content is copied (transparently decompressed on read); the compression storage attribute is not re-applied — counted in the report, documented, never a warning storm.
- **No MFT raw parsing** for enumeration (WizTree-style). Rejected: requires admin + NTFS-only + a second uncorrelated code path for the most correctness-critical stage; parallel `NtQueryDirectoryFileEx` enumeration is within striking distance at far lower risk. (§3.5)

### 2.5 Glossary

See §14.5 — the glossary is part of the maintainability contract. Terms used heavily below: **QD** (queue depth: I/Os in flight per device), **MTL** (`MaximumTransferLength` from the storage adapter), **VDL** (NTFS valid data length), **UASP** (USB Attached SCSI protocol), **engine** (a copy execution strategy: *small-file engine* or *streaming engine*), **join** (matching a source directory listing against the corresponding destination listing), **oracle** (the independent tree-comparison checker used by tests, §12.2).

## 3. Background research

Findings from a structured survey (July 2026) of Microsoft documentation, OS-internals write-ups, tool source/documentation (FastCopy, robocopy, rclone, rsync, TeraCopy, fcp/xcp), and storage-hardware deep dives. Bracketed tags cite Appendix D. Facts marked *(uncertain)* could not be pinned to an authoritative source and must be verified by measurement during implementation (they are all benchmark-verifiable; §12.6).

### 3.1 Why robocopy underperforms — the concrete mechanisms

1. **Per-file parallelism only, default 8 threads.** `/MT:n` (1–128) parallelizes whole files; a single large file is never split, and `/MT` gives little on large-file workloads [MS-robocopy][Andys-MT]. Small-file workloads peak around `/MT:16` in community benchmarks — well short of what modern SSD queue depths accept.
2. **`/J` is a blunt instrument.** It applies unbuffered I/O to *every* file; unbuffered small-file writes lose cache-manager write coalescing and get slower, compounding under `/MT` [MS-robocopy-QA]. The right policy is size-dependent — which is exactly what Windows' own copy engine learned in the Vista SP1 rework (cached I/O below ~256 KiB, pipelined larger I/O above) [Russinovich-SP1].
3. **Restartable mode `/Z` costs ~3×.** Measured 950 → 270 Mbps on 1 GbE when enabled [Stoomkracht-Z]; where its restart state lives is undocumented. Resume must be cheap or nobody uses it — bigcp's watermark design (§5.12) has no steady-state cost.
4. **Retry defaults are a trap:** `/R:1000000 /W:30` means one locked file can stall a job for weeks [MS-robocopy]. (The required `/R:0 /W:0` defaults avoid this; bigcp adopts them.)
5. **Per-file destination stats, per-file attribute reopens** *(uncertain, inferred from behavior)* — no destination-directory join; every file pays extra opens.
6. **No diagnosis.** Robocopy reports what it did, never *why it was slow* — no device awareness, no bottleneck attribution, no hints. This is a product gap as much as a perf gap.
7. Robocopy *has* quietly gained modern features worth matching or noting: `/SPARSE`, ReFS block-clone by default (`/NOCLONE` to opt out), `/NOOFFLOAD`, SMB compression [MS-robocopy]. bigcp matches sparse preservation, deliberately does *not* match block-clone (F28 — hint only, §5.9), and ignores the SMB-only ones (non-goal).

### 3.2 Windows I/O stack facts the design relies on

- **Kernel-mode copy exists now.** Windows 11 22H2+ `CopyFileEx`/`CopyFile2` use `NtCopyFileChunk`: kernel-requestor reads/writes with copy intent signaled at create time so minifilters (Defender) can skip double-scanning [MS-km-copy]. A hand-rolled engine forgoes that filter-skip — one reason the **test harness** keeps a CopyFile2-based reference copier as a differential baseline (§12.6; per VISION it is not a product feature). 22H2 also shipped (then fixed) a large-copy regression [Neowin-22H2] — a reminder to benchmark per-OS-build in CI.
- **What `CopyFileEx` handles automatically** (and a custom engine must reimplement): ADS, EAs, attributes, sparse/compression flags, EFS re-encryption, preallocation; it does *not* copy DACLs [MS-CopyFileEx]. Modern `CopyFile2` flags cover most of our semantics à la carte (`COPY_FILE_NO_BUFFERING`, `ENABLE_SPARSE_COPY`, `OPEN_AND_COPY_REPARSE_POINT`, `SKIP_ALTERNATE_STREAMS`, `DIRECTORY`, `DISABLE_PRE_ALLOCATION` — the last confirming the OS engine preallocates by default) [MS-CopyFile2]. This makes the OS backend nearly semantics-complete on 19041+, strengthening its role as differential-test oracle #2 (§12.6).
- **Unbuffered I/O rules:** offsets/lengths must be multiples of the volume's logical sector size; buffers aligned to physical sector size (query via `StorageAccessAlignmentProperty`; VirtualAlloc's page alignment satisfies it; align to `max(4096, physical)` and be done) [MS-buffering]. `WRITE_THROUGH` is orthogonal (FUA; some USB bridges ignore it — `FlushFileBuffers` issues a real SYNCHRONIZE CACHE that drives near-universally honor [ONT-flush]). Unaligned end-of-file: the pad-then-trim technique (§4.3) works on modern NTFS via `FileEndOfFileInfo` on the same handle; historical reports of failures on other stacks *(uncertain)* mandate the buffered-reopen fallback in `win::file`.
- **Valid Data Length:** writes landing beyond VDL zero-fill the gap first; strictly-increasing completion avoids it; `SetFileValidData` avoids it too but requires admin + `SeManageVolumePrivilege` and exposes stale disk data on crash — bigcp's bounded in-order window (§5.9) gets ~all the benefit with none of the risk. rclone hit this exact issue with out-of-order multi-thread chunks and works around it by marking the destination *sparse* on Windows [rclone-local] — a valid alternative rejected here because it changes the file's allocation semantics (§3.5).
- **Enumeration:** all fast paths sit on `NtQueryDirectoryFile(Ex)`; the wins are big buffers and skipping 8.3-name retrieval. `FileIdExtdDirectoryInfo` returns size, all timestamps, attributes, EA size, **reparse tag**, and 128-bit file ID per entry — everything the classifier needs, plus hard-link detection via ID, with zero per-file opens [MS-IdExtd]. `FIND_FIRST_EX_LARGE_FETCH`-style large buffers measured ~4× on cold HDD metadata (128 s → 29 s) [Schoener-fetch].
- **Small-file cost lives in `CreateFile` and `CloseHandle`, amplified by filter drivers.** Defender scans synchronously in the post-write close path — >100 ms worst case per file; Mercurial/rustup obtained **>3×** small-file throughput by moving `CloseHandle` to a thread pool [Szorc-slow][Rustup-close]. bigcp measured that candidate and rejected it as parity-to-worse: the existing worker count already overlaps closes (§5.8, BENCHMARKS.md).
- **Name tunneling** re-applies a replaced file's creation time for 15 s after delete/rename [Chen-tunnel] — the reason timestamps are set *after* the final rename (§4.3).
- **`FileRenameInfoEx` (`POSIX_SEMANTICS | REPLACE_IF_EXISTS | IGNORE_READONLY_ATTRIBUTE`)** is available from Win10 1607, NTFS-only — with classic-rename fallback [MS-rename]. `FileBasicInfo` sets all timestamps + attributes in one call on the open handle; setting them as the last op before close prevents close-time clobber [MS-basicinfo].
- **Block cloning is ReFS-only** (incl. Dev Drive); in-box copy engines block-clone automatically since Win11 24H2; calls must be cluster-aligned and <4 GiB per extent [MS-clone].
- **Storage introspection needs no admin**: volumes/physical drives opened with `dwDesiredAccess=0` accept the metadata IOCTLs (`STORAGE_QUERY_PROPERTY`, `VOLUME_GET_VOLUME_DISK_EXTENTS`) [MS-ioctl-prop]. Seek-penalty and alignment queries **frequently fail on USB bridges** — the low-confidence fallback profile (§5.5) is mandatory, not defensive gold-plating. Removal policy is detectable via `IOCTL_STORAGE_GET_HOTPLUG_INFO`; device write-cache state via `IOCTL_DISK_GET_CACHE_INFORMATION` [MS-hotplug].
- **Cloud placeholders:** `RECALL_ON_DATA_ACCESS`/`RECALL_ON_OPEN`/`OFFLINE` attributes arrive in enumeration; normal reads hydrate (download); `FILE_FLAG_OPEN_NO_RECALL` opens without triggering recall; copying cloud reparse points verbatim to another volume corrupts them (they belong to the cloud filter) [MS-placeholders][MS-attrs]. Robocopy mass-hydrates silently. bigcp policy: §4.6.

### 3.3 Tool survey — what the proven tools actually do

| Tool | Architecture insight adopted / rejected |
|---|---|
| **FastCopy** [FastCopy-help] | The reference design. Different physical drives → separate reader/writer threads; same drive → alternating bursts; I/O is broadly unbuffered. bigcp adopts topology detection and xxh3/read-back verification, but deliberately keeps buffered I/O and defers alternating bursts until project-specific measurement justifies the complexity. It adds the destination join, verified checkpoint resume, and diagnosis. |
| **robocopy** | §3.1. Also: classification vocabulary (Same/Newer/Older/Changed/Tweaked/Extra/Lonely) — bigcp's classifier (§4.1) is a cleaned-up version; `/DCOPY` letters confirmed as D/A/T/E/X with default `DA` (so plain robocopy does *not* preserve dir timestamps; the required `/DCOPY:DATE` default reads as D+A+T+E — Appendix A note). |
| **rclone** [rclone-local][rclone-docs] | size+modtime skip with per-backend `--modify-window`; `name.XXXXXX.partial` + rename; preallocation via `NtSetInformationFile`; multi-thread single-file chunks ≥256 MiB (sparse-marking to dodge VDL); **no partial-content resume** (restarts file). bigcp adopts: partial suffix + rename, preallocation. bigcp improves: verified checkpoint resume, in-order writes instead of sparse-marking, and *no* modify-window at all — exact comparison, NTFS/ReFS only (F15). |
| **rsync** [rsync-man] | `--whole-file` is default for local↔local (delta transfer is a *loss* locally — CPU for absent network savings; validates our no-delta non-goal). Temp-file + atomic rename; postorder directory-mtime fixup; `--modify-window` for FAT. All adopted (independently arrived at, now confirmed as the convergent design). |
| **TeraCopy** [TeraCopy-doc] | Hash-during-copy then read-back verify phase; per-file skip-and-continue with "retry failed only"; persisted job list = resume. Confirms our verify shape. Caution: no evidence its read-back defeats the OS cache — bigcp's unbuffered read-back closes that hole. |
| **Explorer / Vista SP1 engine** [Russinovich-SP1] | The canonical study in copy-engine tuning: cached I/O below 256 KiB, pipelined 1–2 MiB async I/O above, read-ahead at 2× I/O size; essentially serial per file. Validates the two-engine split and the size threshold's existence (exact crossover re-benchmarked on current hardware, §13 gates). |
| **fcp / xcp (Rust, Linux)** [fcp][xcp] | Work-stealing parallel tree walk feeding per-file parallel copies; block-level parallelism reserved for large files; explicit "not tuned for HDDs" (parallelism inverts to a penalty there — confirming our HDD clamps). Same convergent structure as bigcp's enumeration/scheduler split. |

**Skip-heuristic convergence:** every serious tool lands on size + mtime with a filesystem-granularity tolerance (robocopy `/FFT` 2 s, rsync/rclone `--modify-window`), with optional hash mode. FAT stores local time → DST shifts apparent mtimes by exactly 1 h (robocopy `/DST`) [MS-filetimes]. bigcp goes stricter than the consensus: with only NTFS/ReFS in scope (F15), §4.1 uses exact 100 ns comparison — the tolerance machinery every other tool carries exists solely for FAT-family filesystems bigcp rejects.

**Resume convergence:** no surveyed tool journals chunk state during normal copies; the universal baseline is per-file restart plus source size/mtime revalidation, with atomic publication where offered. bigcp keeps that as layer 1, makes every multi-part logical file transactional, and adds cheap watermarks only for large files — the one place restart cost is real (§4.3, §5.12).

### 3.4 Hardware realities (USB-C, SSDs, HDDs)

- **UASP vs BOT:** BOT serializes one command at a time; UASP provides tagged queueing (practical concurrency firmware-bound *(uncertain)*). Windows loads `uaspstor.sys` for UAS-capable devices and `usbstor.sys` for BOT [MS-usb-classes][ED-uasp]. Multi-stream benchmarks on other tools show possible gains, but bigcp's shipped large path remains one buffered synchronous loop; parallel-large I/O is post-v1 and benchmark-gated rather than implied by the bus label.
- **Realistic link ceilings:** 5 Gbps ≈ 420–460 MB/s; 10 Gbps ≈ 1.0–1.1 GB/s; 20 Gbps ≈ 2.0–2.1 GB/s; USB4/TB 40 Gbps ≈ 3.2–3.8 GB/s [IPlus-USB][Danchar-chipsets]. These feed the report's sanity expectations, not hard-coded limits.
- **Bridge-chip reality:** RTL9210B / ASM2362 / JMS583 (10 Gbps ≈ 875 MB/s class) have documented firmware-dependent dropouts under sustained writes and thermal issues; ASM2464PD (USB4) throttles without active cooling [Danchar-chipsets][AT-bridges]. Consequences designed in: device-gone circuit breaker + resumable exit (§5.13), deliberately conservative static USB profiles (§8.2), no infinite max-QD hammering.
- **Portable-SSD write cliffs are normal:** pSLC caches (e.g. T9 ≈ 180 GB class) then sustained rates drop — some drives hold ~1 GB/s (T7 Shield, X10 Pro), others fall to ~500 MB/s (SanDisk Extreme class) [SR-T9][Shutter-sustained]. DRAM-less drives lose HMB over USB (the bridge is the NVMe host) → weaker sustained/random behavior *(uncertain quantitatively)*. Consequence: the bottleneck analyzer *detects and explains* the cliff (burst vs sustained rates reported separately; ETA switches to sustained rate) instead of "fighting" it — backing off cannot help; the cache drains at its folding rate regardless (§5.14).
- **HDDs:** DM-SMR external drives can collapse after their media cache fills [STH-SMR] and are not reliably classifiable in software. Pure sequential HDD work wants large requests. Same-spindle alternating bursts remain a plausible post-v1 optimization, but the shipped engine only reports shared-disk topology and uses the generic HDD profile (§8.3).
- **Removal policy:** Windows ≥1809 defaults external drives to "Quick removal" (OS write caching off) [MS-removal] — per-file latency dominates small files there; detectable via hotplug IOCTL (§5.5); bigcp explains rather than overrides (§8.6). Even "completed" writes can sit in the *drive's* DRAM; only `FlushFileBuffers` (real cache-flush command) is universally honored — the basis of `--flush` (§8.6).
- **exFAT on external drives:** no metadata journal (interrupted metadata updates can corrupt the volume), slower small-file metadata than NTFS. Originally in scope; this fragility contributed to the VISION decision to support **NTFS/ReFS only** and reject exFAT/FAT32 pre-flight (§4.4, F15) — external drives get reformatted to NTFS instead.

### 3.5 Techniques adopted / improved / rejected

| Technique | Verdict | Reasoning |
|---|---|---|
| Two engines, size threshold | **Adopt** (FastCopy would apply unbuffered everywhere; Explorer's history proves the split) | §8.1 |
| Topology-aware same-spindle burst alternation | **Defer post-v1** — detected and reported, but not implemented without a project-specific benchmark | §8.3 |
| Destination per-directory join | **Improve** — none of the surveyed tools do it; eliminates per-file dest stats | §5.6 |
| Deferred CloseHandle pool | **Reject for v1** — measured parity-to-worse; existing workers already overlap closes | §5.8 |
| Hash-during-read + read-back verify | **Adopt** (FastCopy/TeraCopy shape); cache-defeat comes from running standalone verify later (cold cache) rather than unbuffered read-back — stated honestly in §5.17 | §5.17 |
| Mixed completion: direct plain small; temp + atomic rename for auxiliary-data, sparse, and large files | **Adopt by measurement and harden structurally** (ADRs 0030–0031, 0034) | §4.3 |
| Watermark partial resume | **Improve** — no surveyed tool has cheap large-file resume (`/Z` ≈ 3× cost; rclone restarts) | §5.12 |
| Attribute repair without data rewrite | **Improve** (robocopy needs `/IT` and recopies) | §4.1 |
| Per-FS timestamp tolerance | **Obsolete** — NTFS/ReFS-only scope (F15) makes exact 100 ns comparison possible; every other tool's tolerance machinery exists for filesystems bigcp rejects | §4.1 |
| Bottleneck attribution + hints | **New** — no surveyed tool explains its own performance | §5.14 |
| Counter reconciliation as hard invariant | **New** (borrowed from storage-system design, not copy tools) | §7.3 |
| `SetFileValidData` by default | **Reject** — admin-only + stale-data exposure; in-order window is nearly as good | §5.9 |
| Sparse-marking dest to dodge VDL (rclone) | **Reject** — mutates allocation semantics of the destination file | §3.2 |
| Delta transfer (rsync) | **Reject** for local — CPU spent to save absent network | §3.3 |
| MFT raw parse enumeration | **Reject** — admin + NTFS-only + parallel walk is fast enough | §2.4 |
| Kernel copy (CopyFile2) in the product | **Reject** (VISION: one engine) — gives up scheduling, watermarks, unbuffered control, per-chunk hashing; retained solely as the test harness's differential baseline | §12.6 |


## 4. Copy semantics specification (the contract)

This section is normative. The implementation, tests, and user documentation (`SEMANTICS.md`, §14) must all agree with it. Any change here requires an ADR (§14.4).

### 4.1 The skip heuristic ("is the destination file already correct?")

For every source file with a corresponding destination entry (matched by relative path, compared case-insensitively; §4.5), classify:

| Classification | Condition | Action |
|---|---|---|
| **Same** | size equal AND last-write time **exactly** equal (§ below) AND both plain files (or matching reparse type) | Skip. No data I/O. |
| **Same, metadata differs** | Same as above but copyable attributes (§4.2 mask), creation time, or **`EaSize`** (free in both enumeration records) differ | *Metadata repair*: one `FileBasicInfo` write fixes attributes + creation time; an `EaSize` mismatch additionally dispatches EA reconciliation (§4.2 — rare, cheap). No unnamed-stream data I/O. Counted as `meta_fixed` (disjoint outcome, §7.3). |
| **Different** | size differs, or projected mtime differs | With `--replace=true` (default, F20): **Replace** via §4.3. With `--replace=false`: leave untouched, outcome **`skipped_diff`**. Either way the decision is fully logged: which fields differed, the old file's size/mtime/attributes, and whether the destination was *newer* than the source — and the report aggregates the statistics (F13). |
| **Type conflict** | file vs. directory vs. reparse-point mismatch (including a destination directory that is a reparse point where the source has a real directory) | **Error** (`type_conflict`), never a silent replacement or a write *through* the unexpected object (§9, E27). |
| **New** | no destination entry | Plain small files use `CREATE_NEW`; transactional files publish with a non-replacing rename. A concurrently appeared name therefore fails as `destination_changed`, never clobbers an unexamined object (§4.3). |
| **Extra** | destination entry with no source counterpart | Never touched. Counted, sampled into the report. |

**Timestamp comparison is exact.** NTFS and ReFS — the only supported filesystems (F15) — both store every timestamp field at 100 ns resolution in UTC, so comparison is plain 64-bit FILETIME equality: no tolerance windows, no per-filesystem projection, no DST compensation (those were all FAT-family artifacts, deleted with FAT support). This is strictly stricter than robocopy's 2 s default, and a completed run trivially reaches a stable all-Same state on re-run (pinned by E23). Only creation and last-write participate in classification/repair; **last-access is set on copy but never compared** — it drifts by the act of reading and would cause perpetual churn.

**Sparse/compressed state never affects sameness.** Logical size and mtime are the comparands; a dense or uncompressed destination copy of a sparse/compressed source is *correct* (those are storage-layout attributes, not content) and must not cause recopy churn.

**Scope note — alternate streams and classification.** A file classified *Same* is judged on its unnamed stream (size + mtime) plus the free metadata above (attrs, ctime, `EaSize`); an ADS-only divergence on an otherwise-Same file is **not** detected at copy time — the directory enumeration record carries no ADS indicator (unlike `EaSize`), so detection would cost a stream query per *skipped* file, violating F11/F21 for a vanishingly rare case. It *is* detected by standalone `bigcp verify`, which compares full stream sets (§5.17). This trade is documented rather than hidden: `skipped_same` means "matched on everything the heuristic examines," exactly as specified here (E43).

**Why size+mtime and not hashes:** it requires zero additional I/O (both values arrive in the directory enumeration record), it is the proven industry heuristic (robocopy, rsync `--whole-file`, rclone all default to it), and its false-negative mode (same size, same mtime, different content) requires either deliberate tampering or a broken program that rewrites content while restoring timestamps. Users who need stronger guarantees run `--verify` or `bigcp verify` (§5.17). We *improve* on robocopy by: (a) exact 100 ns comparison instead of a blanket 2 s tolerance (possible because only NTFS/ReFS are in scope); (b) metadata repair (attributes + creation time) without data rewrite; (c) large-file digests computed in flight by default (§5.11), so the log/report carry content hashes for later analysis at zero extra I/O.

**Direction rule:** source always wins. A "different" file is replaced even if the destination is newer (robocopy's default also copies older files). bigcp is a copier, not a synchronizer; the report calls out how many replaced files were newer at destination so the user can notice a mistake.

### 4.2 What is copied, exactly

Per the required `/COPY:DTA /DCOPY:DATE` defaults:

| Item | Copied? | Mechanism | Notes |
|---|---|---|---|
| File data (default `$DATA` stream) | ✔ | engines §5.8/§5.9 | |
| Alternate data streams (files **and directories**) | ✔ | Stream discovery via path-based `FindFirstStreamW` plus identity revalidation against the open handle — one extra metadata operation per file, deliberately chosen over a hand-written unsafe `FileStreamInfo` buffer parser (F21's spirit — near-zero cost in the common no-ADS case — is met; its literal zero-extra-open target was retired as not worth an unsafe parser plus malformed-record test matrix); each `:name:$DATA` copied like file data (directory ADS = the `D` in `/DCOPY:DATE`) | If a destination volume reports no named-stream capability (capability flags, §4.4): streams are **dropped with a per-file warning** and counted (`streams_dropped`) — never silently. |
| Timestamps (create, last-write, last-access) | ✔ | `SetFileInformationByHandle(FileBasicInfo)` on the direct write handle at create for plain small files (ADR 0031), or after atomic rename for auxiliary-data, sparse, and large files (§4.3, ADR 0034) | |
| Attributes — **explicit copyable mask** | ✔ mask only | same `FileBasicInfo` call as timestamps (one syscall for both). Copied: `READONLY, HIDDEN, SYSTEM, ARCHIVE, NOT_CONTENT_INDEXED`. **Never set**: `TEMPORARY` (changes filesystem write-behind behavior), `OFFLINE`/`RECALL_*`/`PINNED`/`UNPINNED` (HSM/cloud-filter-owned), `NO_SCRUB_DATA`/integrity (below), `SPARSE`/`COMPRESSED`/`ENCRYPTED` (feature-specific paths, not basic attributes) | The mask is one named constant; classification and repair use the same mask. |
| ReFS integrity streams | destination-policy governed | none — the new file simply inherits the destination directory/volume integrity policy; bigcp neither copies nor overrides the source's integrity setting (integrity has allocate-on-write costs the *destination owner* should control) | The source setting is not queried or compared, so no per-file delta is logged; documented in LIMITATIONS.md. |
| `FILE_ATTRIBUTE_COMPRESSED` | **✖ not carried over** (F22) | — | Content is copied (source reads are transparently decompressed); re-compressing the destination costs CPU/throughput for no correctness gain (§4.1: layout, not content). Count of compressed sources appears in the report; documented, no per-file warnings. |
| `FILE_ATTRIBUTE_ENCRYPTED` (EFS) | best effort | request `FILE_ATTRIBUTE_ENCRYPTED` at destination create | Content is copied decrypted (we read plaintext); if destination cannot encrypt → warning `efs_downgrade`, file still copied. |
| Sparse allocation | ✔ as optimization (dedicated pipeline, §5.9) | Sparse source (allocation < logical size, free from enumeration): `FSCTL_SET_SPARSE` on the temp **first**, set logical EOF, **no full preallocation** (which would defeat sparseness), then write only the ranges `FSCTL_QUERY_ALLOCATED_RANGES` reports, in offset order. Holes participate in the logical digest (hashed from a reusable zero page — CPU-only, no I/O) so the digest is always the hash of the *logical* file, and checkpoint watermarks advance through holes instantly | `--no-sparse` disables. Every sparse source uses this pipeline when the destination supports it, regardless of the small/large threshold. A destination without sparse capability is expanded dense and reports disk-full normally if capacity runs out; a dense copy is still a *correct* copy (§4.1). |
| Extended attributes (EAs, files and dirs) | ✔ when present | `EaSize ≠ 0` arrives **free** in every enumeration record (F21: zero cost in the common EA-less case); when set, the EA blob is copied via `BackupRead`/`BackupWrite`, parsing only `BACKUP_EA_DATA`, on **dedicated synchronous buffered handles** opened just for this — `BackupRead` requires a synchronous handle and misbehaves on `NO_BUFFERING`/`OVERLAPPED` ones, so the engines' data handles are never reused here (the extra open is paid only by the rare EA-bearing item) | Honors the `E` in `/DCOPY:DATE` (Appendix A). If the destination volume reports no EA capability (§4.4): warn `ea_dropped`, counted, never silent. |
| DACL/SACL/owner | ✖ by requirement | — | `/COPY:DTA` excludes them. |
| Directories: existence, attributes, timestamps | ✔ | create → attrs at creation; timestamps set in **post-order pass** (§5.10) | Children creation bumps parent mtime; hence timestamps must be re-set after a directory's subtree is complete. |
| Symlinks (file & dir) and junctions | ✔ as links | §4.6 | Never followed. |
| Hard links | file content duplicated per link | — | Not detected or reported (VISION): each link copies as an independent file; documented in README. |

### 4.3 Replacement and completion protocol (crash safety)

Two measured completion protocols implement VISION's abort-and-rerun contract (ADRs 0030–0031), with auxiliary-data files structurally assigned to transactional publication by ADR 0034.

**Plain small files** (one unnamed stream below `large_threshold`, no EAs, not sparse or checkpoint-eligible) are read completely and source-revalidated before any destination mutation. A New item opens its final name with `CREATE_NEW`; a replacement truncates its existing final name in place, preserving that file's security descriptor. If a known read-only replacement must be cleared, the clear is bound to the enumerated file identity and restored if the subsequent open fails. Source timestamps and attributes are stamped at create, then the one whole-buffer payload is written. The source is revalidated again, optional `--flush` completes, the handle closes, and only then is `copied` emitted. A kill before the whole unnamed write completes leaves a shorter final-named file that the size heuristic replaces on rerun. Once that write completes, the file's only logical payload is complete even if its terminal audit record was not emitted. This path deliberately does not promise an intact old replacement during interruption; users must not consume a destination until a run completes (LIMITATIONS.md).

**Files with ADS or EAs, sparse files, and large files** use opaque sibling temporaries and atomic publication. Auxiliary data is routed here regardless of size so a kill cannot expose an incomplete logical stream/EA set under a size+mtime-matching final name (ADR 0034):

1. Create `.bigcp-«runid8»-«nonce».part` with `CREATE_NEW` and arm delete-on-close. Its short name is independent of the final component length (E38).
2. Write all data streams at exact logical sizes. At the first checkpoint the temp becomes journal-backed and persistent; resume later requires matching source/temp identities and a reread prefix digest.
3. For a replacement, revalidate the target's identity, kind, size, mtime, attributes, and reparse tag. Preserve a protected destination DACL onto the temp; failure aborts rather than weakening protection.
4. Clear delete-on-close and rename temp → final with `FileRenameInfoEx`. `ReplaceIfExists` is used only for classified replacements; New publication is non-replacing. Set final metadata after rename to defeat NTFS name tunneling.
5. With `--flush`, call `FlushFileBuffers` after rename and metadata. Close, then emit `copied` and retire any checkpoint.

Non-checkpointed transactional temps normally self-delete on process exit. Checkpointed temps persist intentionally. A power loss or the narrow named-stream disposition window can strand an opaque temp; orphan scanning was deliberately removed (ADR 0027), so an unreferenced artifact is reported as an extra and never deleted by path. A kill after rename but before metadata leaves correct content with a mismatching timestamp, so rerun replaces it. The full crash matrix is in §7.2.

### 4.4 Filesystem support: NTFS and ReFS only (F15)

**Pre-flight gate:** both volumes are identified once via `GetVolumeInformationW`; anything that is not NTFS or ReFS — exFAT, FAT32, UDF, third-party filesystems — is **rejected before any tree I/O** with a clear error ("bigcp supports NTFS and ReFS volumes only"; hint: reformat, or use robocopy for legacy media). This single gate is what deleted an entire compatibility layer from earlier drafts: timestamp projection/tolerances, the DST flag, the FAT32 4 GiB limit, rename and enumeration fallback chains, and the FAT-family degradation matrix (E03).

What remains is a small **capability delta between NTFS and ReFS**, handled by per-volume capability *flags* (`FILE_NAMED_STREAMS`, `FILE_SUPPORTS_EXTENDED_ATTRIBUTES`, `FILE_SUPPORTS_SPARSE_FILES`, `FILE_SUPPORTS_ENCRYPTION`, `FILE_SUPPORTS_BLOCK_REFCOUNTING`, …) — never by filesystem-name switches, so ReFS version differences and future changes are absorbed automatically:

| Capability | NTFS | ReFS | If the destination volume lacks it |
|---|---|---|---|
| ADS | ✔ | ✔ (modern versions) | warn `streams_dropped` per item, continue |
| EAs | ✔ | version-dependent | warn `ea_dropped` per item, continue |
| Sparse | ✔ | ✔ | expand dense; pre-check free space vs. logical size |
| EFS | ✔ | ✖ | warn `efs_downgrade` (content copied) |
| Compression attr | ✔ | ✖ | moot — never propagated by design (F22, §4.2) |
| Reparse points (links) | ✔ | ✔ | error per link (`links_unsupported`) — should not occur on NTFS/ReFS |
| Timestamps | 100 ns UTC | 100 ns UTC | — (identical; exact comparison, §4.1) |
| Block cloning | ✖ | ✔ (OS engines only) | not used by bigcp (F28); same-volume ReFS emits the "OS would clone faster" hint (§5.5) |

**Degradation policy — what still counts as "copied" (F15/F17 clarity):** a file counts as *copied* when its data and every piece of metadata the destination volume **can represent** are in place. A missing capability produces per-item warnings and a distinct `copied-with-warnings` counter in the summary — visible, never silent, and not a failure. Reparse points are the exception: with no representable equivalent they **fail** (`links_unsupported`) — a link is identity, not decoration.

### 4.5 Path handling

- All user input paths are canonicalized once at the boundary: `GetFullPathNameW` → verify existence class → convert to extended-length form (`\\?\C:\…`). **Every** Win32 call uses the extended form; display strips it. This buys: >260-char paths, trailing dots/spaces, and reserved device names (`CON`, `NUL`, `COM1`…) all handled without special cases. **Network paths are rejected pre-flight (F27):** UNC input (`\\server\…`, `\\?\UNC\…`) and mapped drives whose volume resolves to `DRIVE_REMOTE` fail immediately with a clear `path` error ("bigcp works with local volumes only") — no UNC branch exists anywhere below the boundary (E44).
- **Identity checks, not lexical checks (pre-flight):** lexical path comparison cannot detect aliases (junctions, symlinks, `subst`, mount points). Both roots are opened and resolved via `GetFinalPathNameByHandleW` + volume serial + 128-bit file ID; the run refuses to start if the roots are the same object or one final path contains the other (src==dst, dst-inside-src, src-inside-dst — E19). The root handles are then **held open for the whole run** without `FILE_SHARE_DELETE`, so neither root can be renamed or deleted from under the run.
- **Destination root that does not exist yet (E42):** resolve and pin the *nearest existing ancestor*, run the identity/no-alias checks against it, create the missing components one at a time (each opened with `OPEN_REPARSE_POINT` — never created through an unexpected reparse), then open and pin the new root and revalidate that its final path lies under the pinned ancestor's final path before enumeration begins. **Exception: `--dry-run` creates nothing** — its zero-destination-write promise outranks pinning, so it validates the prospective path against the pinned ancestor and models the absent tree.
- **Stability scope (F16):** both trees are *assumed exclusive and stable* per VISION. bigcp still detects violations wherever detection is free or near-free (§4.8, §4.3, dir-open reparse checks), and its no-write-through-reparse guarantee applies to reparse points **present when bigcp examines a directory** (classification/open time). Mid-run mutations that race between examination and use fall under the exclusivity assumption — best-effort detection, not a guarantee (E36). This boundary is documented in SEMANTICS.md; no user-mode copier can defend a tree against a concurrent writer that the user was asked not to run.
- Internal representation: `Vec<u16>`/`OsString` (native UTF-16, unpaired surrogates preserved). Conversion to UTF-8 happens only for display/log, lossy with `U+FFFD`, and the log additionally records a hex form for non-roundtrippable names (`path_raw`) so the log remains unambiguous.
- Relative paths (source-root-relative) are the tool's universal file identifier — used in logs, reports, the journal, and the destination join. Stored in an arena to keep per-file memory ~2× path length.
- Destination path length is pre-checked: `len(dst_root) + len(rel)` vs. ~32,760 UTF-16 units and per-component 255 vs. the destination FS's max component; violations fail pre-flight with hint `path_too_long`.
- Case: Windows-standard case-insensitive matching for the join (simple Unicode uppercase fold, `CompareStringOrdinal(ignoreCase)` semantics); source case is preserved when creating. Per-directory case-sensitive NTFS dirs (WSL) are a documented edge (§9, E26): the join uses case-insensitive matching regardless, which can merge two source files differing only by case — detected (duplicate join key) and reported as an error rather than silently last-writer-wins.

### 4.6 Symlinks, junctions, mount points (`/SJ /SL` semantics)

- Detected during enumeration via the reparse tag returned in the directory record — **no extra syscall**, and reparse-point directories are never recursed into (this also makes traversal cycles impossible).
- Copy mechanism: open with `FILE_FLAG_OPEN_REPARSE_POINT`, read `FSCTL_GET_REPARSE_POINT`. Then, by tag: **symlinks** are recreated via `CreateSymbolicLinkW` with `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE` (the documented path that honors Developer Mode; raw `FSCTL_SET_REPARSE_POINT` for the symlink tag has unclear Dev-Mode behavior — §3.2), reconstructing target + relative/absolute flag from the reparse buffer; **junctions/mount points** are applied verbatim via `FSCTL_SET_REPARSE_POINT` on the new (empty) directory; **unknown third-party tags** (app-exec links, HSM, ProjFS, WCI, …) **fail by default** as `unsupported_reparse` — a verbatim buffer is not guaranteed meaningful without its owning filter driver, so raw copying requires the explicit `--raw-reparse` opt-in (E31); **cloud-filter tags are never copied as reparse points** (they belong to the cloud minifilter and corrupt off-volume; those files follow the placeholder policy below). Targets are **not** rewritten (robocopy `/SL /SJ` behavior): relative links stay relative; absolute links keep pointing at their original absolute target — documented loudly in README, since a junction copied to another machine may dangle. Dangling sources are fine (we never dereference).
- Symlink creation without Developer Mode requires `SeCreateSymbolicLinkPrivilege`; when neither is available, each symlink fails with hint `enable developer mode or run elevated`. Junctions/mount points need no privilege.
- Volume mount points are treated as junctions (copied as a junction, not recursed) — matches robocopy `/SJ`.
- Cloud placeholders (OneDrive etc., `IO_REPARSE_TAG_CLOUD_*`): these are *data* files, not links. Default: copy (which hydrates/downloads on read) but count them and surface prominently ("N files were cloud placeholders and were downloaded to copy"); `--skip-cloud` excludes them instead. Rationale: VISION requires not missing files; silent mass-download requires informing the user.

### 4.7 Default exclusions (root-level OS artifacts)

When the source is a volume root (only then), the following are excluded by default, each logged as `excluded{reason:system}`: `$RECYCLE.BIN`, `System Volume Information`, `pagefile.sys`, `swapfile.sys`, `hiberfil.sys`, `DumpStack.log.tmp`. Robocopy instead spews access-denied errors on these. `--include-system` restores robocopy behavior. Notification is unconditional (F17): the run banner and `--dry-run` list what *would be* excluded, every actual exclusion is logged individually, and the summary totals them — nothing is ever silently missed. There are **no user exclusion patterns** (no `--exclude` globs): VISION asks only for the system-file exclusion with a disable flag, and a glob engine is exactly the kind of surface the minimal-arguments rule exists to prevent — users who need partial copies copy the subtree they want.

### 4.8 Source and destination stability (exclusivity assumptions, F16)

Per VISION, **both** trees are assumed exclusive and stable for the duration of the run — bigcp does not defend against concurrent writers (no VSS; §2.4). The assumptions are cheaply *policed*, never blindly trusted; every violation is an **error**, not a tolerated condition.

**Destination side:** the policing *is* the commit-safety machinery already required for correctness — `CREATE_NEW` for direct plain-small files, same-handle snapshot validation before their truncation, pre-rename identity revalidation for transactional replacements (§4.3), reparse checks at every directory open (§5.6), and the run lock (§5.12). All of it costs zero additional data I/O; violations surface as `destination_changed` / `type_conflict`.

**Source side:** every detection below adds **zero source data I/O** (handle-based metadata re-queries are served from in-memory filesystem structures; CPU/RAM cost is explicitly acceptable per VISION). Detection overhead carries a measured budget: if post-read revalidation ever costs >2 % on the small-file benchmark (hypothesis H4, §8.7), its default scope narrows to replacements and large files, with a flag restoring full coverage. Hashing whatever bytes happened to be read is not proof of a coherent source version:

- **Vanished** between enumeration and open (`ERROR_FILE_NOT_FOUND`): `failed{category:source_changed, detail:vanished}`.
- **Open-time mismatch:** size/mtime from the opened handle are compared against the enumerated values; mismatch → `failed{source_changed}`, nothing copied (the classification decision was about different content).
- **Post-read revalidation:** after the last byte is read, the source handle's size and mtime are re-queried and compared to the open-time values; a short/long read against the open-time size triggers the same path. The plain-small engine performs this check before destination creation and again after its one write; an early violation writes nothing, while a late violation can leave a direct artifact that the next stable rerun repairs. The transactional engine revalidates after data plus auxiliary work and does not publish its temp on violation.
- Source files are still opened with `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` (maximum compatibility — refusing shared-write opens would turn in-use-but-idle files into spurious failures).
- If any `source_changed` failures occurred, the summary flags the run prominently: "the exclusive-source assumption was violated — quiesce writers and re-run."

## 5. Architecture

### 5.1 Language and platform choice

**Rust.** Reasons, in priority order:

1. **Reliability leverage.** The #1 requirement is "no bugs that corrupt/miss files". Rust eliminates whole classes (UAF, data races, buffer overruns) at compile time, and its `Result`-based error handling makes *ignored error paths* visible in review — the classic copy-tool bug (unchecked `WriteFile` return) becomes unrepresentable under our lint policy (§14.3).
2. **Zero-compromise access to Win32**: `windows-sys` provides complete, machine-generated, officially maintained (Microsoft) bindings; IOCP/overlapped I/O is fully expressible.
3. **Ecosystem fit**: `ratatui` (TUI), `xxhash-rust` (SIMD xxh3 — the single content hash, F30), and `crossbeam-channel` (bounded worker queues) are mature, focused crates.
4. C++ was the runner-up (equal API access, but every reliability property must be earned by discipline rather than the compiler). Python is disqualified for the throughput goal (per-file syscall overhead through the interpreter, GIL vs. many-core, no clean IOCP story).

Toolchain: latest stable Rust, **MSRV pinned in `rust-toolchain.toml`** and CI-enforced; `Cargo.lock` committed; x64 target `x86_64-pc-windows-msvc`, CRT statically linked (`+crt-static`) → single self-contained `bigcp.exe`.

### 5.2 Crate layout (Cargo workspace)

```
bigcp/
├── Cargo.toml
├── crates/
│   ├── win/src/       # the only unsafe boundary: file, metadata, stream, EA,
│   │                  # sparse, reparse, volume/device, path, lock primitives
│   ├── core/src/      # copy coordinator/engines, worker pool, journal, audit,
│   │                  # report, verification, profiles, statistics, options
│   ├── tui/src/       # live dashboard and saved-report browser
│   ├── cli/src/       # clap grammar, prompting, output mode, exit mapping
│   └── testkit/src/   # confined generator, independent oracle, extent reader
├── docs/              # normative/as-built docs, ADRs, and JSON schemas
└── scripts/           # reproducible MSVC launcher and policy checks
```

Dependency direction is `cli → {core,tui}`, `tui → core`, `core → win`, and
`testkit → win`. The oracle never links core copy logic, preserving its
independence (§12.2). `cargo-deny` gates dependency policy; crate manifests and
the workspace build enforce this graph.

The `win` crate exposes documented safe wrappers that preserve Win32 errors.
Every unsafe block carries a local `SAFETY` proof; the other crates deny unsafe
code. Capability differences are explicit data returned by volume/device
queries. NTFS/ReFS and Windows 11 gates eliminate legacy-filesystem and
old-OS compatibility branches, while unsupported optional features degrade
only through the documented capability policy (§4.4).

### 5.3 Process overview and threading model

One coordinator thread owns iterative enumeration, the per-directory join,
classification, journal/audit/report state, exact counters, circuit breaking,
and the sequential large-file loop. It dispatches eligible small files to a
fixed worker set through directory-affine per-worker queues of 1024 metadata
jobs; results return through one bounded channel. Workers own a complete small
copy and never mutate audit or aggregate counters. A large ADS discovered by a
worker is promoted back before destination mutation. Hashing runs inline.
Interactive mode uses one additional scoped thread so the TUI can render
immutable snapshots while the copy runs.

There is no enumeration pool, large-stream pool, finalizer pool, or log-sink
thread. Those earlier designs either never existed or were retired by measured
complexity control. Worker count is the only concurrency knob (§8.2).

**Rule: no unbounded queues anywhere.** Every channel is bounded; producers block (backpressure) rather than balloon memory. Buffer memory budget: bounded per §8.2 profiles, override `--tune mem=`.

**No async runtime.** Tokio is deliberately not used: file I/O on Windows
under tokio is thread-pool-simulated anyway. Synchronous I/O plus the bounded
small-file worker pool is simpler and easier to reason about (ADR 0003).

### 5.4 Data flow and the work item model

`model.rs` defines the single source of truth:

```rust
struct FileEntry {           // produced by enumeration (src) and join (dst side)
    rel: RelPath,            // arena-interned UTF-16 relative path
    size: u64,               // unnamed-stream logical size (per-stream truth arrives at open, below)
    alloc: u64,              // allocation size (sparse detection input)
    mtime: i64, ctime: i64, atime: i64,   // FILETIME units
    attrs: u32,
    file_id: u128,           // commit revalidation (§4.3)
    ea_size: u32,            // free EA presence/inequality signal (§4.1, §4.2)
    reparse_tag: Option<u32>,
}
struct StreamInfo { name: Vec<u16>, size: u64, allocation_size: u64 } // FindFirstStreamW (§4.2)
enum DstState {              // what the join learned about the destination twin
    New,
    Replace { old: OldMeta },        // OldMeta: file_id + size + mtime + attrs — everything
}                                    //   pre-rename revalidation (§4.3) and F13 logging need
enum CopyItem {              // scheduler output
    SmallFile { src: FileEntry, dst_state: DstState },
    LargeFile { src: FileEntry, dst_state: DstState },
    Reparse   { src: FileEntry, dst_state: DstState },
    MetaFix   { src: FileEntry },                        // metadata repair (§4.1)
}
// directories are created synchronously by enumeration tasks (§5.6) — there is no DirCreate work item
enum Outcome { Copied{bytes_all_streams,ms,hash,streams: u32,replaced: Option<OldMeta>},   // F13
               SkippedSame, MetaFixed, Failed{err}, Excluded{why}, NotAttempted{why} }
```

Every `CopyItem` terminates in exactly **one** `Outcome`, delivered to the coordinator. This is the backbone of counter reconciliation (§7.3).

**Streams are first-class in sizing and accounting.** Enumeration knows only the unnamed-stream size; `FindFirstStreamW` discovers the true set after open, followed by source identity revalidation. A zero-byte file may carry a huge ADS, so any stream at or above `large_threshold` promotes the owning file from a worker to the coordinator before destination mutation. Every named stream uses the selected engine's data path; checkpoint records are keyed by relative path plus stream name. Successful logical-byte accounting includes named streams, and verification retains per-stream digests.

**One result/accounting contract, two completion protocols.** Both engines return `EngineResult` with exact byte, digest, stream/EA degradation, journal, EFS, and checkpoint facts. Plain small files complete through direct final-name writes; ADS/EA, sparse, and large files complete through temp publication. Only the coordinator converts either result into one terminal outcome, counters, audit, and verification work. This shared ownership — not a fictional shared finalizer — prevents semantic drift.

### 5.5 Device profiler

Runs once at startup (per distinct volume), before any copy I/O; results go into the log, the report, the Devices tab, and the tuning tables.

1. Volume: `GetVolumePathNameW` → `GetVolumeInformationW` (FS name, capability flags, max component) — **the F15 gate runs here: FS name not NTFS/ReFS → fatal exit 5 before any tree I/O** (§4.4) → `GetDiskFreeSpaceExW` (free space) → `GetDiskFreeSpaceW` (cluster size) → `GetDriveTypeW` (`DRIVE_REMOTE` → reject, F27).
2. Volume→disk: open `\\.\C:` with `dwDesiredAccess = 0` (query-only; works without admin) → `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` → physical disk number(s).
3. Query the same zero-access volume handle with `IOCTL_STORAGE_QUERY_PROPERTY`:
   - `StorageDeviceSeekPenaltyProperty` → HDD vs. SSD,
   - `StorageAdapterProperty` → `BusType` (USB / NVMe / SATA / RAID…), `MaximumTransferLength`, `MaximumPhysicalPages`,
   - `StorageAccessAlignmentProperty` → logical/physical sector size (4Kn vs 512e),
   - `IOCTL_DISK_GET_CACHE_INFORMATION` → device write-cache state, query-only — deleted as cosmetic in ADR 0027 and **revived by measurement in ADR 0032** (~3.4× small-file impact drives the pre-copy notice and the report hint). Vendor/model strings and hotplug-policy inference stay deleted: they informed cosmetic Devices-tab lines only.
   - `MaximumTransferLength` clamps the chosen chunk; no BOT/UASP concurrency inference is claimed.
4. Shared-disk detection: source and destination disk-number sets intersect; the fact is reported but does not select a separate scheduler (§8.3).
5. **Fallbacks and epistemic honesty:** USB bridges routinely fail or lie on these IOCTLs. Query failure selects the conservative Unknown profile (4 workers, 4 MiB chunks) and low confidence. A positive no-seek-penalty answer on an unrecognized VMD/RAID bus selects the moderate SATA-SSD row rather than discarding trustworthy media evidence. `--profile`/`--tune` override static choices (§10.1). Same-volume ReFS block-refcounting is used only for an OS-copy-engine hint (F28).

**There is no free-space forecast.** An earlier revision specified a conservative shortfall-range estimator here; it was deleted as complexity without a matching payoff — cluster rounding, replacement double-occupancy, and sparse savings make any figure approximate, and an approximate warning changes nothing about the outcome. The disk-full circuit breaker (§5.13) is the one authoritative stop: the run halts resumably with clear guidance the moment space actually runs out, which is also exactly what a warned user would have had to do anyway (free space, rerun).

### 5.6 Enumeration and destination join

Design goals: saturate metadata IOPS on SSDs, avoid seek-thrash on HDDs, never stat destination files one-by-one, start copying while still discovering.

- **Unit of work = one directory frame.** The coordinator processes an explicit iterative stack. Each frame:
  1. Opens the source dir (`FILE_LIST_DIRECTORY | SYNCHRONIZE`, `FILE_FLAG_BACKUP_SEMANTICS`) and enumerates with `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` into a 256 KiB buffer, looping until done. One handle, few syscalls, and each record carries size/times/attrs/reparse-tag plus the 128-bit file ID (used for commit revalidation, §4.3) — everything the classifier needs, **no per-file `CreateFile`/stat**. Hard links are *not* detected or counted (VISION): each linked file is simply copied as an independent file, and no cross-walk ID set exists.
  2. Opens the *destination* twin dir with `OPEN_REPARSE_POINT` — if it turns out to be a reparse point where the source has a real directory, the subtree fails `type_conflict` (§4.5; bigcp never writes *through* an unexpected reparse point). If present, enumerates it into a per-directory hash map (case-folded name → `FileEntry`). This is the **join**: one listing replaces N per-file existence checks. Missing dst dir → **created here, synchronously, before any child work is emitted** (attrs applied; logged `dir{action:created}`). Creation failure → the source subtree is **still enumerated** (read-only) so every descendant is discovered and accounted `not_attempted{parent_dir_failed}`, each item logged, counters intact — one-pass accounting needs no pre-knowledge of the subtree. This structural ordering is the guarantee that no file item can reach an engine before its parent exists — no scheduler priority rule needed.
  3. Registers per-directory outstanding work before dispatch. Small-file completions decrement that exact directory; the exit frame yields by consuming one completion until its own work settles, then applies directory streams/EAs/metadata post-order.
  4. Classifies every source entry (§4.1), dispatches eligible small files to the directory-affine worker, processes large/reparse/meta work on the coordinator, records extras left in the destination map, and pushes child exit/enter frames.
- Deep trees are iterative: frames, never Rust recursion, hold the current traversal state.
- **Known pre-1.0 gap:** each source listing and destination name map is materialized for one directory, so metadata memory is proportional to the largest directory. The earlier budgeted two-pass fallback is not implemented. Million-entry behavior requires a synthetic enumeration harness before certification; LIMITATIONS.md states the operational boundary.
- Enumeration has no independent pool or I/O-priority manipulation. The coordinator naturally backpressures on bounded worker queues and handles large files inline.
- Progress semantics: totals are "discovered so far"; the TUI shows a discovery ticker until enumeration drains, and the ETA model (§6.4) reports a lower bound until then.

### 5.7 Scheduler

Receives classified `CopyItem`s, dispatches to engines. Policies:

- **Size classes:** a worker first accepts files whose enumerated unnamed size is below the 16 MiB default threshold; stream discovery promotes hidden-large-ADS or otherwise ineligible work before any write. Reparse/meta items stay on the coordinator.
- **Locality:** the parent directory hashes to one worker. Same-directory creates serialize while distinct directories proceed in parallel; every queue remains bounded.
- **Interleaving:** the coordinator copies one large file/stream at a time. Already-dispatched small jobs may remain in flight; there is no device-specific runtime throttle or parallel-large scheduler.
- **Ordering:** parent existence is guaranteed structurally by the directory frame; no scheduler priority rule exists.
- **Circuit breaker:** five consecutive device-gone/disk-full outcomes with no intervening success stop further dispatch and produce resumable exit 4 (§5.13).

### 5.8 Small-file engine

One file per worker at a time, synchronous buffered I/O — the OS cache manager is *good* at this (write-behind batches small writes; unbuffered I/O would force per-file alignment handling and sync flushes and is measurably slower for < ~1 MiB files; §3.2, §8.1).

**Scheduling is directory-affine, and this is the measured heart of the small-file win (2026-07-29 evidence — do not regress any of the four parts):** NTFS serializes same-directory creates on the directory index regardless of caller parallelism (measured: ~2 ms/create with 64 interleaved workers, ~0.5 ms directory-serialized), so (1) each file is dispatched to the worker chosen by hashing its parent directory — same-directory creates form a clean pipeline on one worker while distinct directories run truly parallel; (2) per-worker job queues are deep (~1024 metadata-only records, bounded) so the single-threaded coordinator races ahead across sibling directories instead of stalling on the one being enumerated; (3) the coordinator's per-entry path does **no blocking work** (no probes, no revalidation — moved to workers; measured 4 µs/entry); (4) a directory's post-order timestamp step waits only for *its own* outstanding files (per-directory counters) and **yields** — re-queuing itself after draining one completion — whenever its files are still in flight, because a global or immediate drain measurably re-serialized the run directory-by-directory. Combined result: 1.3–1.5× robocopy `/MT:32` on the bounded small-file workload with default settings (BENCHMARKS.md). Removing any one part restored a bottleneck in measurement: interleaved dispatch → create convoy; shallow queues → coordinator stall; immediate exits → one directory at a time.

Per file: open source non-following/read-only → validate enumeration metadata → discover streams with `FindFirstStreamW` and revalidate identity → read EAs when present → route any ADS/EA, sparse, checkpoint-capable, or large file to the transactional path (ADR 0034) → for a remaining plain small file, read the unnamed file into one fallibly reserved buffer (≤ threshold) → revalidate source → create the final name exclusively or open the exact replacement non-following, validate the classification snapshot on that same handle, then truncate → write the one unnamed payload → revalidate source → optional flush → close → return `EngineResult`. The coordinator alone records the outcome.

There is no finalizer/closer stage. Each worker owns the entire file, which measurement showed is already enough to overlap destination close latency. The hot-path rules are: no coordinator stream probe, one final-name destination open, one whole-buffer unnamed write, no hash unless verification requests it, and no extra metadata update when create-time stamping is safe. Anything added requires an isolated benchmark.

### 5.9 Transactional streaming engine (auxiliary-data, sparse, and large files)

The throughput core, deliberately shaped by the complexity rule: one **stream** = one large file copied by a **synchronous sequential chunk loop** — read a large chunk, hash it, write it, repeat. The read/write overlap a hand-built pipeline would provide comes from the OS cache manager instead: sequential buffered reads trigger read-ahead (the next chunk is usually in RAM before the loop asks), and buffered writes complete into write-behind (the device stays busy while the loop turns around). Two successively simpler designs were specified here and deleted, each on the same rule: first an IOCP overlapped ring (adversarial completion-order machinery for throughput no target class was shown to demand), then an unbuffered reader/writer thread pair (dropped when the owner clarified that robocopy-`/J` unbuffered semantics were never a mandate — ADR 0028; the plan is free to choose the simplest mechanism that meets the performance goals, and must prove anything fancier with a benchmark). Unbuffered I/O may return post-v1 only behind an isolated bounded benchmark demonstrating a material shortfall of the buffered loop; its known trade — file-cache churn during huge copies — is declared plainly in LIMITATIONS.md and is mildest on exactly the large-RAM machines VISION targets. Structure:

- Source and destination are plain synchronous handles (destination created via the temp-name protocol); chunks come from the class profile (§8.2), clamped to the adapter's MTL. **No sector-alignment machinery exists anywhere** — buffered I/O needs none, which deletes the alignment helpers, padded-tail handling, and their test surface outright.
- **Hashing is trivially offset-ordered:** reads are sequential, so the rolling xxh3 follows the read cursor directly, and a checkpoint watermark `W` is recorded only once writes are contiguous through `W`.
- Destination preallocation (**dense path only**): `FileAllocationInfo` (round up to cluster) at open → contiguity and no mid-write allocation stalls; exact `FileEndOfFileInfo` at completion. `SetFileValidData` is **prohibited** (F18, VISION): it requires special privileges and can expose stale disk data, and with strictly in-order writes the VDL zero-fill cost it would avoid is already ~zero (§8.1). There is no flag to enable it and no call site for it — a code-review/grep guard keeps it that way.
- **Sparse path** (source allocation < logical size, destination supports sparse, `--no-sparse` not set — §4.2): `FSCTL_SET_SPARSE` on the temp first, logical EOF set, **no full preallocation** (it would materialize the holes the path exists to avoid); allocated ranges written in offset order; holes are hashed from a reusable zero page (CPU-only) so the digest stays the *logical* digest, and the write watermark advances through holes instantly.
- Rolling hash: xxh3 over source bytes is **always** computed in flight (§5.11, offset-ordered as above) — it feeds checkpoint integrity and gives every streamed file a logged digest for free.
- Checkpoints — **only for streams ≥ the checkpoint threshold (default 16 GiB, `--tune checkpoint-threshold=`)**, and **tentative by design**: no `FlushFileBuffers` accompanies a checkpoint. Because resume *always* re-reads and digest-verifies the temp prefix (§5.12), the journal is allowed to run ahead of what survived a power loss — a short or mismatching temp simply restarts the file from zero. This deletes the periodic device-cache flush entirely (a real throughput cost on many drives) while keeping integrity exactly as strong: verification, not flush ordering, is the guarantee (I13). Interval: **deterministic size tiers** from 256 MiB up to 4 GiB, chosen by stream size (a throughput-scaled interval was considered and dropped: it would be a misleading pseudo-adaptation, and determinism is worth more). **Digest snapshots are taken at exact boundaries:** the hash cursor runs ahead of the write watermark, so recording the *live* hasher at watermark time would hash bytes beyond `W`. Instead, when `next_hash_offset` crosses a checkpoint boundary `B`, the hasher state is cloned (xxh3 state is tiny) and retained; the checkpoint for `W = B` is emitted once contiguous writes reach `B`, carrying `snapshot(B)` — the digest of exactly `[0, B)`, never of "whatever was hashed so far".
- There is exactly one coordinator-owned large-file loop. The removed `streams` knob never affected execution and is intentionally absent (ADR 0033). **There is no runtime governor** (F29): static chunks/workers/thresholds are chosen once, and a misbehaving bridge is the device breaker's job (§5.13).
- **No same-volume clone path (F28):** bigcp always streams. When the profiler detects same-volume ReFS with block-cloning support, it emits the permitted hint that the OS copy engine (Explorer/robocopy) would clone near-instantly for same-volume duplication — informing, not implementing (§5.5).

### 5.10 Meta engine and directory timestamps

- Executes `Reparse` (§4.6), `MetaFix` (metadata repair, §4.1), and `DirStamp` items (directory creation itself happens synchronously in enumeration tasks, §5.6).
- **Directory terminal outcome:** a directory finishes as `dir_done` only after its `DirStamp` (post-order timestamps + masked attrs + dir-ADS/EA when present) succeeds; a created-or-existing directory whose stamp/ADS/EA finalization fails terminates as `dirs_meta_failed` — a distinct counter in the §7.3 equation, so "the tree copied but some directory metadata didn't" is visible, not absorbed.
- **Dir-completion tracker:** every directory holds a countdown of (children dirs not yet complete + own pending file items). When it hits zero, a `DirStamp` item (set the directory's timestamps+attrs from the source entry) is emitted — a rolling **post-order pass** that requires no end-of-run tree walk and works with streaming enumeration. The destination root is stamped last, at run end.
- Existing destination directories get their timestamps/attrs corrected only if they differ (no-op writes avoided, §8.4).

### 5.11 Hash pipeline

- Algorithm: **xxh3-128, the single hash used everywhere** (F30) — SIMD, >20 GB/s/core; there is no algorithm option and no second implementation to keep consistent.
- Plain small files and sub-threshold transactional ADS/EA files hash only with `--verify`; the default common path spends zero CPU on it (and per VISION there is no separate hash-recording option). **Large/checkpoint-capable streams always use xxh3** — expected cost well under 2 % of one core per GB/s (hypothesis H3, §8.7), required for checkpoint integrity (F12, §5.12), and lands a full-file digest in the log/journal at zero extra I/O. All hashes are computed from buffers the engines already hold (no extra reads), in **file-offset order** (§5.9).
- Honesty about hash strength (documented in README/SEMANTICS): xxh3-128 equality is overwhelming statistical evidence against *accidental* corruption and omissions — exactly the VISION threat model — not cryptographic proof against deliberate tampering, which is explicitly out of scope (F30).
- The verify pass and `bigcp verify` (§5.17) reuse the same engines/profiler for reading — one read path in the codebase, exercised by all features.

### 5.12 Journal, resume, and run exclusivity

State directory: `%LOCALAPPDATA%\bigcp\state\<16-hex of SHA-256(final_src|final_dst)>\`, keyed by the **resolved final paths** (§4.5; override `--state-dir`). Contains `journal.jsonl` plus per-run `run-<ts>.log.jsonl` / `run-<ts>.report.json` (defaults; `--log/--report` may point elsewhere, including the source *drive* — the only writes ever allowed toward source, per VISION). **Audit paths may never lie inside either tree:** `--state-dir`, `--log`, and `--report` resolving under the source or destination *root* are rejected pre-flight (a log under SRC would mutate the "stable" source and get enumerated/copied by its own run; under DST it violates exclusivity and shows up as an extra) — same physical volume is fine, same tree is not (E46). Retention: artifacts of the last 10 runs are kept, older ones pruned; the journal is compacted to its header on every clean run end.

**Run exclusivity (I12, F26).** Before any tree I/O: acquire the named mutex `Global\bigcp-<hash of case-folded final dst root>` (created with an explicit DACL permitting all users to open it, so the exclusion is genuinely machine-wide across sessions). Held for the run's lifetime; acquisition failure → refuse to start (exit 5), naming the conflict. Scope is deliberately **exact-root only**, per VISION F26: nested/overlapping destination roots are *not* detected — they fall under the destination-exclusivity assumption (F16) and the documented limitation in SEMANTICS.md. (The previous design's cross-run overlap registry was deleted on this basis: exact-root is what the vision asks to enforce, and a registry scan cannot be made atomic without a coordinator lock it doesn't need.)

**Resume model — two layers:**

1. **Idempotent re-run (the workhorse).** Completed files are *not* trusted from the journal — a re-run re-enumerates and the skip heuristic (§4.1) classifies them Same in microseconds per file with zero data I/O. Robust against anything that happened to the destination between runs (user deletions, other tools) — the journal can never go stale in a harmful direction (I8).
2. **Checkpointed large-file resume (F12) — tentative watermarks, verified resume.** Per checkpoint the journal carries `{rel, stream, temp, src_size, src_mtime, watermark W, prefix_digest = snapshot(B=W) (§5.9)}`. Checkpoints are **hints, not durability claims**: no destination flush accompanies them (deleted — Rec. adopted from review; it was a per-interval device-cache flush bought for a guarantee verification already provides). The journal may therefore legitimately run ahead of data that survived a power loss.
   **Resume protocol — always verified, never a blind watermark trust (F12, I13):** the candidate must match the *current* source on `(size, mtime)` and the temp must exist with size ≥ W. The temp's `[0, W)` is then re-read via the streaming engine and its digest compared against the journaled `prefix_digest`. Equality establishes byte-identity with overwhelming confidence against accidental corruption (128-bit xxh3, §5.11) — and legitimizes continuing the rolling hash from that state, so the final whole-file digest remains a true *source* digest. Any mismatch — source changed, temp short or missing, digest differs, journal torn — restarts the file from zero, safely. The promise, stated exactly: **after process termination, resume from near the last checkpoint; after power/device loss, verify-and-resume when the data survived, otherwise restart safely.** Cost model: a 900 GB file killed at 80 % costs a 720 GB verify-read + 180 GB copy instead of a 900 GB copy, and the resumed portion is *verified* — stronger than what a fresh copy claims about its own written bytes.

Journal hygiene: append-only JSONL; every record CRC-tagged; a torn tail is detected and everything from the tear on is dropped. Journal append failure at runtime → checkpointing disabled for the run (copies continue; large files lose resume; warning + audit note per §5.15). **Temp lifecycle (§4.3):** in-flight temps carry a pending delete disposition, so ordinary process kills leave none; resumable partials (≥ checkpoint threshold, disposition cleared at first checkpoint) are described by their checkpoint records. Invalid checkpoint candidates are discarded only through their identity-verified open handle. Power-loss or named-stream-window stragglers that are not live checkpoint candidates are reported as extras with a cleanup hint, never auto-deleted by path (I2/ADR 0027). `--fresh` ignores the journal and restarts under the same temp-safety rules.

### 5.13 Error handling

- Every failure produces `Outcome::Failed{err}` carrying: Win32 code, message, the operation (open-src / read / create-dst / write / rename / set-meta / …), and the relative path. Nothing is ever retried, and there are **no retry arguments** (VISION): the breaker model plus cheap idempotent re-run *is* the retry mechanism, without retry loops entangling the revalidation/temp protocols.
- **Classification** (`errors.rs`, table-driven — the single place error codes are interpreted; ERRORS.md is generated from it, §14):

| Category | Example codes | Hint shown |
|---|---|---|
| `permissions` | 5 `ACCESS_DENIED` | "Check/repair ACLs on <path> (or take ownership), or run elevated" — no backup-privilege mode exists (VISION) |
| `locked` | 32 `SHARING_VIOLATION` | "In use by another process — close it and re-run" (no lock-owner lookup: a Restart Manager integration was specified and deleted; the hint's job is done without naming the process) |
| `path` | 3, 206, name too long | "Enable Win32 long paths / shorten destination root" |
| `space` | 39/112 `DISK_FULL` | "Free destination space, then re-run to resume" |
| `media` | 23 `CRC`, 1117 `IO_DEVICE` | "Source device reported hardware read errors — check the drive (chkdsk / SMART)" |
| `device_gone` | 433, 1167 `DEVICE_NOT_CONNECTED` | "Device disconnected — reconnect and re-run to resume" |
| `fs_limit` | missing volume capability (ADS/EA/sparse/EFS on this ReFS version) | per-case hint (§4.4) |
| `source_changed` | vanished at open; open-time or post-read metadata mismatch (§4.8) | "Source was modified during the run — quiesce writers and re-run (exclusive-source assumption, F16)" |
| `destination_changed` | `CREATE_NEW` collision; pre-rename identity mismatch (§4.3) | "Something else is writing to the destination — nothing was overwritten; re-run to reconcile" |
| `unsupported_reparse` | unknown third-party reparse tag (§4.6) | "Tag 0x… has no meaning without its owning filter; use --raw-reparse to copy the raw buffer at your own risk" |
| `parent_dir_failed` | subtree not attempted after dir-create failure (§5.6) | inherits the parent directory's error hint |
| `type_conflict` | file vs dir vs reparse mismatch (§4.1) | "Resolve the conflicting destination object manually; bigcp will not delete or write through it" |
| `cloud` | hydration failures (0x8007016A family) | "OneDrive placeholder could not be downloaded — check connectivity / --skip-cloud" |
| `internal` | anything unexpected | "This is a bigcp bug — please file the log" (and exit code 6 if an invariant broke) |

- **Circuit breaker** (prevents 100,000-error cascades): five consecutive `device_gone`/`space`-class failures with no success in between trip the breaker — no new objects are dispatched, in-flight work drains, everything remaining is accounted `NotAttempted{breaker}`, and the run **aborts resumably** with exit code 4 and clear "reconnect the device / free space, then re-run to resume" guidance. The streak deliberately survives interleaved failures of other categories (a removed device surfaces mixed error codes) and resets only on a genuine success. Abort-and-rerun is the *only* recovery model (F31) — there is no in-run reconnect flow to implement, display, or test.
- Error tally is live in the TUI (§11): counts by category × top-level folder, navigable to per-file detail.

### 5.14 Stats and bottleneck analysis

- Byte counters accumulate per completion; the coordinator rolls them into windowed application-side read/write rates (30-second cadence normally, 5-second with `--analyze`). An earlier revision specified per-I/O latency accumulators, per-device IOPS/p99/queue-occupancy — deleted: application-visible rates answer the user's actual question ("how fast, and when was it slow") without a measurement subsystem to maintain.
- Bottleneck **hypotheses** (not verdicts) per window and for the whole run: read-rate versus write-rate separation yields `source-bound`/`dest-bound`/`balanced`, with near-tied evidence reported as `balanced (low confidence)` rather than a confident wrong answer — application-side rates are a proxy, not physical device utilization, and the report says so. The report stores the downsampled timeline plus phase extremes (fastest/slowest active windows).
- **"Best observed sustained throughput"** (VISION's own term, F10): the best sustained 5 s window on the bottleneck device during this run — no probing options exist; throughput observed during the run is the sole basis. Efficiency = run average ÷ best observed sustained. The report labels the number's provenance explicitly; no theoretical maxima are fabricated.
- Disk temperatures are **not** monitored (F25, VISION): drive slowdowns are communicated through the throughput-signature detectors below, which capture the user-visible consequence (thermal throttling looks like an SLC-cliff-style sustained-rate drop) without the IOCTL surface.
- **`--analyze` (F35):** bounded live-run insight for production analysis. Per-file wall-clock timing aggregates into **five fixed size-class buckets** and a **top-20 slowest-copies table**, and the stat cadence tightens from 30 s to 5 s (the 3600-point cap still bounds the report). Output is exactly one `analysis` JSONL event plus one optional report section — no per-file log growth. Off by default; the shared phase tracker still performs two relaxed atomic additions per timed operation, while analysis allocation/aggregation stays off.
- Pattern detectors emit hints (§11 Hints tab): sustained dst-throughput cliff after tens of GB (typical SLC-cache exhaustion / SMR behavior → "expected on this class of drive, not a bigcp or cable problem"), high dst latency with low throughput on small files (AV filter → "consider a Defender exclusion for the destination during bulk restore"), `discovery-bound` verdict (→ "source metadata is the limit; nothing to tune").

### 5.15 Logging and reporting

Split by audience: the **log** (JSONL, event per line, complete) is for machines and post-mortems; the **report** (single JSON, aggregated) is for humans via `bigcp report` and for dashboards. Formats in §10; versioned schemas shipped in `docs/schemas/` and treated as public API (additive changes only within v1).

**Audit-record safety** (the log and report *are* bigcp's claims, so they get guarantees of their own): the report is written temp → flush → rename; `run_end` is appended and flushed to the log before the summary prints and the process exits.

**Audit-failure policy (F7/I7 coherence):** the machine-readable log is not optional decoration — F7 requires it and I7 promises every failure lands in it, so a run that can no longer log must not keep making claims. On a log-write failure: (1) one reopen attempt on the same path; (2) failover of the log stream to the state directory (a different device in the common case), noted in the log itself and stderr. Only if **both** fail: stop dispatching new work, drain in-flight operations, write the best-available report + stderr summary, exit 6 with `audit:"failed"` — completed work remains valid and resumable; bigcp just refuses to continue un-audited. Lesser degradations that lose no per-file events (e.g., journal failure → checkpointing disabled, §5.12; report unwritable → state-dir fallback path printed) mark `audit:"degraded"` and continue. "Infallible by buffering" is not a property this design claims anywhere.

### 5.16 CLI

Full grammar in §10.1. Subcommands: `bigcp SRC DST [flags]` (copy), `bigcp verify SRC DST`, `bigcp report FILE`. Robocopy flag mapping in Appendix A.

### 5.17 Verify mode

Exactly two forms, per VISION (F32) — no sub-modes, no variants:

- **`--verify`** (post-copy pass, same run): waits for copy completion, then re-reads the **destination files this run wrote** — all streams, EAs, masked attributes, creation and last-write times — comparing content against the digests computed during the copy read (with `--verify` on, small files are hashed in flight too, so every copied file has one). Scope is deliberately **files only**: directories and reparse objects this run created are verified by the standalone form (their full coverage there keeps one object-verification implementation per form instead of a second run-local one). Files this run *skipped* are also the standalone mode's job. **Read-back caching, honestly:** read-back is buffered **by design** (unbuffered read-back was deleted with the unbuffered engine, ADR 0028), so a post-copy verify of freshly written data may be partially served from RAM — it still catches logic/hash/truncation mistakes, and the authoritative cold-cache check is a standalone `bigcp verify` run later (documented in LIMITATIONS.md).
- **`bigcp verify SRC DST`** (standalone): enumerate+join both trees; report missing/extra/type-mismatch; hash **both** sides in full (full read of both trees). Always authoritative — no cached-hash shortcuts, no metadata-only mode: a cache keyed by size+mtime would reintroduce exactly the false-negative class (content changed, size and mtime preserved) that full verification exists to catch.

**The object-contract matrix (shared by both forms and the oracle — one semantic definition, independent implementations, §12.2):**

| Object | Verified |
|---|---|
| Files | unnamed + named stream sets (names, sizes, contents); EA blobs when `EaSize ≠ 0` on either side; masked attributes (§4.2); creation + last-write on exact 100 ns equality |
| Directories | existence; masked attributes; creation + last-write (post-order-stamped); directory ADS and EAs when present; the destination *root's* own metadata included |
| Links (symlinks/junctions) | reparse tag + full reparse buffer/target verbatim |
| Tree shape | missing objects, extra objects, extra/missing *streams*, type conflicts. Extra destination-only streams on surviving objects are **reported as divergence, never deleted** — the no-delete contract (I2) outranks stream-set convergence; exact convergence of a pre-existing directory carrying foreign ADS requires manual resolution |
| Last-access (all objects) | **reported separately, informational only** (F34) — it legitimately drifts, so never pass/fail, never silently ignored |
- Verify results land in the same report structure (`verify` section): pass/fail counts, mismatched files (these are *serious* — flagged in red, with the guidance that a mismatch after a clean copy indicates hardware/FS problems).
- Honesty note (documented in README and report): same-run read-back may be served from the OS cache, and no read-back defeats the drive's internal DRAM cache entirely; verify catches bus/FS/logic corruption reliably, media decay only as well as the drive lets it — a later standalone verify (cold cache) is the strong form. `--verify` costs one extra read of the copied bytes — the report prices it in advance in the plan line.

## 6. Key algorithms

Pseudocode is normative for control flow; error handling (every call can fail → `Outcome::Failed`) is elided for readability but mandatory in implementation.

### 6.1 Run orchestration (coordinator)

```
run(opts):
  paths   = canonicalize(opts.src, opts.dst)          # §4.5 lexical form
  roots   = open_and_pin_roots(paths)                 # §4.5 identity checks (final paths + file IDs);
                                                      #   src==dst / nesting → fatal; UNC/non-NTFS/ReFS → fatal
                                                      #   (F27/F15); handles held all run
  lock    = acquire_run_lock(roots.dst)               # §5.12: exact-root machine-wide mutex (F26); held → exit 5
  devices = profile(paths)                            # §5.5
  journal = Journal::load_or_new(state_dir(roots))    # §5.12
  plan    = announce(devices, journal.resumables())   # log run_start; TUI up
  create bounded directory-affine small-worker set
  iteratively process directory enter/exit frames:
    join/classify entry
    dispatch small work or copy large/reparse/meta inline
    consume worker outcomes for backpressure and directory completion
    after each outcome: counters.apply; journal.maybe_record; log.emit; stats/tui.publish
    breaker or user cancel → stop dispatch, drain workers, finish report resumably
  drain all workers and finalize directories/root post-order
  meta.stamp_root_dir()
  if opts.verify: run_verify_pass()                   # §5.17
  counters.reconcile_or_die()                         # §7.3 — invariant breach ⇒ exit 6
  report.write; journal.finalize; summary.print
```

Cancellation through `q`, Esc, or Ctrl-C is graceful: stop dispatch, let small
jobs finish, poll between large chunks, preserve eligible checkpoints, write
the report, and exit 3. There is no second hard-cancel state.

### 6.2 Directory task (enumeration + join)

```
process_dir(frame):                             # coordinator thread
  src_entries = enum_dir(frame.src_handle)      # FileIdExtdDirectoryInfo, 256 KiB batches
  dst = open_dir(frame.dst_path, OPEN_REPARSE_POINT)
  if dst is reparse-point:  fail_subtree(type_conflict); return          # §4.5 — never write through it
  if !exists(dst):
      dst = create_dir(attrs = src_attrs)       # synchronous, BEFORE any child work (§5.6)
      on failure: account_subtree(not_attempted{parent_dir_failed}); return
  dst_map = enum_dir(dst) into casefold-keyed map
  tracker.register(frame, own_small_jobs = 0)  # before dispatch
  for e in src_entries:
    if excluded(e): count excluded; continue
    match e:
      Dir if !reparse   → push exit(frame), then enter(child)
      Dir|File reparse  → copy/account inline
      File small        → send to parent-affine worker; tracker += 1
      File large        → copy/account inline
      File same/metafix → account or repair inline
  for leftover in dst_map: count extra; log extra{rel}
  on exit(frame): consume completions until own_small_jobs == 0; finalize metadata
```

### 6.3 Streaming copy state machine (per stream)

```
states: Opening → Loop (read → hash → write, per chunk) → Streams → Finalizing → Done|Failed
Opening:  open src (sequential buffered); open-time metadata check (§4.8); stream discovery
          create dst temp `.bigcp-…` CREATE_NEW; set pending-delete disposition (§4.3)
          sparse source ∧ dst-capable ∧ !--no-sparse ? FSCTL_SET_SPARSE + set EOF (no preallocation)
                                                     : FileAllocationInfo(dst, roundup(size))     # §5.9 two paths
          if resume_candidate: journal matches current src(size,mtime) ∧ temp size ≥ W
              → re-read temp [0,W); digest == journaled prefix_digest ? continue at W : restart at 0   # §5.12
Loop:     ReadFile(chunk) → xxh3.update(chunk) → WriteFile(chunk); watermark W advances contiguously
            # sparse path: holes are never read — hashed from a reusable zero page, written as nothing
          hash cursor crosses boundary B → snapshots[B] = xxh3.clone()   # exact-prefix digest (§5.9)
          stream ≥ checkpoint_threshold ∧ W reached B → journal.append{W=B, prefix_digest=snapshots.take(B)}
                                                        # tentative — no temp flush (§5.12, I13)
          graceful-cancel probe between chunks (§5.13) → close, temp self-deletes or resumes later
Streams:  remaining named streams ≥ threshold copied the same way into temp:stream (§5.4)
Finalizing: revalidate source stability (§4.8) — changed → close (temp self-deletes), emit Failed{source_changed}
            if Replace: revalidate target identity + preserve explicit DACL onto temp (§4.3)
                        — changed → old file kept, temp self-deletes, emit Failed{destination_changed}
            clear delete disposition; rename temp→final (ReplaceIfExists | IgnoreReadonly [| Posix if probed], §4.3)
            FileBasicInfo(timestamps + masked attrs)     # after rename — tunneling, §4.3
            [--flush → FlushFileBuffers]                 # after rename+meta so the flushed state is final (§7.5)
            close both; emit Copied{hash, streams, replaced: OldMeta if Replace}   # F13
```

I/O errors in any state → cancel outstanding ops for the stream, delete nothing (temp stays for resume unless the error implies corruption of the temp itself, in which case journal entry is dropped so resume restarts it), emit `Failed`.

### 6.4 ETA model

Deliberately simple: remaining known work = bytes enumerated so far minus bytes settled by a terminal outcome; `ETA = remaining / current write rate`. The figure is labeled as covering work discovered *so far* (discovery streams, so it grows), and it is suppressed while writes are idle — a skip-heavy rerun settles files without writing, and dividing by a near-zero write rate would display nonsense. A dual-rate `max(bytes_r/B, files_r/F)` model was considered and dropped: it needs a per-file-overhead estimator for at most a marginally better number.

### 6.5 Static profile selection (no adaptive tuning — F29)

```
select_profile(side):                       # once per volume at startup; never changes mid-run
  class = classify(seek_penalty, bus, mtl, confidence)   # NVMe | SATA-SSD | USB-SSD | HDD | Unknown
  p     = PROFILE_TABLE[class]              # §8.2 — chunk and worker count
  p.chunk = min(p.chunk, mtl)               # never exceed adapter limit
  apply user overrides (--profile forces class; --tune pins individual keys — §10.1)
  log profile{class, values, confidence}; show in Devices tab
```

Per VISION, settings come from the class table at startup and flags are the manual override. **There is no runtime governor.** The shipped synchronous large path has no stream-count or queue-depth control; those non-functional fields were removed in ADR 0033. The device breaker (§5.13) stops resumably rather than tuning around failing hardware. Chunk size, worker count, memory cap, and thresholds never change mid-run, so *same devices + same inputs → same behavior* holds.

## 7. Reliability and failure-mode design

### 7.1 Invariants (each maps to tests, §12)

| # | Invariant | Enforcement |
|---|---|---|
| I1 | Source is opened with `GENERIC_READ` only, always | single choke-point `win::file::open_source()`; grep-guard test asserts no other source-open site exists |
| I2 | No code path deletes a destination file bigcp did not create this/previous run | temporary deletion is handle-bound delete-on-close; resume mutation/deletion requires journaled temp identity; there is no arbitrary path-delete fallback |
| I3 | Transactional replacement never truncates in place; plain-small replacement truncates in place **by design** (ADR 0030 — the rerun contract covers the single-payload interruption window, and in-place overwrite preserves the destination's security descriptor). ADS/EA and sparse files are transactional regardless of size (ADR 0034) | destination writes flow only through `bigcp-win`'s two writer primitives: `DestinationTemp` (transactional) and `DestinationFinal` (plain small) |
| I4 | `copied` is reported only after all required data/auxiliary writes, source revalidation, final metadata, optional flush, and handle completion | `EngineResult` is returned only after the selected plain-direct or transactional completion path finishes |
| I5 | **Run-level convergence (ADRs 0030/0031/0034):** a plain direct write is truncated only after same-handle identity validation, so a kill before its single unnamed write completes leaves a shorter file; every multi-part logical file publishes only via temp+rename. Re-running converges unfinished work to the source under the stable-tree assumption | direct-write rerun and timestamp-freeze tests, auxiliary-data atomic-routing e2e coverage, and future bounded chaos/oracle gate |
| I6 | Counters reconcile exactly per the §7.3 equations (disjoint outcomes, per-universe, files and logical bytes) | coordinator assert at run end; violation ⇒ exit code 6 + `internal` error in report |
| I7 | Every failure is logged with path + code + operation; a run that can no longer log stops making claims | `Outcome::Failed` carries all three by construction; audit-failure policy §5.15 (reopen → failover → drain-and-exit-6) |
| I8 | Journal never causes a skip that metadata wouldn't also justify | journal powers only prefix-verified checkpoint resume (§5.12), never "done" skips and never verification shortcuts |
| I9 | Copy-data buffers and worker queues are bounded; directory-join metadata is the documented pre-1.0 exception | bounded channels, fallible small-buffer reservation, chunk cap; single-directory map gap tracked in §5.6/LIMITATIONS.md |
| I10 | The tool never writes to the source tree | read-only source capability types plus preflight rejection of state/log/report paths inside either active tree; platform write constructors are destination/testkit primitives |
| I11 | Commit safety: plain-small New items use `CREATE_NEW`; transactional New items use a non-replacing rename; transactional replacements and all metadata repairs revalidate the target snapshot. Direct plain-small replacements validate the classification snapshot on the exact opened handle before truncation, with identity-checked read-only clearing when needed | §4.3/§4.8; adversarial tests E34/E35 |
| I12 | Single writer: a second run on the same exact destination root is refused machine-wide before any tree I/O (nested overlap is covered by the F16 assumption, not the lock) | run lock (§5.12, F26); test E33 |
| I13 | Resume never trusts a watermark: the temp prefix is always re-read and digest-verified before continuation; a checkpoint is a hint, and verification is the integrity guarantee | resume protocol (§5.12); fault-injection lost-write test E40 |

### 7.2 Crash matrix (FMEA)

Process killed at any point — destination state and next-run behavior:

| Kill point | Destination state | Next run |
|---|---|---|
| during enumeration | untouched (reads only) | clean restart, trivially |
| dir created, files pending | empty/partial dir, correct attrs | dirs are idempotent (`exists` → reuse); files classified New |
| plain small file mid-write (New or Replace) | final name exists and is shorter than the source; a replacement's old bytes are already gone | size differs → copied cleanly on rerun (I5/ADR 0030) |
| transactional stream/EA write, below checkpoint threshold | temp self-deleted (pending disposition); prior final object remains intact | file restarts on re-run — cheap by definition of the threshold (ADR 0034) |
| stream mid-write, checkpointed | partial temp persists (disposition cleared at first checkpoint); journal ≤ or *ahead of* durable data (tentative, I13) | resume: prefix digest re-verified against the journaled snapshot — continue on match; short temp / mismatch / source change → restart from zero, safely |
| after rename, before meta | correct content, wrong mtime | Different → recopied (wasteful, correct); window is microseconds |
| after meta, before journal/log line | fully correct file, no record | classified Same → skipped; counters correct for *this* run |
| mid-journal-append | torn last line | CRC check drops it; affected file falls back one watermark or restarts |
| during verify pass | copy already complete | re-run standalone `bigcp verify` re-verifies from scratch |

Power loss (vs. process kill) differs in two ways: completed files may still have data in OS/drive caches without `--flush` (§7.5), and pending-delete cleanup does not run, so an opaque temp can survive and is reported rather than auto-deleted. Checkpointed partials whose journal ran ahead of durable data are caught by mandatory prefix verification: short or mismatching temps restart from zero (I13). The default size+mtime skip remains a heuristic; users requiring authoritative post-failure content proof use `--verify` or standalone `bigcp verify`.

### 7.3 Accounting model and counter reconciliation

Object universes are **disjoint and counted separately**: *files* (plain data files), *dirs*, *links* (reparse entries). Terminal file outcomes are disjoint — `copied_new`, `copied_replaced`, `skipped_same`, `meta_fixed`, `failed`, `excluded`, `not_attempted` (`meta_fixed` is its own outcome, not a skip sub-class; `copied-with-warnings` per §4.4 is an *annotation* on `copied_*`, not an outcome). Reconciliation (I6), enforced exactly at run end:

```
files_discovered = copied_new + copied_replaced + skipped_same + skipped_diff + meta_fixed
                   + failed + excluded + not_attempted
dirs_discovered  = dir_done + dirs_meta_failed + dirs_failed + dirs_excluded
                   (dir_done ⊇ created and pre-existing dirs whose DirStamp succeeded — §5.10)
links_discovered = links_copied + links_failed + links_excluded + links_not_attempted
```

(`skipped_diff` = destination differs but `--replace=false` withheld replacement, F20 — logged with the full F13 difference detail.)

Byte counters each have exactly one meaning and are never mixed: `bytes_logical_discovered` (source logical sizes), `bytes_logical_copied` (logical size of `copied_*` files — reconciles like the file equation), `bytes_read_src` / `bytes_written_dst` (actual I/O — includes sparse savings, resume verify-reads, checkpoint replays; expected to *differ* from logical), `bytes_verified` (verify-pass reads).

The coordinator is the single owner of all counters; engines report only via `Outcome`. Any imbalance means a code bug (an item dropped without an outcome, or double-counted): the run is marked `integrity: FAILED` in report+log, exit code 6, and the summary tells the user to trust the log over the summary and file a bug. This turns "the tool silently missed files" — the worst possible failure — into a loud, detectable one. The chaos suite (§12.4) hammers this invariant specifically.

### 7.4 What bigcp will never do (design-level safety recap)

No delete/mirror mode exists; no source-write path exists (I1/I10); direct truncation is limited to a classified plain-small replacement; no "quiet skip" exists (every non-copied source file appears in exactly one accounted category); no success is emitted before completion (I4–I6); no blind creation over an unexamined New name exists (I11); no blindly trusted resume exists (I13). These are structural properties of the code, reinforced by the grep guards, type restrictions, lints, and invariant tests named in §7.1.

### 7.5 Durability guarantees — what "copied successfully" claims

Two explicit levels; the applied level is always stated in the summary and the report's `durability` field:

- **Logical completion (default):** all data and auxiliary writes were acknowledged, final metadata was set, and handles closed; the transactional path also completed its rename. Buffered data may still sit in the OS cache and any file's data may sit in the drive's volatile cache. `CloseHandle` is not a durability barrier, so this level makes no power-loss promise for recently completed files.
- **Durable completion (`--flush`), best-effort and honestly reported:** `FlushFileBuffers` is issued per copied file after its final metadata (and after rename on the transactional path). Per-file flushing only — no volume-level flush. The report records the requested mode and flush failures; some hardware does not fully honor cache-flush semantics (H5), so bigcp reports what it requested rather than promising hardware behavior.

Power loss or abrupt termination may leave a small final-named file incomplete and may strand a large opaque temp. Re-running repairs detectable incomplete small files; large resume never trusts a checkpoint without identity and prefix verification (I13). Because ordinary rerun equality is intentionally the zero-I/O size+mtime heuristic, authoritative content assurance after a suspected device/power fault requires `--verify` during the completing run or standalone `bigcp verify`. Audit records are durably synchronized before their claims become terminal (§5.15).

## 8. Performance engineering

### 8.1 Why two engines and where the 16 MiB threshold comes from

Per-file *fixed* cost (open+create+meta+close, AV filter scans on create/close) dominates below ~1 MiB on NTFS — parallelism across files is the only lever, and buffered I/O lets the cache manager coalesce flushes. Per-byte cost dominates for big files — there, big sequential requests that keep the device busy are the lever (the cache manager's read-ahead and write-behind supply the request overlap, §5.9). **The threshold default is 16 MiB, set by measurement, not citation** (2026-07-29 evidence, BENCHMARKS.md): the direct final-name path ran 8 MiB files ~1.85× faster than the temp path, so the boundary sits where whole-file worker buffering stays RAM-safe (64 workers × 16 MiB ≤ 1 GiB transient on VISION's ≥32 GB targets), not at the older 4 MiB industry figure. The threshold currently conflates two concerns — buffering strategy and destination strategy — and the registered follow-up separates them: chunked streaming *directly to the final name* for every non-checkpoint-eligible size, leaving temps only where resume needs them (≥ checkpoint threshold). Tunable via `--tune large-threshold=`.

NTFS VDL note: a write completing beyond the valid-data-length forces zero-fill of the gap. The streaming pipeline writes strictly sequentially (§5.9), so no gap ever exists and the zero-fill cost is exactly zero — which is why prohibiting `SetFileValidData` (F18, §5.9) costs nothing at all.

### 8.2 Static class-profile table (fixed at startup; overridable by flags — F29)

| Device class (per side) | Chunk | Small-file workers |
|---|---|---|
| NVMe (internal) | 8 MiB | `min(64, 4×cores)` |
| SATA SSD | 8 MiB | 32 |
| USB SSD | 4 MiB (≤ MTL) | 16 |
| HDD (any bus) | 16 MiB | 4 for a source HDD; 32 for a destination HDD (measured close-overlap win) |
| Unknown/low-confidence | 4 MiB | 4 |

There are no stream-count, queue-depth, or enumeration-thread columns because the implementation has no such schedulers (ADR 0033). **Composition is deterministic:** `chunk = min(src.chunk, dst.chunk, both nonzero MTLs, optional memory cap)`; worker count follows the destination row unless the source is HDD, in which case the source's seek-conservative row caps it. The memory override also limits threshold-sized small-file workers. Every selected value is logged/reported.

**Generality rule (F24/F29):** these tables encode *class* characteristics (HDD seek economics, UASP queueing, NVMe parallelism) that hold across drive generations and vendors — never measurements of any particular machine's drives, and they are applied **statically** at startup (§6.5); the flags are the only per-run adjustment mechanism. Nothing learned about one PC's drives is ever baked into defaults.

### 8.3 HDD-specific policies

- Sequential is everything: 16 MiB chunks and one synchronous large-file loop; small-file creates stay directory-affine.
- **Same-disk copies:** topology overlap is reported, but no alternating-burst engine or special enumeration policy ships. Such a scheduler is post-v1 and must earn its complexity with an isolated benchmark; until then the generic HDD profile is the honest behavior.

### 8.4 "No unnecessary I/O" checklist (each is a review item)

- No per-file destination stat (the join, §5.6). No re-open for metadata (handle-based, §4.3). No directory re-walk for timestamps (tracker, §5.10). No small-file hash unless requested (streamed files hash by design, §5.11). No write of identical attributes (§5.10). No journal "done"-records for small files when hashing is off (the skip heuristic subsumes them; journal then holds only watermarks + run header). No log flush storm (batched, 2 s cadence). No TUI-driven I/O (render from in-memory snapshots only).

### 8.5 No probing, no adaptive tuning

Per VISION: no device probing or benchmarking options exist — the "observed peak" reported (§5.14) comes solely from real copy traffic; and tuning is static per §6.5/§8.2. Destination ceilings are only ever learned from the writes the copy itself performs.

### 8.6 Write caching and removal safety

Windows (≥1809) defaults external drives to "Quick removal" (OS write caching off) — small-file copies feel that as per-file latency. bigcp does not attempt to read or infer the policy (that inference was specified and deleted with the profiler extras, §5.5); the README explains the trade-off, and bigcp never changes system settings. Durability layering (README states this plainly): buffered writes land in the *OS* cache first, and the *drive's* DRAM cache is only emptied by a real cache-flush command — `FlushFileBuffers` flushes both layers and is honored far more reliably than `FILE_FLAG_WRITE_THROUGH`/FUA (which some USB bridges ignore, §3.4), though not guaranteed by all hardware (H5; §7.5 reports what was *done*, not what hardware promises) — so `--flush` uses `FlushFileBuffers`, and without it the standard "Safely Remove" flow covers both caches. (NTFS and ReFS both journal their metadata — the exFAT no-journal fragility concern disappeared with exFAT support, F15.)

### 8.7 Performance objectives and gates (gates in §12.6)

**Two tiers, deliberately separated.** *Aspirational KPIs* (tracked, reported, never ship-blockers): ≥3× the best measured competitor configuration on the bounded small-file workload (W1s), ≥1.3× on the bounded large-file workload (W2s), and the topology-matched-ceiling fraction for the big-file case — a correct implementation must not become unshippable because one competitor or one controller excels somewhere. *Release gates* (hard, evidence-derived): **meet or exceed robocopy's best-known configuration on the bounded workloads using bigcp's default settings** (VISION's stated goal — the user must never need a `/MT`-style incantation); bounded-workload numbers recorded in BENCHMARKS.md with methodology and destination extent-count evidence; and — once baselines exist — **no regression beyond the noise band against the implementation's own rolling per-class baseline**. Per F24, every figure is expressed relative to measured ceilings/baselines of the drive *class*, never as absolute MB/s of one PC.

**Write-volume budget (F23 and the VISION prohibitions apply to benchmarks too):** every benchmark workload is *small and bounded* — e.g., W1s = 20 k×4 KiB (~80 MB), W2s = 2×20 GiB — with a hard per-suite write ceiling recorded in TESTING.md. Multi-TiB competitor sweeps and endurance measurements are **never performed** (drive-lifespan-reducing tests are prohibited outright, not merely rationed); competitor comparisons run the same bounded workloads, and each competitor's best-known configuration is cached so it is never re-swept. The aspirational KPIs are therefore *measured on the bounded workloads* — million-file and TB-class behavior is extrapolated from the bounded results plus design analysis, and the report/BENCHMARKS.md say so explicitly.

**Ceiling methodology:** `min(independent read ceiling, independent write ceiling)` is *invalid* whenever the two sides share anything (controller, USB hub, spindle, filesystem) — the honest ceiling is a **simultaneous** diskspd read-on-source + write-on-destination run with matched request sizes and placement (diskspd runs unbuffered so it measures the *device*, not the cache — that is the point of a ceiling). Source-cache control: benchmark datasets exceed installed RAM, or the protocol documents an explicit reset of **both** volumes (dismount/remount) — remounting only the destination leaves source caching uncontrolled and is not accepted.

**Measurement methodology (recorded in BENCHMARKS.md for every published number):** competitors run at their best-known configurations — robocopy swept across `/MT:{8,16,32,128}` × `/J` on/off — never one fixed invocation. Each result records OS build, Defender real-time state, cache protocol (above), dataset generator spec + seed, drive models/firmware/fill level, thermal rest intervals, repetitions (≥5), and median ± spread. Gates compare medians with a noise band; a single run is never a gate.

**Performance hypotheses register.** Numbers this plan quotes for mechanisms not yet measured on target-class hardware are hypotheses tracked in BENCHMARKS.md. **H2 — falsified and dispositioned (2026-07-29):** uniform temp+rename cost roughly two AV-filter evaluations per small file and prevented the default-throughput goal; ADR 0030 therefore moved plain small files to direct final-name writes while retaining atomic temps for ADS/EA, sparse, and large/checkpointed files (ADR 0034). **H3** always-on xxh3 <2 % of one core per GB/s (§5.11); **H4** post-read source revalidation <2 % on the small-file workload; **H5** `FlushFileBuffers` is honored across the tested drive matrix (§7.5 reports the request, not a universal hardware promise). H1, the deferred-close finalizer pool, was measured and retired.

## 9. Edge-case catalog

Each case: expected behavior + the test that pins it (§12 IDs). This table is the checklist for `testkit gen` scenarios.

| ID | Case | Behavior |
|---|---|---|
| E01 | 0-byte files | created, metadata copied, no data I/O |
| E02 | file size exactly at threshold / chunk / sector boundaries (±1) | correct via tail-trim protocol (§4.3); property-tested sizes |
| E03 | source or destination volume is exFAT/FAT32/other non-NTFS/ReFS | rejected pre-flight with a clear "NTFS and ReFS only" error + reformat/robocopy hint (§4.4, F15) |
| E04 | path > 260 chars; component = 255; total near 32 760 | works via `\\?\`; over-limit → pre-flight `path_too_long` |
| E05 | trailing dot/space in names; reserved names (`CON`, `NUL.txt`) | copied verbatim via `\\?\` |
| E06 | Unicode: emoji, surrogates (incl. unpaired), RTL, combining, NFC/NFD differences | byte-preserved (no normalization ever); NFC≠NFD are distinct files |
| E07 | hidden/system/readonly files and dirs | attrs copied; readonly *destination* replaced via `IGNORE_READONLY_ATTRIBUTE` rename (§4.3) |
| E08 | ADS: multiple streams, large streams, stream-only size | copied; warned+counted if the destination volume lacks named-stream capability (§4.4) |
| E09 | sparse 1 TB file, 1 % allocated | allocated-ranges copy; sparse preserved; non-sparse dst → space pre-check |
| E10 | NTFS-compressed / EFS-encrypted source | content copied; attr best-effort re-applied (§4.2) |
| E11 | symlink file/dir: relative, absolute, dangling | reparse copied verbatim; no recursion; privilege hint if needed |
| E12 | junction, volume mount point | copied as junction; never recursed |
| E13 | hard-linked pairs | copied as independent files; not detected or counted (VISION) — correctness pinned by oracle byte-compare |
| E14 | source file vanishes / changes mid-run | §4.8: `source_changed` at open or post-read revalidation; transactional publication is withheld, while a direct plain-small write already started may remain for rerun repair |
| E15 | destination full mid-run | breaker → `not_attempted`, estimated shortfall *range* in report (§5.5) |
| E16 | device disappears mid-run (surprise removal) | breaker after 5 consecutive `device_gone`/space failures without a success; resumable exit 4; resume verifies checkpointed partials (§5.12). Deterministic fault-injection proof remains release work (§12.3); forced disconnects are prohibited |
| E17 | `TerminateProcess` at arbitrary ms (chaos) | crash matrix §7.2 holds; oracle-clean after convergent re-runs |
| E18 | dst tree deleted between runs (stale journal) | journal partials fail validation → clean restart; no bad skips (I8) |
| E19 | same-run src == dst, dst inside src, src inside dst | fatal pre-flight error before any I/O |
| E20 | locked destination file (open exe) | replace fails `locked` with the close-and-rerun hint (no lock-owner lookup, §5.13) |
| E21 | AV/indexer interference (slow closes) | correctness unaffected; detector may hint |
| E22 | OneDrive placeholders | §4.6: hydrate+count by default; `--skip-cloud` |
| E23 | re-run steady state | a completed run re-classifies 100 % Same on re-run — exact 100 ns comparison, zero churn (§4.1) |
| E24 | *withdrawn* (FAT-only concern: DST shifts on local-time filesystems) | NTFS/ReFS store UTC; no DST handling exists (F15) |
| E25 | deep tree and million-entry synthetic directory | iterative walk is routine-tested at bounded depth; bounded-memory million-entry simulation and fallback remain release work (§5.6, F33). Real trees at that scale are prohibited (§12.0) |
| E26 | names differing only by case (WSL case-sensitive dir) | duplicate join key → per-file error, not silent overwrite (§4.5) |
| E27 | dest has a *directory* where source has a *file* (and inverse) | error `type_conflict` (no recursive delete exists to "fix" it); hint |
| E28 | 4Kn native / 512e mixed sector sizes | alignment from per-volume profiler values; VHDX matrix test |
| E29 | run from non-console (redirected stdout) | auto `--plain`; no ANSI garbage |
| E30 | set timestamps don't round-trip on the destination | should be impossible on NTFS/ReFS (100 ns fidelity); post-set read-back *in debug builds only* asserts exact round-trip — a failure indicates a filter driver interfering |
| E31 | reparse tag unknown (HSM, custom filters) | not recursed; fails `unsupported_reparse` by default with the tag logged; verbatim copy only with explicit `--raw-reparse` (§4.6) |
| E32 | source root is a drive root with system artifacts | §4.7 default exclusions, visible count |
| E33 | second bigcp run on the same exact destination root | refused machine-wide before any tree I/O by the run lock (I12, F26); nested-root overlap is an F16 assumption, documented |
| E34 | destination object appears or changes after classification | plain-small New `CREATE_NEW` / transactional New non-replacing rename rejects a new collision; transactional replacement and metadata repair revalidate. Direct replacement validates the classification snapshot on its opened handle before truncation, within the exclusive destination contract (§4.8) |
| E35 | destination file is a hard link (including a link to a source file) | rename-replace severs the *name* only; the other link — and the source — remain untouched; pinned by test |
| E36 | destination subdirectory is a junction/symlink | guaranteed for reparses **present when the directory is examined** (dir open with `OPEN_REPARSE_POINT` → subtree `type_conflict`); mid-run swaps fall under the F16 exclusivity assumption — best-effort detection (§4.5) |
| E37 | log/journal/report device full or disconnected mid-run | audit-failure policy governs (§5.15): lossless fallbacks continue with `audit:degraded`; if **both** log paths fail, dispatch stops, in-flight drains, exit 6 — copying is *not* "unaffected"; journal-only failure disables checkpointing (§5.12) |
| E38 | 255-character filename | plain direct path adds no suffix; the transactional path uses an opaque short sibling independent of final-name length (§4.3) |
| E39 | *withdrawn* (the non-POSIX rename fallback no longer exists) | single rename path with `IGNORE_READONLY_ATTRIBUTE` — no attribute pre-clearing to undo (§4.3, F15) |
| E40 | lost unflushed writes around a kill (simulated power loss) | the journal may legitimately run ahead (tentative checkpoints); mandatory prefix verification detects short/mismatching temps → safe restart, never a trusted-but-wrong resume (I13, §5.12, §12.3) |
| E45 | zero-byte (or tiny) file carrying a huge ADS | stream discovery promotes it to the streaming engine; buffers never exceed the small-engine model; totals/digests per stream (§5.4) |
| E46 | `--state-dir`/`--log`/`--report` path inside the source or destination tree | rejected pre-flight — audit artifacts may share a volume but never a tree (§5.12) |
| E41 | `--replace=false` with differing destination files | files left untouched, outcome `skipped_diff` with full F13 difference detail (F20) |
| E42 | destination root does not exist at startup | nearest-existing-ancestor pinned + identity-checked, components created reparse-safely, new root pinned and revalidated (§4.5) |
| E43 | ADS/EA divergence on an otherwise-Same file | not detected at copy time (documented §4.1 scope note); caught by standalone `bigcp verify`'s stream-set + EA comparison (§5.17) |
| E44 | UNC / network path as SRC or DST (incl. mapped drives to shares) | rejected pre-flight with a clear "local volumes only" error (F27, §4.5) |

## 10. CLI, log format, report format

### 10.1 CLI grammar

```
bigcp <SRC> <DST> [flags]      # copy (the default subcommand)
bigcp verify <SRC> <DST>     # always full, both trees (§5.17 — no sub-modes)
bigcp report <REPORT.json> [--plain]   # --plain prints instead of opening the TUI browser
bigcp report <REPORT.json>     # open report browser TUI

Flags (copy):
  --dry-run                enumerate+classify only; full report, zero destination-tree writes (log/report still written)
  --verify                 post-copy verification of this run's copies (§5.17)
  --include-system         include root OS artifacts (§4.7)
  --skip-cloud             skip OneDrive/cloud placeholders (§4.6)
  --replace[=true|false]   replace differing destination files (default true, F20; false → outcome skipped_diff, fully logged)
  --flush                  FlushFileBuffers per file after data+metadata and publication where applicable (§7.5)
  --analyze                bounded live-run insight: size-class timings, top-20 slowest copies,
                           5 s stat cadence — one log event + one report section (F35, §5.14)
  --no-sparse | --raw-reparse                         (troubleshooting escape hatches)
  --profile <auto|nvme|sata-ssd|usb-ssd|hdd|unknown>[,<dst-class>]   default auto = detected class per side
                           (§8.2; `unknown` = the conservative fallback profile);
                           one value forces both sides, two values force src,dst
  --tune <k=v,...>         the single advanced override hatch (replaces per-knob flags): chunk,
                           threads, mem, large-threshold, checkpoint-threshold —
                           keys documented in MAINTENANCE.md; every applied value is logged
                           (no stream-count/queue-depth keys: the sequential pipeline has none to tune, §5.9)
  --fresh                  ignore journal/partials
  --state-dir <DIR> --log <FILE> --report <FILE>
  --plain                  line output instead of TUI (auto when not a TTY)
  --no-color --quiet
Exit codes: 0 ok · 2 completed-with-failures · 3 user-canceled (resumable)
            (code 6 covers both internal invariant breaches and unrecoverable audit failure, §5.15;
             CLI usage/parse errors exit 5 — never 2, which is reserved for real completed-with-failures)
            4 aborted by breaker (resumable) · 5 fatal startup · 6 internal invariant breach
```

**No interactive prompts, ever (F19), with one owner-mandated pre-copy exception (ADR 0032):** when the destination device reports write caching disabled (Quick-removal policy, measured ~3.4× cost on small-file workloads), the CLI shows a notice and — in an interactive terminal only, never under `--plain`/`--quiet`/`--dry-run`/redirection — one Continue? confirmation *before any copying begins*. Mid-run prompts remain forbidden, and every behavior choice is an argument. Replacement authorization is `--replace` (default true per VISION F20); swapped-SRC/DST mistakes are caught by the identity pre-flight (§4.5) and made visible by the run banner's replacement forecast and the F13 statistics — never by a mid-run question. (The device-gone recovery pause, §5.13, auto-aborts resumably rather than waiting on input.)

### 10.2 Log (JSONL, schema v1 — `docs/schemas/log-v1.schema.json`)

**The shipped schema files are the normative field-name authority**; the examples below are illustrative, and where an as-built name differs (e.g., `directory`/`warning`/`reason`/`replacement`, `duration_seconds`, `bytes_read_source`, `dir_done`), the schema wins. There is no `governor` event (the governor was deleted, §6.5) and no `locker` field (Restart Manager lookup was deleted, §5.13); checkpoint `watermark` data lives in the journal, not the log. One JSON object per line; every line has RFC3339 `ts` and tagged `ev`. Events:

```jsonc
{"ev":"run_start","v":1,"run_id":"…","argv":[…],"src":"…","dst":"…","options":{…},
 "devices":[{"role":"src","model":"…","bus":"usb","kind":"ssd","fs":"NTFS","sector":4096,
             "mtl":1048576,"free":…,"confidence":"high","profile":"usb-ssd"}…]}
{"ev":"dir","action":"created|exists","rel":"…"}
{"ev":"file","action":"copied","rel":"a/b.bin","size":123,"ms":4,"hash":"xxh3:9f…","streams":2}
{"ev":"file","action":"copied","rel":"c/d.docx","size":…,"ms":…,"hash":"…",          // F13: every overwrite
 "replaced":{"old_size":…,"old_mtime":…,"old_attrs":…,"dest_newer":true,"why":["size","mtime"]}}  // decision fully logged
{"ev":"file","action":"skipped","why":"same","rel":"…"}
{"ev":"file","action":"skipped_diff","rel":"…",                                      // --replace=false (F20):
 "diff":{"old_size":…,"old_mtime":…,"dest_newer":false,"why":["size"]}}              //   not replaced, fully logged
{"ev":"file","action":"meta_fixed","rel":"…","fixed":["attrs","ctime"]}              // repair without data I/O (§4.1)
{"ev":"file","action":"failed","rel":"…","op":"open_src","code":5,"category":"permissions",
 "msg":"Access is denied","hint":"…"}
{"ev":"file","action":"excluded|not_attempted","why":"…","rel":"…"}
{"ev":"warning","kind":"streams_dropped|efs_downgrade|ea_dropped|cloud_hydrated|…","rel":"…"}
{"ev":"extra","rel":"…"}                                          // dest-only entry (never touched)
{"ev":"watermark","rel":"…","off":268435456}
{"ev":"stat","counters":{…},"read_mbps":…,"write_mbps":…}         // every 30 s
{"ev":"profile","source_class":"UsbSsd","destination_class":"Hdd","chunk_bytes":4194304,
 "workers":32,"same_physical_disk":false}                         // static, once at start (§6.5)
{"ev":"run_end","counters":{"files_discovered":…,"copied_new":…,"copied_replaced":…,"skipped_same":…,
 "skipped_diff":…,"meta_fixed":…,"failed":…,"excluded":…,"not_attempted":…,"extra":…,
 "dirs_discovered":…,"dirs_created":…,"links_copied":…,
 "bytes_logical_discovered":…,"bytes_logical_copied":…,"bytes_read_src":…,"bytes_written_dst":…},
 "durability":"logical","audit":"ok","integrity":"ok","exit":0}                      // §7.3, §7.5, §5.15
```

Paths are UTF-8 lossy + `path_raw` (hex UTF-16) added when lossy (§4.5). Every event carries an RFC3339 UTC timestamp. The log is append-oriented with torn-line rollback, flushed every 2 s and durably synchronized at run end.

### 10.3 Report (JSON, schema v1)

Aggregated, self-contained (embeds config + device profiles so it's meaningful years later):

```jsonc
{"v":1,"run":{"id":…,"started":…,"ended":…,"duration_s":…,"exit":…,"resumed_from":…,
              "durability":"logical|durable","audit":"ok|degraded"},
 "config":{…},"devices":[…],
 "counters":{… as run_end …},
 "replacements":{"count":…,"bytes":…,"dest_newer":…,"by_folder":{…},              // F13 summary statistics
                 "samples":[{"rel":…,"old_size":…,"old_mtime":…,"why":["mtime"]}]},
 "folders":[{"rel":"photos","copied":…,"failed":…,"bytes":…,"mbps":…}],   // per top-level dir
 "errors":[{"category":"locked","code":32,"count":17,"hint":"…",
            "by_folder":{"docs":12,…},"samples":[{"rel":…,"msg":…,"locker":…}]}], // ≤100 samples/cat; log has all
 "warnings":{"streams_dropped":3,"cloud_hydrated":120,"compressed_sources":42,…},
 "extras":{"count":42,"samples":[…]},
 "timeline":[{"t":0,"read_mbps":…,"write_mbps":…,"files_s":…,"verdict":"dest-bound"}…],
 "phases":{"fastest":{"span":[…],"mbps":…,"folder":"…"},"slowest":{…}},
 "bottleneck":{"hypothesis":"dest-bound","confidence":"high","evidence":"dst busy 97%, src busy 41%",
               "observed_peak_mbps":940,"avg_mbps":610,"efficiency_vs_observed_peak":0.65,
               "provenance":"best sustained 5s window on dst — observed peak, a proxy for attainable maximum"},
 "hints":[{"id":"slc_cliff","text":"…","confidence":"medium"}],
 "verify":{"mode":"copied","passed":…,"failed":…,"mismatches":[…]},
 "integrity":"ok"}
```

## 11. Terminal UI design

Stack: `ratatui` + `crossterm`. Truecolor with graceful 256/16-color fallback (terminal capability detect); honors `NO_COLOR`; full Unicode with width-aware truncation of long paths (middle-ellipsis, keeping filename visible). The TUI renders immutable state snapshots published by the coordinator (watch channel, ≤30 fps) — **the UI can never touch run data structures or block I/O threads**.

The surface is deliberately **compact and truthful**: every widget renders data the engine actually produces. (An earlier revision specified sparklines, an active-transfers table with per-file progress, live governor readouts, latency percentiles, and a color-coded verdict strip; those were deleted — decoration ahead of real telemetry would be fiction, and the compact surface answers the questions users actually have: how far along, how fast, what failed, what to do.)

Tabs (keys `1–6`, `Tab`/`Shift-Tab`):

1. **Dashboard** — progress gauge over discovered files; state, copied/skipped/failed counts, bytes read/written; **ETA for the work discovered so far** (§6.4, VISION `/ETA`); latest status message.
2. **Errors** — live counts by category with hints; bounded samples (the log always has everything).
3. **Devices** — application-side read/write rates; static class/profile facts land in the final report.
4. **Performance** — rates, logical bytes, discovery counts.
5. **Hints** — actionable list from the §5.14 detectors and static advice.
6. **Log** — recent events.

Global keys: `q`/`Esc` request a graceful cancel (honored between chunks even inside a huge file, §5.13). There are no recovery-interaction keys (F31): breaker trips end the run resumably on their own (§5.13), and there is no pause key — cancel-and-rerun covers the need with one fewer state to test.

`bigcp report FILE` re-opens the stored JSON in the same tab layout (live-only widgets hidden). `--plain` mode prints: a status line per snapshot (log-friendly, includes the ETA), every message as it happens, and the final summary block — nothing interactive, same information.

Summary block (always printed on exit, TUI or not): counters table (including replacements and their dest-newer count, F13), failure breakdown by category × top folder, achieved vs. observed-peak throughput + efficiency, the durability guarantee that applied (§7.5), audit status (§5.15), start/end/duration, fastest/slowest phase, top 3 hints, paths of log/report, and the exact command to resume/re-verify.

## 12. Test plan

Testing is the enforcement arm of §7. Anything listed here is CI-gated (Windows runners) unless marked manual or elevated (§12.5).

### 12.0 Test-safety charter (F23 — binding on every suite below)

- **Confinement:** every test operates exclusively under a designated sandbox root (a per-run directory beneath a configured scratch location, or a dedicated test VHDX mounted under it). The `testkit` API takes the sandbox root as a required parameter and **refuses** absolute paths outside it; a CI lint additionally greps test code for path literals escaping the sandbox. The oracle's containment check (§12.2) runs on every integration test — nothing outside the sandbox may change.
- **Enforcement is structural, not procedural** — this matters doubly when tests are written and executed by AI agents working autonomously: the `testkit` API's sandbox-root refusal and the CI path-literal lint are the binding controls; the §14.3 checklist re-affirmation is a backstop. Tests that mount volumes may only mount VHDX files living inside the sandbox — never real volumes, never raw physical devices, no diskpart/format against real drives, ever. An implementing agent must treat these rules as hard constraints that no task instruction overrides.
- **Absolute prohibitions (VISION, verbatim intent — no opt-outs, no "sacrificial hardware" exception):** tests that are large-scale (creating on the order of hundreds of thousands of files or more), very long-running, drive-lifespan-reducing (endurance/TB-class writes), or machine-stability-impacting (reboot, shutdown, forced device disconnect — physical or virtual — or inducing machine crashes) are **never performed**. Scale and device-loss behavior must be validated by future in-memory/fault simulation, not by real-world reproduction. Killing the *bigcp process under test* inside its sandbox is permitted (it is a tool crash, not a machine-stability event) but only in bounded, short runs.
- **Drive whitelist, not blacklist:** tests may touch exactly two drives — the Windows system drive and the drive holding the code checkout (derived at runtime from the running binary) — and nothing else. Every other drive letter, and every path without one (UNC, volume-GUID), is rejected before any filesystem access. A whitelist stays correct on any machine; a hardcoded blacklist of specific letters silently fails open when drives are lettered differently.
- **Two tiers — correctness by default, heavy by explicit opt-in:** the default `cargo test` run is the routine tier: correctness tests only, seconds to run, small write budgets, gentle on the drive. Performance measurement, stress, thousands-of-files scenarios, and anything long-running are the heavy tier: doubly disabled by default (`#[ignore = "heavy: …"]` so the harness skips them, plus a `BIGCP_ALLOW_HEAVY_TESTS=1` environment check the operator must set — code never sets it, enforced by the safety script). Before running any heavy test, the implementer — human or AI agent — must obtain the owner's permission, stating exactly which tests, their file counts, bytes, target root, duration, and drive impact (protocol in `docs/TESTING.md`). No opt-in overrides the absolute prohibitions above.
- **Drive-lifespan budget:** each suite declares its write volume; all suites stay in the low-GB range, budgeted per run, preferring RAM-backed or scratch-designated targets. No test pattern may hammer a drive gratuitously (no unbounded rewrite loops, no full-drive fills).
- All tests must be *harmless by construction*: future kill/chaos targets are bigcp processes inside the sandbox; fault injection will be in-process simulation (§12.3), never real device manipulation.

### 12.1 Unit tests (per crate, `cargo test`)

- `win`: every wrapper against real temp files (create/rename/meta/reparse/sparse FSCTL round-trips); error mapping. (No alignment helpers exist to test — buffered I/O needs none, §5.9.)
- `core::path`: the normalization table — long paths, UNC, `\\?\` passthrough, trailing dot/space, reserved names, surrogates, case-fold join keys; property: normalization is idempotent; display+`path_raw` round-trips.
- `core::classify`: the full §4.1 decision table; exhaustive small-case enumeration (size ±, mtime ± 1 FILETIME tick — exact equality, no tolerance rows exist — attr diffs, type conflicts).
- Coordinator behavior: breaker trip/stop/accounting, cancellation (including mid-file), bounded worker queues, dir-before-file ordering — driven through the public `run_copy` path on small sandbox trees rather than a dedicated scheduler harness (the per-device scheduler this bullet once targeted was deleted, §5.8).
- `core::journal`: round-trip; torn-tail (truncate at every byte offset of a valid file — property test) → loader never panics, never resumes unsafely (I8).
- Streaming path: the §6.3 machine is a sequential loop by construction, which collapses the old adversarial-completion-order test burden (there are no completion orders to adversarially permute, and no lock-free structures for `loom` to model-check — a deliberate consequence of the §5.9 simplifications). What remains testable and tested: short reads and mid-stream errors at every chunk boundary, exact digest equality against a single-threaded reference, and every checkpoint's `prefix_digest` equal to the reference digest of exactly `[0, W)` (boundary snapshots, §5.9); the deterministic kill-point simulation (§12.4) kills the loop at every state transition.

### 12.2 The oracle (`testkit check`) and generator (`testkit gen`)

- `gen` builds trees from YAML specs: counts, size distributions (incl. lognormal small-file clouds and boundary sizes from E02), depth profiles, name alphabets (Unicode zoo), ADS, sparse maps, links, attrs, timestamps (incl. 100 ns boundary values), read-only/hidden/system mixes. Specs live in `testkit/scenarios/*.yaml`. Coverage rule, complexity-controlled: **every §9 E-case is pinned by a test** — a scenario file *or* a directed unit/e2e test, whichever expresses it more naturally (many E-cases are single-object edge conditions that a five-line test pins better than a YAML tree).
- `check src dst` is the **independent oracle**: deliberately naive synchronous code (no shared logic with `core` — enforced by crate dependency rules §5.2) that walks both trees and byte-compares data + compares copied metadata per the §4 contract — explicitly including full stream sets (names, sizes, contents) and EA blobs — emitting a machine-readable diff. The oracle is the arbiter of every integration test: *bigcp is correct iff the oracle finds no diff*. The oracle also asserts **containment**: a sentinel tree placed beside the destination (and the source tree itself) is snapshotted before and after every run — nothing outside the intended destination root may change.

### 12.3 Fault injection

**Release work, not present in the current tree:** introduce narrow test ports around completion-critical Win32 calls and a deterministic in-memory fault driver. It must inject representative errors by stable site name, model lost unflushed writes, and assert exact failure accounting, no panic, no invariant breach, and rerun convergence. This is the required proof for error paths a healthy filesystem cannot exercise routinely; no hidden product CLI flag is required.

### 12.4 Chaos (crash/kill) harness — the flagship reliability test

`testkit chaos`: loop { spawn bigcp on a *small bounded* scenario → `TerminateProcess` at uniform-random ms (also: suspend/resume storms, breaker-inducing device-error injections via the fault port) → re-run to completion → oracle } until N clean cycles. Asserts after every cycle: crash matrix §7.2 outcomes only; I5 as rescoped by ADR 0030 (the re-run converges to an oracle-verified correct tree from any kill point — small-file partials at final names are legal intermediate states that the rerun must replace; no *large* final name is ever incomplete); temps only ever self-deleted, ownership-provable, or reported; counters reconcile on the completing run. **Duration is bounded per the VISION prohibitions:** exhaustiveness comes from the *deterministic in-process kill-point simulation* (the sans-I/O state machine killed at every step — thousands of kill points in seconds, no real time or writes), while the real-process chaos loop is a short, bounded confirmation pass (minutes, small scenarios) — never an hours-long soak. A **mutator mode** additionally races the run with destination changes (create/modify/delete/junction-swap at random moments, sandbox-confined) to exercise the `destination_changed` and `type_conflict` paths under fire (E34/E36). Historical note for maintainers: this harness is the reason the completion protocol (§4.3) can be trusted; never ship a change to `engine_*`/`journal` without a bounded chaos pass.

### 12.5 Filesystem & hardware matrix

- **VHDX matrix (elevated — post-v1 by owner decision, ADR 0029):** creating, mounting, and formatting VHDXs requires elevation, and ReFS availability depends on Windows edition/configuration; the implementation environment has neither, so **v1 ships with ReFS as best-effort (code-reviewed, not matrix-certified)** and this matrix certifies it post-v1 (VHDX files confined to the test sandbox, §12.0). The fixture creates+formats+mounts: NTFS (4 KiB and 64 KiB clusters, compressed dirs, 512e and 4Kn `-PhysicalSectorSizeBytes`), ReFS (incl. the same-volume-ReFS *hint* path, F28; skipped-with-notice on editions without ReFS), plus **one exFAT cell that exists solely to test the pre-flight rejection** (E03 — a clean "NTFS and ReFS only" error, no tree I/O); full scenario suite × supported cells; asserts include the capability rules (§4.4) and E23 no-churn. Pure state-machine, fault-injection, property, and unit suites stay in ordinary unelevated CI.
- **Device-loss testing is simulation-only (VISION prohibitions):** the future E16 fault port must return `device_gone`-class errors at arbitrary points to drive the breaker, resumable abort, and verified-resume paths deterministically. Forced disconnects of any kind — physical cable pulls and virtual surprise-detach — are prohibited. VHDX matrix operations are always graceful.
- **Real-hardware checklist (`[HW]` — the one suite requiring a human operator with physical drives; release-gated):** USB-C NVMe enclosure (UASP), portable SSD (T7-class), USB HDD (SMR if available), internal NVMe⇄USB, same-spindle HDD copy, Quick-removal vs Better-performance policies — all with *bounded* per-session write volumes per §12.0/§8.7 (low-GB, recorded). Scripted via `testkit` so the operator only plugs and confirms; results archived in BENCHMARKS.md. Everything else in §12 is executable end-to-end without a human (§13.1).

### 12.6 Performance regression + differential

- Perf CI on a designated runner is **heavy-tier** (§12.0), never part of the default suite. Bounded W1s/W2s/W3s/W4s workloads record MB/s, files/s, and destination extent counts via the read-only `testkit extents` command; the regression gate is −10 %. Million-file/million-directory behavior requires the not-yet-built synthetic enumeration harness; real-file scale tests at that magnitude are prohibited.
- **Differential testing is post-v1 backlog, not a release gate.** The `testkit oscopy` CopyFile2 reference copier and three-way robocopy comparisons were release-gating in an earlier revision; deleted from the gate set because the independent oracle (§12.2) already provides the authoritative correctness verdict, and "the OS agrees" is a second opinion worth having but not worth blocking on. Appendix A remains the documented semantic-delta reference.
- Perf gates are expressed relative to measured class ceilings (F24, §8.7) so the same gate definitions hold on any runner's hardware generation.

### 12.7 Miscellaneous suites

- TUI: `ratatui` TestBackend snapshot tests for every tab in small/large terminals + non-TTY fallback (E29).
- Schema: routine tests currently parse both schema documents and pin their v1 identity. Emitted-instance validation plus archived-sample compatibility is explicit release work; do not claim it before that validator lands.
- Leak watch: soak runs assert flat handle count and bounded working set.
- Long-run: there is no long-run suite. Very long tests and endurance/TB-class writes are prohibited outright (VISION, §12.0); repetition coverage comes from bounded re-run/re-verify cycles inside normal suite budgets, and durability-over-volume claims are simply not made (BENCHMARKS.md states this).

### 12.8 Adversarial suite

Directed tests for the abuse-shaped cases (all IDs from §9): concurrent invocations racing for one destination (E33); destination created/modified between classification and commit (E34); src/dst aliased through junctions, `subst`, and mount points (E19); destination subtree junction swaps before and during the run (E36); destination hard-linked to a source file (E35); maximum-length filenames receiving temps (E38); lost-write/journal-ahead orderings (E40); log/journal/report device full and disconnected (E37); huge-ADS promotion (E45); audit paths inside a tree (E46); and content changed with size+mtime preserved — asserting both that the copy *skips* it (documented heuristic behavior) and that standalone `bigcp verify` *catches* it.

### 12.9 Release criteria (v1.0, technical)

All suites green: unit + property · fault-injection matrix over the wrapper-boundary sites (§12.3) · deterministic kill-point simulation exhaustive + bounded real-process chaos clean (incl. mutator mode; §12.4 — no hours-long soaks exist) · adversarial suite (§12.8) · schema validation · real-hardware checklist executed within its bounded write budget and archived in BENCHMARKS.md · bounded-workload performance evidence recorded per §8.7 (measured numbers + no-regression once baselines exist; the multiplier KPIs stay aspirational) · docs self-sufficiency criteria satisfied (§14.6). Post-v1 by owner decision: the elevated VHDX/ReFS matrix (ADR 0029 — ReFS is best-effort at v1) and differential copier runs (§12.6).

### 12.10 Final production validations — executed only on explicit owner request

The following validation work is **out of scope for ordinary development**
and runs only when the owner explicitly asks for the production-validation
pass (owner decision, 2026-07-29). Until then, the 1.0 claim is simply not
made; nothing here blocks feature or performance work:

- **Chaos/kill-convergence harness** (§12.4): bounded kill-anywhere →
  rerun-converges cycles, oracle-verified — the evidence behind the
  rerun-repair contract.
- **Adversarial edge-case set** (§12.8) as directed e2e tests: aliased
  roots, run-lock races, mid-run destination mutation, lost-write orderings.
- **Sentinel + schema honesty checks**: a canary tree beside the destination
  asserted untouched; emitted log/report instances validated against the
  shipped schemas.
- **Certified performance protocol**: median of ≥5 quiet-machine repetitions
  for every BENCHMARKS.md scoreboard cell (single-session numbers stay
  labeled indicative until this runs). Two requirements added by the
  2026-07-29 measurements: every repetition is preceded by a **quiesce
  step** (flush wait or settle interval — a write-cached destination drains
  each run's lazy flush backlog into the following run, swinging results up
  to ~3×), and configuration **orderings are rotated** so no variant
  systematically inherits another's backlog.

An initial code review of the paths these validations cover was performed
2026-07-29 (per-directory outstanding-counter symmetry including promoted
hand-backs and worker-panic completions; exit-rotation progress guarantees;
breaker/cancel walks still exiting partially-dispatched directories;
stamp-at-create versus verification expectations) and found no critical
defects — the validations remain the mechanical proof of that review.

## 13. Implementation order and technical gates

Per VISION, this plan carries no development phases, timelines, or team-process material. What belongs in an engineering plan — and is recorded here — is the *technical dependency order* (what cannot be built before what) and the *quality gates* that protect reliability as the system grows.

**Dependency order** (each layer builds only on tested layers beneath it; correctness layers deliberately precede performance layers, because a fast wrong copier is worthless):

1. `win` wrappers + path/identity layer (§4.5) + `testkit gen/check` — property/unit tests first; nothing above compiles against untested wrappers.
2. **Single-threaded semantic baseline** implementing the entire §4 contract (metadata, ADS, EAs, links, directory post-order, direct-plain-small/transactional completion, exclusions), reachable as `--tune threads=1` and validated by the independent oracle. Sparse-layout preservation is an optimization; a dense logical copy remains correct (§4.1).
3. Journal + checkpoints + resume (§5.12) with the chaos harness — before any parallelism, because crash-correctness bugs are easiest to isolate in a deterministic single-threaded world.
4. Join + small-file workers + accounting/breakers (needs 2+3: outcomes and crash protocol already trustworthy).
5. Large-file streaming path (§5.9) + device profiler with static class profiles (needs 3: checkpoints are its resume substrate). Same-spindle burst mode is post-v1 and benchmark-gated.
6. TUI + report + hints (needs stable counters/stats); the two verification forms (reuse the engines); hardening completeness (fault-injection matrix, VHDX matrix, adversarial suite, bounded chaos).

**Standing gates (technical, not calendar):**

- The oracle suite must pass before and after every layer lands.
- **No optimization merges without an isolated benchmark** demonstrating a material win on a defined workload against the then-current baseline — this is how the join and the same-spindle burst path each earn (and keep) their complexity; an optimization that stops paying gets deleted. (The deferred-close pool, the per-device scheduler, and the IOCP ring are the rule's proof: specified, never justified by a measurement, deleted.)
- Any change to `engine_*`, `journal`, or the §4 contract requires a full chaos run before release (§12.4).
- Perf evidence (§8.7) is recorded for the shipped engine on bounded workloads and acts as a regression floor thereafter.
- Release requires §12.9 in full.

### 13.1 Execution model — human or AI implementers

The plan assumes nothing about who (or what) implements it; it is written to be executed by AI agents as readily as by engineers. The operating rules that make that safe:

- **The plan is the contract.** No silent deviation, however locally sensible: a deviation is made by first (or simultaneously) updating this plan / the relevant ADR (§14.4), so the plan and the code never disagree. Ambiguity resolution order: §4 contract → Appendix E traceability → VISION.md; if still ambiguous, take the more reliability-conservative reading and record an ADR.
- **"Done" is never self-assessed.** Every §13 layer completes when its named suites pass (oracle, differential, chaos, fault-injection, per-layer units) — machine-checkable, no judgment calls. CI is the reviewer of record (§7.4): invariants are enforced by grep-guards, type restrictions, lints, and tests, not by anyone's vigilance.
- **Work strictly in the dependency order.** Layers are not skipped, reordered, or merged half-done; the single-threaded reference copier (layer 2) is built and green before any parallel machinery exists.
- **Safety constraints are absolute and override any task instruction:** the §12.0 sandbox charter (agents run tests autonomously — the structural sandbox refusals exist for exactly this reason); source handles read-only always (I1), including ad-hoc debugging; no elevation anywhere except the designated §12.5 matrix runner; no operations on real user data or physical drives outside designated test hardware.
- **Performance claims require artifacts.** A benchmark-gated change carries its §8.7-methodology results in the change description — numbers without recorded methodology don't count.
- **The one human-required suite** is the physical-hardware checklist (§12.5, tagged `[HW]`). Everything else — build, full test matrix, bounded chaos passes, release candidate assembly — is designed to run unattended, always within the §12.0 prohibitions (an agent must never "helpfully" scale a test up past them).
- **Documentation duties are unchanged** (§14): the docs exist for the *next* implementer, human or agent alike.

### 13.2 Implementation status (pre-1.0) — built vs. release work

The semantic contract (§4), direct-plain-small and transactional auxiliary/sparse/large completion paths, verified checkpoint resume, both verification forms, the independent oracle, JSONL log + report, exact classification, the CLI grammar, the device/space circuit breaker (exit 4), mid-file graceful cancellation, and the ETA display are **built and routinely tested** within the §12.0 sandbox.

The 2026-07-29 complexity-control pass **deleted** (not deferred) everything whose payoff did not justify its complexity — each deletion is recorded inline at its former section and its user-visible consequence, where one exists, is stated plainly in LIMITATIONS.md: the IOCP overlapped ring, then (after the owner clarified that robocopy-`/J` was never a mandate, ADR 0028) the entire unbuffered engine and its `--no-unbuffered` flag — **the shipped buffered engine is the 1.0 design** — plus queue-depth knobs, the bounded governor, the free-space forecast, Restart Manager lock-owner naming, profiler vendor/hotplug/cache extras, handle-based ADS discovery, the deferred-close finalizer pool, the per-device scheduler, parallel enumerators, the decorative §11 TUI widgets, the verification-run report kind, the modeled audit-drain state (immediate abort is the design), orphan-scan/retention cleanup, and differential-copier release gates.

What remains before a 1.0 claim is primarily verification/evidence, plus one product-scaling gap: a bounded single-directory enumeration fallback (§5.6). The on-request validation pass is §12.10; optional performance candidates live in BENCHMARKS.md; disposition history is in `docs/REVIEW_2026-07-29.md` and ADRs 0027–0034. **Deviation rule:** any future intentional difference from this plan is recorded before the deviating code merges — either as a normative edit here or an ADR — there is no separate deviations file. Open gate highlights:

- **Verification matrices (§12.3/§12.4/§12.8)** — fault-injection at the wrapper boundary, exhaustive deterministic kill-point simulation, the bounded chaos binary with mutator mode, the adversarial E-case suite, destination sentinel snapshots, and emitted-instance schema validation.
- **Bounded huge-directory behavior (§5.6/E25)** — synthetic million-entry validation and an implementation that falls back without materializing an unbounded per-directory map.
- ~~Elevated graceful-VHDX ReFS matrix~~ — **moved post-v1 by owner decision (2026-07-29, ADR 0029): ReFS support ships at v1 as best-effort, verified by code review only.** The capability-flag design, `FileRenameInfoEx` publication, and EA/stream degradation paths are code-reviewed; the dedicated elevated matrix (blocked on elevation + Hyper-V tooling in the implementation environment) certifies them post-v1. LIMITATIONS.md states the user-facing meaning plainly.
- **Real-hardware checklist + bounded performance evidence (§8.7, §12.5 `[HW]`)** — operator-run, bounded write budgets, archived in BENCHMARKS.md with extent-count evidence. This evidence also arbitrates whether the buffered engine leaves anything material on the table; only a measured shortfall reopens the unbuffered question (post-v1 backlog).

Rule for this list: an item leaves it only by landing **with** its specified verification; nothing on it may be silently claimed earlier by docs, hints, or UI.

### 13.3 Extension seams — how future scope lands without a rewrite

Several currently-rejected capabilities may return later: networked drives/UNC paths, efficient same-drive copy, FAT/exFAT support, and WSL/Linux/macOS ports. None is enabled now; this section records where each would attach, so today's code review can protect those seams (2026-07-29 review verdicts confirmed them):

- **The platform boundary is `bigcp-win`'s `lib.rs` surface.** Core consumes only exported types (`SourceFile`, `DestinationTemp`/`DestinationStream`, `ObjectMetadata`, `VolumeInfo`/`VolumeCapabilities`, enumeration, reparse, EA, security helpers); destination writes are physically confined to the temp types. A future backend split is a *module→trait extraction* over roughly ten concepts, not a redesign — budget that extraction as the first task of whichever extension lands first, and fold the known `ReparseTemp`/`DestinationTemp` duplication (§13.2 registry) into it.
- **Network/UNC** is blocked by *policy checks*, not plumbing: the `path.rs` UNC gate and the `volume.rs` `DRIVE_REMOTE` rejection. Lifting it means deleting two guards, then owning SMB semantics (metadata fidelity, latency-aware profiles) — the honest cost was never the code path.
- **FAT/exFAT** returns through `VolumeCapabilities`: the capability-flag design (§4.4) already expresses "no streams/EAs/sparse"; re-admitting FAT-family means relaxing the §4.4 pre-flight gate and reintroducing exactly the timestamp-projection and rename-fallback layers ADR-0020 deleted — keep that ADR as the checklist of what must come back.
- **Efficient same-drive copy** attaches at the transport port (§13.2): the alternating-burst scheduler (§8.3) is a policy over the same read/write loops, and the same-physical-disk detection already exists in the profiler.
- **Cross-platform (WSL/Linux/macOS)** means a second platform crate behind the extracted trait boundary; `core`'s Windows leakage is confined to types (`FILETIME` i64s, attribute u32s, UTF-16 names) that the extraction must abstract. The `compile_error!` guard in `bigcp-win` stays until a real second backend exists.

Structural rules protecting these seams (review-enforced): no `windows_sys` import outside `bigcp-win`; no destination-write primitive outside `bigcp-win`'s two writer types (`DestinationTemp`, `DestinationFinal` — ADR 0030); capability decisions by flags, never filesystem-name switches; policy gates (UNC, FS, same-volume hint) stay in exactly one place each.

Post-v1 backlog (explicitly deferred): `bench` subcommand, ARM64 build, config file, `--move` (would require relaxing I1 — needs its own safety design), network tuning, differential copier runs (`testkit oscopy` + robocopy three-way, §12.6), unbuffered large-file I/O (only if the §8.7 evidence measures a material buffered shortfall — ADR 0028), and any resurrection of the deleted §13.2 items — each would need a fresh benchmark or user-need case, not a revival by default.

## 14. Documentation and maintainability

The bar: *a future maintainer can build, test, modify, and release without asking anyone anything.*

### 14.1 Document set (all in-repo, versioned with the code)

| Doc | Contents | Freshness rule |
|---|---|---|
| `README.md` | user guide: install, examples, flag reference, FAQ, safety model summary, removal-safety note | every user-visible change |
| `docs/SEMANTICS.md` | the §4 contract, user-facing wording; the *single* normative statement of behavior | changes require ADR + version bump |
| `docs/DESIGN.md` | concise as-built architecture; this plan remains the detailed engineering record and must be amended with any intentional deviation | every architectural PR |
| `docs/TESTING.md` | how to run every suite, add scenarios, run chaos/VHDX/real-hardware checklists | with test changes |
| `docs/MAINTENANCE.md` | code map (crate/module → §), the invariant list I1–I13 with their enforcing tests, release checklist, toolchain/deps policy, debugging cookbook (how to read a log/journal, decode a crash) | every release |
| `LIMITATIONS.md` | every deliberate limitation with rationale and workaround — the user-facing mirror of §2.4/§4's scope decisions | every scope change |
| `docs/ERRORS.md` | generated from `errors.rs` table: code → category → hint → resolution | generated in CI, never hand-edited |
| `docs/adr/NNNN-*.md` | Architecture Decision Records; the filename-ordered index covers platform, semantics, persistence, simplifications, measured performance changes, and profile surface. ADR 0033 removes configuration fields that had no execution effect; ADR 0034 makes auxiliary-data publication transactional. | one per contract/architecture change, forever |
| `docs/schemas/*.json` | log + report JSON Schemas, versioned | additive-only in v1 |
| `BENCHMARKS.md`, `CHANGELOG.md`, `CONTRIBUTING.md` | numbers per release · keep-a-changelog · PR checklist + dev setup | per release / per PR |

### 14.2 Code documentation rules

- Every module: header comment — purpose, invariants touched, concurrency notes, pointer to its DESIGN.md section.
- Every `pub` item in `win` and `core`: rustdoc (`#![deny(missing_docs)]`).
- Every `unsafe` block: `// SAFETY:` discharging each obligation; `unsafe` outside `win` is a compile error.
- Comments explain *constraints* ("timestamps after rename — NTFS tunneling, see §4.3"), never narrate code.

### 14.3 Code standards (CI-enforced)

`rustfmt` default · clippy with `unwrap_used`, `expect_used`, `panic` denied in `core`/`win`/`cli` runtime paths (tests exempt) · `cargo-deny` (licenses, dupes, advisories) + `cargo-audit` · no magic numbers for Win32 constants (only `windows-sys` names) · error types via `thiserror`, Win32 code always preserved · bounded channels only · **JSONL is the sole telemetry/audit narrative** — no parallel `tracing`-span stack (two competing audit narratives would be worse than one complete one; spans may be added later only with a proven zero-cost-when-disabled measurement). **Change checklist** (CONTRIBUTING.md — every item a verifiable condition, author human or agent): does this touch an invariant I1–I13? → name the test that still enforces it; does it add I/O to a hot path? → attach the §8.7-methodology benchmark; does it change §4 semantics? → ADR + SEMANTICS.md + LIMITATIONS.md + schema review; engine/journal changes → chaos run before release (§13). The checklist is the second line of defense — CI's mechanical gates are the first (§7.4).

### 14.4 Decision process

Any change to: the §4 contract, on-disk formats (journal/log/report), safety invariants, or default tuning values ⇒ ADR with context/decision/consequences. ADRs are append-only history; MAINTENANCE.md indexes them.

### 14.5 Glossary (maintained in MAINTENANCE.md)

QD, MTL, VDL, UASP, BOT, SLC cache, SMR/CMR, 4Kn/512e, ADS, EA, reparse point, junction, tunneling, watermark, ring, join, oracle, breaker, engine, stream, FMEA — each with a two-line definition and a pointer to where it matters in the code.

### 14.6 Documentation self-sufficiency criteria

The docs are complete when each of the following is achievable by a **fresh implementer with no prior context — human engineer or AI agent alike — from the repository alone**: (1) build and run every test suite (TESTING.md); (2) add a new error-category hint (MAINTENANCE.md cookbook); (3) add a new scenario YAML and make it pass (TESTING.md); (4) produce a release candidate (release checklist in MAINTENANCE.md). Procedures (1)–(3) are themselves exercised by CI where scriptable; any friction discovered in practice — including an agent failing to complete one from the docs — is filed and fixed as a documentation bug. This is the criterion §12.9 references.

## 15. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Cache-manager throttling stalls on very large buffered copies | throughput dips | bounded benchmark evidence (§8.7) watches for them; the sequential loop plus preallocation minimizes dirty-page pressure; a measured material stall pattern is the one trigger that reopens unbuffered I/O (ADR 0028) |
| USB bridges lying to IOCTLs / dropping under load | wrong tuning, mid-run dropouts | conservative low-confidence profile (§5.5), deliberately modest static USB profiles (§8.2), device-gone breaker + resumable abort (§5.13) |
| SLC-cache cliffs / SMR collapse misread as "bigcp is slow" | user mistrust | bottleneck analyzer + honest hints (§5.14); BENCHMARKS.md educates |
| AV filters serializing creates | small-file throughput ceiling | measured + hinted, never auto-tampered (§5.14); documented expectations |
| OneDrive hydration storms | surprise bandwidth/disk usage | placeholder detection, prominent count, `--skip-cloud` (§4.6) |
| Windows semantic changes (rename flags, cloud tags, new FS) | breakage on new builds | runtime feature-detect chains (§5.2), differential suite vs OS engine (§12.6) catches drift |
| `windows-sys`/`ratatui` churn | build breakage | pinned versions + lockfile; upgrade PRs run full matrix |
| xxh3 non-cryptographic | adversarial collision (not a corruption risk) | documented limitation; tamper-proofing is explicitly out of scope per VISION (F30) |
| Scope creep toward robocopy flag parity | complexity erodes reliability | §2.4 non-goals + ADR gate; "defaults good enough to need no flags" is the product thesis |
| Fancy TUI hiding the truth | missed errors | summary block always printed; log is source of truth; TUI storm-safe (§5.13) |

## 16. Appendices

### Appendix A — robocopy flag mapping (required defaults → bigcp behavior)

| robocopy | meaning | bigcp |
|---|---|---|
| `/E` | recurse incl. empty dirs | default (only mode) |
| `/J` | unbuffered I/O | not carried over (and later removed from VISION's defaults): bigcp streams large files buffered with big sequential chunks (§5.9, ADR 0028); unbuffered returns only behind a benchmark |
| `/COPY:DTA` | data+timestamps+attrs, no ACLs | default (§4.2); ACL copy not implemented |
| `/DCOPY:DATE`¹ | dir data+attrs+timestamps+EAs | default: dir attrs at create + post-order timestamps (§5.10) |
| `/R:0 /W:0` | no retries | the only behavior — no retry arguments exist (VISION); re-running is the retry |
| `/V /FP` | verbose, full paths | JSONL log always full-detail with full relative paths (§10.2) |
| `/ETA` | show ETA | dashboard + plain status line (§6.4) |
| `/SJ /SL` | junctions/symlinks as links | default (§4.6) |
| `/MIR /PURGE /MOV` | deletion modes | **intentionally absent** (§2.4, I2) |
| `/Z` | restartable | superseded by verified checkpoint resume (§5.12) without `/Z`'s throughput cost |
| `/B` | backup mode | **not implemented** (VISION): permission failures are reported with a repair hint |
| `/DST` | DST tolerance | not needed — NTFS/ReFS store UTC; FAT (the reason `/DST` exists) is rejected (F15) |
| `/MT:n` | thread count | automatic per device profile; `--tune threads=` override |

¹ Robocopy's `/DCOPY` letters are `D` (directory data, i.e. dir ADS), `A` (attributes), `T` (timestamps), `E` (dir EAs), `X` (skip ADS); its default is `DA` — plain robocopy does *not* preserve directory timestamps. `/DCOPY:DATE` therefore reads as D+A+T+E, and bigcp implements **all four**: directory ADS, attributes, post-order timestamps, and EAs when present (`EaSize ≠ 0` — free detection, §4.2). File EAs are copied on the same mechanism (CopyFileEx-parity; strictly a superset of `/COPY:DTA`). One deliberate divergence: the NTFS compression attribute is not re-applied at the destination (F22, §4.2).

### Appendix B — Win32 API inventory (implementation checklist for `win`)

| Area | APIs |
|---|---|
| Handles/files | `CreateFileW` through Rust `OpenOptions` flag combinations (§5.8/§5.9), synchronous `ReadFile`/`WriteFile`, `CloseHandle`, `FlushFileBuffers` |
| Metadata | `GetFileInformationByHandleEx` (`FileBasicInfo`, `FileStandardInfo`, `FileIdInfo`, `FileAttributeTagInfo`), `SetFileInformationByHandle` (`FileBasicInfo`, `FileAllocationInfo`, `FileRenameInfoEx`, `FileDispositionInfoEx`), `BackupRead`/`BackupWrite` for EAs on dedicated synchronous handles, `GetSecurityInfo`/`SetSecurityInfo` for protected-DACL preservation on transactional replacements |
| Enumeration | `FileIdExtdDirectoryInfo` for directory enumeration; `FindFirstStreamW`/`FindNextStreamW` for path-based ADS listing followed by identity-checked non-following stream opens (§4.2) |
| Streaming I/O | plain synchronous sequential `ReadFile`/`WriteFile` in profile-sized chunks (no `FILE_FLAG_NO_BUFFERING`, no completion-port APIs, no alignment rules — §5.9, ADR 0028) |
| Reparse/sparse | `FSCTL_GET_REPARSE_POINT`, `FSCTL_SET_REPARSE_POINT`, `FSCTL_SET_SPARSE`, `FSCTL_QUERY_ALLOCATED_RANGES` (compression and block-clone FSCTLs deliberately absent — F22/F28; `FILE_SUPPORTS_BLOCK_REFCOUNTING` capability *checked* only for the hint §5.5) |
| Volume/device | `GetVolumePathNameW`, `GetVolumeInformationW`(`ByHandleW`), `GetDiskFreeSpaceW/ExW`, `GetDriveTypeW` (`DRIVE_REMOTE` → reject, F27), `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, `IOCTL_STORAGE_QUERY_PROPERTY` (device/adapter/seek-penalty/access-alignment), `IOCTL_STORAGE_GET_HOTPLUG_INFO`, `IOCTL_DISK_GET_CACHE_INFORMATION`, `GetFinalPathNameByHandleW` |
| Paths | `GetFullPathNameW`, `CompareStringOrdinal` |
| Privileges/locks | `CreateSymbolicLinkW` uses the unprivileged-create flag when Developer Mode permits it; bigcp does not adjust token privileges and uses no backup/restore/manage-volume or Restart Manager APIs |
| Misc | `GlobalMemoryStatusEx`, `SetThreadPriority`/`SetThreadInformation` (I/O priority), `GetStdHandle`/console mode (TTY detect), `CreateSymbolicLinkW` (`ALLOW_UNPRIVILEGED_CREATE`), `CreateDirectoryW`, `CreateMutexW` + security-descriptor helpers (machine-wide run lock, §5.12), `CreateHardLinkW` (future) |

### Appendix C — journal format (v1)

JSONL, one record per line, each with `crc` (CRC-32C of the line minus the crc field):

```jsonc
{"j":1,"ev":"job","run_id":"…","src":"…","dst":"…","opts_hash":"…","ts":"…","crc":"…"}
{"j":1,"ev":"checkpoint","rel":"vm/disk.vhdx","stream":"","temp":".bigcp-a1b2c3d4-7f19.part",
 "src_size":987654321098,"src_mtime":133497…,"watermark":268435456,
 "prefix_digest":"xxh3:…","crc":"…"}   // TENTATIVE (no temp flush, §5.12): resume verifies the prefix or restarts;
                                       // digest is snapshot(B=W), §5.9; per-stream ("" = unnamed); temp→final mapping lives here
{"j":1,"ev":"part_done","rel":"vm/disk.vhdx","crc":"…"} // temp renamed to final; checkpoint entries retired
{"j":1,"ev":"end","run_id":"…","counters":{…},"crc":"…"}
```

Loader rules: unknown `ev` → ignored (forward compat); bad CRC → drop line and everything after it (torn tail); `checkpoint` without matching *current* source metadata → discarded (I8). On clean run end the journal is compacted to its header; state-dir artifacts of the last 10 runs are retained (§5.12).

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

### Appendix E — VISION requirement traceability

Each explicit VISION.md requirement (F-numbers from §2.1) mapped to its normative rule and the test that pins it. Any future change must keep all three columns consistent.

| Req | Normative rule | Pinned by |
|---|---|---|
| F1 recurse incl. empty dirs | §4, §5.6 | scenario suite (all trees); E25 |
| F2 robocopy-default semantics | Appendix A | differential suite (§12.6) |
| F3 skip-if-same heuristic | §4.1 | classifier decision-table unit tests; E23 no-churn |
| F4 resume after termination | §5.12 | chaos harness (§12.4) |
| F5 links copied as links | §4.6 | E11, E12 |
| F6 error tally + hints, no retries | §5.13 | fault-injection matrix (§12.3) |
| F7 machine-parseable log + report | §10 | schema validation suite (§12.7) |
| F8 efficient verify mode | §5.17 | adversarial size+mtime-preserved test (§12.8) |
| F9 dashboard TUI + `report` reopen | §11 | TUI snapshot tests (§12.7) |
| F10 summary, breakdowns, bottleneck, peak | §5.14, §10.3 | report-content assertions in scenario suite |
| F11 no unnecessary disk I/O | §8.4 | syscall-budget + I/O-count benches (§12.6) |
| F12 40 GB+ verified partial resume | §5.12 (tentative checkpoints + mandatory prefix verify) | E40; deterministic kill-point simulation + bounded process-kill during a bounded large-file scenario (§12.4) |
| F13 overwrite decisions logged + summarized | §4.1, §10.2/§10.3 | replacement-logging assertions in scenario suite |
| F14 Windows 11 22H2 baseline | §2.3, §5.2 | CI runners ≥22H2; no OS-version branches (code review rule) |
| F15 NTFS/ReFS only; other filesystems rejected pre-flight | §4.4 | E03 rejection cell + NTFS/ReFS cells in the VHDX matrix (§12.5) |
| F16 both trees assumed exclusive/stable; violations detected | §4.8 | E14, E34, E36; chaos mutator (§12.3/§12.4) |
| F17 system-file exclusion + notification + flag | §4.7 | E32 |
| F18 simple preallocation only; SFVD prohibited | §5.9 | grep-guard test: no `SetFileValidData` call site exists |
| F19 no mid-run prompts; arguments instead | §10.1, §5.13 | TUI/plain snapshot tests assert no blocking prompt states (§12.7) |
| F20 `--replace` (default true) | §4.1 | E41 |
| F21 ADS/EA near-zero cost when absent | §4.2, §5.8 | syscall-budget bench: exactly one in-memory `FileStreamInfo` per *copied* file, none per skipped file, zero extra opens, zero `BackupRead` calls on EA-less trees (§12.6) |
| F22 compression dropped; sparse maintained | §4.2 | NTFS/ReFS VHDX matrix cells assert both (§12.5) |
| F23 tests confined + harmless | §12.0 | sandbox lint + containment oracle on every integration run |
| F24 class-based tuning, relative gates | §8.2, §8.7 | gate definitions reviewed for machine-relative form (§12.6) |
| F25 no temperature monitoring (withdrawn) | §5.14 | slowdown communicated via throughput-signature hints — SLC/thermal detector test |
| F26 one run per exact destination root | §5.12 | E33 |
| F27 local volumes only; UNC rejected | §4.5 | E44 |
| F28 one product engine; OS engines test-harness only; no clone (hint allowed) | §5.4, §5.9, §12.6 | differential vs `testkit oscopy`; ReFS hint path in VHDX matrix (§12.5) |
| F29 static class profiles + override flags, no runtime adaptation | §6.5, §8.2 | profile-selection unit tests; determinism assertion (same devices → same settings) |
| F30 single hash (xxh3-128) | §5.11 | no algorithm option exists; digest assertions throughout §12 |
| F31 abort-and-rerun only | §5.13 | E16; no reconnect states in TUI snapshots (§12.7) |
| F32 exactly two verification forms | §5.17 | CLI grammar test; adversarial size+mtime-preserved case (§12.8) |
| F33 ~1 M-entry directory target; minimal argument surface | §5.6, §10.1 | E25; flag-count lint in CI |
| F34 last-access best-effort/informational | §4.1, §5.17 | verify reports atime separately; never a pass/fail input (matrix test) |

---

*End of plan. Implementation starts at §13 layer 1; the first document to split out of this plan is `docs/SEMANTICS.md` (§14.1), extracted from §4 as soon as the reference copier exists.*
