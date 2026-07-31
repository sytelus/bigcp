# bigcp limitations and important behavior

This guide explains what bigcp does not guarantee, what you may notice during
a copy, and what to do about it. For the safest default experience, use local
NTFS source and destination volumes, keep both trees unchanged while bigcp is
running, wait for a successful final report before using the destination, and
run standalone verification for important copies.

## Read this first

| If this applies to you | What it means | What to do |
|---|---|---|
| You need bigcp's strongest certified path | NTFS is the only certification target. Every other filesystem or remote provider is best-effort. | Prefer NTFS. For important non-NTFS or remote copies, use `--verify` and later run `bigcp verify SRC DST`. |
| The source contains ACLs, ownership, auditing data, compression, or hard links | Those properties are not reproduced as source properties at the destination. | Use a tool and options designed to preserve those properties if they matter. |
| The copy is interrupted or fails | Some final-named small files may be incomplete; resumable large-file temporaries may remain. This is expected and repairable. | Do not use the destination as complete. Fix the reported cause and run the same command again. |
| Other programs may change either tree | Concurrent mutation is outside the supported contract and can create races that are not all detectable. | Stop writers and give bigcp exclusive use of both trees for the run. |
| You need a mirror | bigcp never removes destination-only files. | Remove extras yourself or use a carefully configured mirror tool. |
| You are copying to FAT/FAT32/exFAT | Metadata and link fidelity are reduced, FAT has a 4 GiB-minus-one-byte file limit, and unsafe removal is riskier. | Read the FAT-family section and explicitly accept the warning. Prefer NTFS when fidelity matters. |
| You are copying over UNC, a mapped drive, or WSL | Provider metadata, disconnect behavior, and durable storage differ from local NTFS. | Read the remote section and explicitly accept the warning. Verify important results. |
| You need proof against power loss | Normal completion is logical completion, and even hardware flushes are not universally honest. | Use `--flush`, safely remove external media, and run standalone verification later. |
| A single directory may contain about one million entries | The current coordinator holds one directory listing and destination name map in memory. | Treat such directories as pre-1.0 and uncertified; monitor memory or split the directory if practical. |

## Supported environment

- **Windows 11 22H2 or later, x64 only.** Older Windows releases and ARM64
  builds are not currently supported. Use another copy tool on those systems.
- **Local NTFS, ReFS, FAT/FAT32, and exFAT are accepted.** Within bigcp's
  documented copy contract, NTFS is the strict, highest-confidence path and
  the only certification target. ReFS and FAT-family support remain
  best-effort even when they reuse the same mechanics as NTFS. Other local
  filesystems, including UDF and third-party drivers, are rejected before
  copying starts.
- **UNC paths, mapped network drives, and WSL UNC paths are accepted
  best-effort.** Their providers can offer weaker metadata, availability, and
  durability guarantees than a local NTFS volume.
- **Running as administrator does not enable a backup mode.** bigcp never asks
  for backup privileges or bypasses ACLs. Elevation may change what Windows
  permits the account to open, but unreadable files still fail normally and
  are reported. Fix access or ownership and rerun.

## Before starting a copy

- **Keep the source and destination stable.** bigcp assumes exclusive access
  to both trees. It detects many changed, vanished, or substituted objects,
  but it cannot make arbitrary concurrent writers safe and does not create a
  Volume Shadow Copy snapshot.
- **Only one run may use an exact destination root on the machine.** A
  machine-wide lock refuses a duplicate run. Nested or overlapping destination
  roots are not detected; do not run them concurrently.
- **There is no up-front free-space forecast.** Sparse allocation, cluster
  rounding, and replacements make forecasts approximate. If the destination
  fills, the circuit breaker stops the run after repeated disk-full failures.
  Free space and rerun; completed files remain and are skipped.
- **Differing destination files are replaced by default.** Use
  `--replace=false` when you want them reported and left untouched. In either
  mode, destination-only paths are never removed.
- **Audit files must be outside both trees.** `--state-dir`, `--log`, and
  `--report` paths inside the source or destination are rejected. They may be
  on the same volume, just not beneath either active tree. A dry-run leaves
  the destination tree unchanged but still writes its audit state, log, and
  report.
- **Cloud placeholders hydrate by default.** Reading OneDrive or another
  provider's placeholder can download its content and consume network and
  local storage. Use `--skip-cloud` to exclude placeholders. bigcp never copies
  a provider-owned cloud reparse buffer as though it were a portable link.
- **Selected root-level Windows artifacts are excluded by default.** This
  includes `System Volume Information`, paging, swap, hibernation, and
  dump-stack files. Exclusions are reported. `$RECYCLE.BIN` is not excluded:
  a volume-root copy attempts it normally, and protected entries may report
  access failures. Use `--include-system` only when you have reviewed the
  implications of including the remaining OS artifacts.
- **Locked files fail immediately.** There are no retries, waits, or lock-owner
  discovery. Close the program using the file and rerun; Task Manager or
  Resource Monitor can help identify it.

## What is and is not preserved

| Property | Behavior |
|---|---|
| Regular file content | Preserved. This is always part of a successful file outcome. |
| Creation and last-write times | Exact on NTFS/ReFS; projected to the destination representation on FAT/exFAT and provider-dependent remote filesystems. |
| Last-access time | Set best-effort, but never used for skip equality and reported only as informational during verification. |
| Basic attributes | NTFS/ReFS receive `READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE`, and `NOT_CONTENT_INDEXED`; FAT/exFAT receive only the subset they can represent. Storage-managed and cloud attributes are not copied. |
| Alternate data streams and extended attributes | Preserved when both endpoints advertise support. Otherwise they are dropped with per-file warnings and report counts. |
| Source ACLs, owner, and auditing data | Not copied. New destination objects inherit destination security. |
| Protected destination DACL | Preserved when replacing an existing protected object. If it cannot be preserved, the replacement fails instead of silently weakening protection. |
| NTFS compression | Not reproduced. The content is read normally and stored uncompressed; compressed-source counts appear in the report. |
| ReFS integrity streams | Follow the destination directory or volume policy. bigcp neither copies nor overrides the source setting. |
| Hard links | Not detected or preserved. Each linked name becomes an independent file and may consume additional space. |
| Sparse allocation | Preserved when supported; otherwise the logical content is copied densely and the expansion is reported. |
| EFS encryption | Content is read as plaintext. bigcp asks a capable destination to encrypt it, but an incapable destination receives plaintext with an `efs_downgrade` warning. |
| Symlinks and junctions | Recreated without following them when supported. Their target text is copied verbatim, so an absolute target may be wrong or dangling at the destination. Creating symlinks requires Developer Mode or the appropriate privilege. |
| Unknown reparse types | Fail as `unsupported_reparse` by default. `--raw-reparse` opts into verbatim copying at your risk; the destination may not have the filter driver needed to interpret the data. |
| Destination-only objects | Reported as extras and never deleted. |

Type conflicts are errors. If the source has a file where the destination has
a directory, link, or other incompatible object, bigcp reports
`type_conflict` and leaves the destination object untouched. Resolve the
conflict yourself and rerun.

Names that differ only by case collide on ordinary case-insensitive Windows
destinations. WSL destinations use exact Linux name matching and may contain
both names. Copying such a WSL tree back to a case-insensitive destination
reports the collision instead of overwriting one name.

## Skipping, replacement, and interrupted runs

- **The fast skip test is size plus last-write time, not a content hash.** On
  NTFS/ReFS, time comparison is exact. FAT-family comparison uses its coarser
  representation. A file whose content changed while both size and timestamp
  were deliberately preserved can be skipped. Run `bigcp verify SRC DST` to
  detect that case.
- **ADS or EA-only changes can be missed by the copy-time skip test.** Querying
  them on every apparently identical file would add a per-file I/O operation.
  Standalone verification compares stream sets and EA data.
- **Plain small files are written directly to their final names for speed.** A
  hard kill can leave one or more in-flight files incomplete. A replacement's
  previous content is already gone once the direct overwrite begins. The next
  stable rerun sees the mismatch and repairs it from the untouched source.
- **Large, sparse, ADS-bearing, and EA-bearing files publish transactionally.**
  They use opaque sibling temporaries and an atomic rename so their multi-part
  state is not exposed as a completed file.
- **A kill can rarely strand an opaque ADS temporary.** Opening a named stream
  requires a short interval in which delete-pending disposition is cleared.
  A process kill in that window may leave one reported `.part` object, but not
  a partially published logical file or damaged previous destination.
- **Checkpoint watermarks are tentative.** Checkpoints are not individually
  flushed. Resume always rereads and hashes the temporary prefix before
  trusting it, so a valid prefix continues and a damaged or missing prefix
  restarts safely.
- **Abort and rerun is the recovery model.** A disconnected device/share,
  stopped WSL distribution, disk-full breaker, or other fatal condition stops
  the run. bigcp does not wait for reconnection. Fix the cause and rerun.
- **Graceful cancellation finishes already-running small files.** Large files
  stop between chunks and retain only verified resumable state. A hard process
  kill has the weaker direct-small-file behavior described above.

Until the run reports success, treat the destination as in progress. The
presence of a file is not the completion signal.

## Durability and verification

- **Normal completion is logical, not guaranteed power-loss durability.** Data
  and metadata were accepted by Windows and publication completed, but recent
  writes may remain in an OS, bridge, controller, or drive cache.
- **`--flush` requests per-file durable completion.** Some hardware and USB
  bridges do not fully honor cache-flush commands, so bigcp can report the
  request but cannot certify the hardware's behavior. There is no volume-level
  flush option.
- **Always use Safely Remove for external media.** This matters especially for
  FAT/exFAT, which lack an NTFS-style metadata journal. After unsafe removal or
  power loss, run standalone verification; move or remove any reported bad
  destination object and rerun the copy.
- **Same-run `--verify` can read from cache.** It catches application and
  filesystem mistakes, including wrong bytes, truncation, streams, EAs,
  attributes, and timestamps for files written in that run. It cannot prove
  that a physical platter or flash cell was reread.
- **Standalone verification is the authoritative tree comparison.** Run
  `bigcp verify SRC DST` later, after caches have naturally moved on, when the
  result is important. It compares both complete trees; post-copy `--verify`
  covers copied files only, not directories and reparse objects.
- **xxh3-128 is an accidental-corruption check, not a cryptographic proof.** It
  is not designed to resist a deliberate attacker constructing collisions.
- **Remote durability belongs to the server.** SMB, third-party providers, and
  WSL may acknowledge writes or flushes while data remains in a remote cache.
  Use the server's snapshot or backup guarantees when physical durability is
  required.

## FAT, FAT32, and exFAT

A real copy to a FAT-family destination requires one default-no startup
confirmation. Scripts and other noninteractive runs must pass
`--accept-degraded-filesystem`. Dry-run and standalone verification do not
need acceptance because they do not change the destination tree.

- **FAT files cannot exceed 4,294,967,295 bytes.** An oversized source fails
  before its destination is opened. exFAT supports the 64-bit file sizes used
  by bigcp.
- **Timestamps are coarser and range-limited.** FAT creation time has 10 ms
  precision and last-write time 2 s; exFAT creation and last-write use 10 ms.
  Drivers generally support calendar years 1980–2107. A value the driver
  cannot encode fails instead of being invented. FAT local-time/DST conversion
  may cause a safe extra copy after a seasonal clock change.
- **ADS, EAs, source ACLs, EFS state, sparse layout, and links cannot be fully
  represented.** Streams and EAs are dropped with warnings, encrypted content
  may land as plaintext, sparse files expand, and symlinks/junctions fail
  rather than being followed or flattened.
- **Only `READONLY`, `HIDDEN`, `SYSTEM`, and `ARCHIVE` attributes transfer.**
  Last-access time is informational and especially coarse.
- **The destination driver decides which names it accepts.** bigcp reports
  illegal characters, component-length limits, and directory-entry failures;
  it never sanitizes or renames source paths.
- **There is no metadata journal.** Process interruption remains rerunnable,
  but power loss or unsafe removal can damage filesystem structures beyond the
  current file. `--flush` cannot add journaling.
- **Older driver operations are used when necessary.** FAT-family identity and
  rename fallbacks remain handle-bound and rerunnable, but they do not create
  NTFS semantics.
- **Verification is destination-projected.** A successful result proves the
  unnamed content and representable fields, not that unsupported NTFS
  metadata survived. Reports mark this as `projected: true`.

FAT-family behavior is best-effort regardless of optional compatibility-test
results. For important data, test the actual drive/enclosure and use both
verification forms.

## ReFS

- ReFS is accepted best-effort and uses exact timestamps plus its advertised
  capabilities, but it is not a certification target.
- If a ReFS volume reports no ADS or EA support, those properties are dropped
  with warnings and report counts. ReFS does not support EFS, so encrypted
  source content lands decrypted with a warning.
- Same-volume ReFS block cloning is not implemented. bigcp always streams the
  content. Explorer or robocopy may be dramatically faster for a same-volume
  clone-capable copy, and bigcp reports a hint rather than making a false
  performance claim.

## UNC, mapped drives, and WSL

A real remote copy requires one default-no startup confirmation. Scripts and
other noninteractive runs must pass `--accept-remote-paths`. When FAT-family,
remote, and removal-policy notices overlap, bigcp combines them into one
prompt. Dry-run and standalone verification remain read-only and need no
acceptance.

- **Remote topology is opaque.** bigcp does not send local disk, bus, extent,
  or cache-policy IOCTLs to a remote root and cannot know whether two shares
  use the same server disk. It therefore never selects the local same-spindle
  transport for remote paths.
- **Remote tuning uses bounded static defaults.** Generic UNC and WSL each use
  an independently owned 8 MiB request/16-worker Auto profile. Streamed files
  use two bounded buffers so the next source read can overlap the current
  destination write. Independent files below the 16 GiB default checkpoint
  threshold can use separate workers; checkpointed and sparse files stay
  coordinator-owned. Equal current UNC/WSL values do not merge their policies.
  These settings are correctness-tested, not certified as optimal for every
  network or provider. `--profile` and `--tune` remain available.
- **A single remote handle still has synchronous I/O depth one.** The pipeline
  overlaps source and destination stages; it does not issue an unbounded set of
  SMB requests or bypass the server. Link latency, server disks, signing,
  encryption, compression policy, antivirus, and provider caching can remain
  the bottleneck. No generic UNC performance result has been measured yet on
  an approved scratch share, so compare with robocopy on your own disposable
  workload before choosing manual overrides.
- **Generic UNC fidelity depends on the provider.** Known filesystem names use
  their corresponding policy and reported capabilities. Unknown providers
  require regular content and last-write time but do not claim Windows
  creation/access times or attributes. Provider timestamp limitations may
  cause a conservative recopy or verification failure.
- **Credentials, quotas, DFS, Offline Files, snapshots, and share availability
  remain administrator concerns.** bigcp reports the resulting Windows errors
  and does not retry in the same run.
- **WSL has a deliberately narrower contract.** Current and legacy WSL UNC
  aliases share one identity. Regular-file bytes and last-write time are
  preserved, destination names are matched case-sensitively, and unsupported
  reparse objects fail. Linux uid/gid/mode/xattrs and special-file semantics,
  Windows creation/access times and attributes, ADS/EAs, ACLs, EFS, and sparse
  layout are not reproduced through this Win32 engine.
- **WSL has its own performance path.** WSL destination creates are striped
  across the bounded worker pool instead of using NTFS directory affinity.
  WSL destination handles receive the sequential cache hint, new files avoid
  an unnecessary handle-metadata query, and the projected last-write time is
  set once after data rather than before and after it. These changes reduce
  Plan 9 round trips but have not been measured on an approved distribution.
- **A single WSL stream still has synchronous provider depth one.** The
  two-buffer path overlaps its source and destination stages, while concurrency
  across independent files supplies the larger request window. One enormous
  file can therefore remain limited by one WSL provider request at a time.
- **WSL UNC is not the fastest Linux-to-Linux path.** Windows access crosses
  WSL's translation boundary and may start the distribution. Prefer native
  Linux tools inside WSL for sustained copies entirely within Linux.

Important remote copies should use same-run verification and a later
standalone verification against the actual provider.

## Performance boundaries

- **Profiles are selected once at startup.** bigcp does not continuously
  retune itself. A merely unusual slow drive is not automatically retuned;
  inspect the recorded profile and use bounded `--tune` overrides when you
  have measurement evidence. Repeated device failures stop the run instead of
  causing aggressive retries.
- **Same-drive HDD optimization is implemented but not hardware-certified.**
  When both roots map to one rotational disk, bigcp batches small-file reads
  before writes and stages large/sparse/ADS data in bursts of up to 256 MiB.
  This avoids needless head switching but cannot make one spindle perform like
  two independent drives. The 1 MiB–1 GiB `same-spindle-burst` setting is
  capped by `mem=`. If Windows cannot prove shared physical extents and seek
  penalty, bigcp safely retains the standard path rather than guessing.
- **Same-drive SSDs retain the normal parallel transport.** SSDs have no seek
  penalty, so serial HDD phasing would normally reduce useful concurrency.
- **Large files use Windows buffered I/O.** There is no robocopy-style `/J`
  unbuffered mode. Local standard copies alternate one read and write;
  redirector copies overlap those stages with two bounded buffers. Windows
  caching is simple and fast on the primary external-drive scenarios, but a
  huge copy can temporarily evict other applications' cached data. That is
  harmless and self-correcting.
  Large files are also hashed while being read to protect checkpoint/resume
  integrity; there is no switch to disable that integrity check.
- **One directory is currently materialized in memory.** Total tree size is
  streamed and bounded elsewhere, but peak memory grows with the largest
  single directory. The synthetic million-entry fallback remains a pre-1.0
  evidence and implementation gap.
- **Reported ceilings are observations, not device specifications.** “Best
  observed sustained throughput” is the best window seen during the actual
  copy. Bottleneck explanations are confidence-rated hypotheses based on
  application-side I/O, not physical-device telemetry.
- **Some device behavior cannot be identified reliably.** USB bridges often
  omit or misreport capabilities, and drive-managed SMR behavior is inferred
  only from throughput. Unknown devices receive conservative defaults.

## Logs, reports, and command output

- **A run that loses both audit-log paths aborts.** bigcp tries to reopen the
  configured log and then fails over to the state directory. If both fail, it
  exits with code 6 without a normal final report or `run_end`. One operation
  already executing may finish while workers unwind, but no unaudited
  completion is claimed. Rerun to reclassify the destination.
- **Report error samples are bounded.** Reports retain counts plus a limited
  sample per category. The JSONL log contains every emitted event.
- **Standalone verification writes JSON to stdout only.** Redirect it when you
  need a saved artifact: `bigcp verify SRC DST > result.json`.
- **Logs and reports are not automatically expired.** Delete old audit files
  yourself if they accumulate.
- **The dashboard is intentionally focused.** It shows progress, rates, ETA,
  the currently active path, errors, and hints, but not per-file progress bars
  or historical sparklines. Use `--plain` for line-oriented progress,
  `--quiet` for only the final summary, or `--no-color` to keep the dashboard
  without color styling.

## Pre-1.0 evidence gaps

These are not silent passes. They remain open in
[`docs/PRODUCTION_READINESS.md`](docs/PRODUCTION_READINESS.md):

- deterministic fault and kill-point coverage for every completion boundary;
- bounded real-process chaos and the final adversarial validation set;
- synthetic million-entry single-directory validation and its bounded-memory
  fallback;
- topology-matched NTFS performance evidence, including same-spindle HDD;
- emitted-instance schema validation and the final production checklist.

Project safety rules prohibit endurance-scale writes, real million-file test
trees, forced disconnects, crashes, reboots, and other machine-stability risks.
Those scenarios cannot be reclassified as passed; safe simulation or bounded
evidence must be used instead. See [`docs/TESTING.md`](docs/TESTING.md).

## When another tool is a better fit

Use another tool or workflow when you need:

- Windows versions before 11 22H2 or an ARM64 build;
- source ACL, owner, auditing, hard-link, or compression preservation;
- backup-privilege access to unreadable files;
- automatic deletion of destination extras;
- ReFS block cloning or guaranteed best performance for same-volume ReFS;
- protocol-specific SMB acceleration, server-side copy, delta transfer, or
  reconnect-and-retry behavior;
- native preservation of Linux ownership, modes, xattrs, or special files;
- cryptographic tamper evidence rather than accidental-corruption detection.

For implementation rationale and invariant identifiers, see
[`PLAN.md`](PLAN.md), [`docs/DESIGN.md`](docs/DESIGN.md), and the
[`docs/adr`](docs/adr) directory.
