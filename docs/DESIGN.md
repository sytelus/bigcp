# As-built design

## Dependency direction

```text
bigcp (CLI)   -> bigcp-tui, bigcp-core, bigcp-win
bigcp-tui     -> bigcp-core
bigcp-core    -> bigcp-win
bigcp-testkit -> bigcp-win (dev dependency only)
```

`bigcp-win` is the only crate allowed to contain `unsafe`; it converts Win32
contracts into owned, safe Rust values. `bigcp-core` contains semantics,
scheduling, journaling, verification, audit, and reporting without direct
Win32 calls. The CLI consumes all three: display through `bigcp-tui`, options
and reports through `bigcp-core`, and the process-wide console cancel handler
through `bigcp-win`, injected into the display layer as a plain function
probe so `bigcp-tui` itself stays free of Win32. `bigcp-testkit` depends only
on `bigcp-win`, never on core copy logic, so its oracle is independent.

## Control flow

Preflight normalizes and opens local/UNC roots, resolves final paths, rejects
aliases and unsupported local volumes, queries capability and device facts
without write probes, selects a deterministic static profile, validates audit
containment, and acquires the destination lock. Remote endpoints use
handle-bound volume queries and never enter local device-IOCTL discovery. The
coordinator then performs an iterative directory join.
Directories are created before children and stamped after children.
Their created or enumerated identities are retained through the post-order
pass; stream, EA, and metadata handles must match those identities before any
destination update.

Plain files are classified from the joined snapshots under one immutable
destination `FilesystemPolicy`. Stream discovery can promote a nominally small
file with a large ADS. The single product engine routes plain small work to a
bounded fixed worker pool. Redirector paths may also route non-sparse streamed
files that do not require checkpoint persistence to that pool; checkpointed
and sparse files remain coordinator-owned. Workers receive immutable snapshots
and distinct destinations. Only the coordinator mutates counters, audit,
report aggregates, and journal state.

Plain small files use `DestinationFinal`: workers read and revalidate the
source, then create or identity-check-and-truncate the final destination before
one whole-buffer unnamed-stream write. FAT-family and remote files are
restamped on that same handle after data I/O; WSL defers its projected stamp
until that point instead of issuing a redundant initial Plan 9 metadata call.
New final names also skip a handle-metadata query because `CREATE_NEW` already
proves the returned object was just created as a regular file. The exact local
NTFS/ReFS policy retains its measured create-time stamp. Files
with destination-representable ADS/EAs, sparse files, and large/resumable files
use `DestinationTemp` and atomic publication. Unsupported source ADS/EAs are
counted and warned but do not force that slower route. Large transfers are
bounded synchronous streams. Standard-transport unnamed large streams and
every immutable generic `Redirector` and `Wsl` stream use exactly two
buffers and a scoped reader so one source read overlaps one destination write
(ADR 0055); hashing and writes remain ordered, and pipeline segments stop at
checkpoint boundaries. Standard-transport sparse ranges and named streams
keep the one request-sized buffer.
WSL remains a distinct transport/profile identity and stripes plain-small
dispatch across its worker pool whenever either side is WSL, while generic
UNC retains directory affinity. A generic-redirector source switches to the
same striping only when the preflight volume queries — timed at zero extra
I/O — measured a network-class round trip (≥250 µs floor); loopback-class
shares keep affinity, and the once-per-run decision is logged (ADR 0053).
Large non-sparse, unnamed-only,
non-checkpointed, non-resume files with exactly one WSL side additionally
move as 2–8 chunk-aligned parallel segments of the single opaque temp — each
segment on its own identity-checked source open and identity-proven
`SegmentWriter` — because one Plan 9 handle caps below the boundary's
aggregate throughput (ADR 0052); the digest, cleanup, and publication tail
are the ordered path's own.
When local volume disk extents intersect and an effective profile is
rotational, an immutable `SameSpindle` transport instead stages a bounded
multi-request burst before each destination phase; the coordinator drains the
phased small-file worker before inline work. Plain small files on that topology
are read/revalidated as one bounded batch, written as one destination phase,
then source-revalidated before success. Checkpoint boundaries retain exact
offset-ordered xxh3 snapshots. All paths
return the same `EngineResult` and only the coordinator records terminal
outcomes. Channels and buffers have explicit caps.
Distinct-drive local NTFS plain-small workers additionally cache one verified
destination-parent capability. `bigcp-win` opens the final child relative to
that handle with `NtCreateFile`, so repeated files in one directory do not
reparse the same absolute parent path. New/replacement dispositions feed the
same `DestinationFinal` validation and completion code; a parent-capability
open failure falls back to the absolute constructor. Selection is deliberately
absent from same-physical-disk, ReFS, FAT/exFAT, UNC, WSL, sparse, large, and
transactional paths (ADR 0048).
Known symbolic links are created through `CreateSymbolicLinkW` so Developer
Mode can authorize unelevated creation; junction and opted-in unknown tags use
the raw reparse control path. All are built under owned opaque sibling names.

## Persistence and audit

The CRC32C-framed JSONL journal is a resume hint, never a completion database.
(One non-content use of a second hash exists: run-lock and job identity derive
from SHA-256 of the resolved root paths — identity naming, not data integrity,
so VISION's single-content-hash rule is unaffected.)
Loading truncates only an invalid final/torn record. Invalid interior records
are ignored without being trusted or destroying later valid records; an
unsupported journal version fails closed without rewriting the file. Job
signatures bind checkpoints to the resolved source/destination pair and resume
protocol version, so harmless CLI-option changes do not erase safe hints. A
temp prefix is reread and hashed before resume. Each resumable record also binds the
source and temp to their volume serial plus filesystem file ID. NTFS/ReFS use
the 128-bit `FileIdInfo` path; FAT-family drivers can fall back to the legacy
64-bit ID. A stale, legacy,
type-swapped, or identity-mismatched candidate is ignored without modifying
the path it names.

Clean-end compaction retains the current job header and live checkpoints. It
shares the report's artifact publisher: a unique exclusive sibling is
synchronized and atomically replaces the old path through its creating handle.
That handle denies delete sharing and carries delete-on-close until publication,
so neither replacement nor cleanup can be redirected to a new occupant of the
temporary name (ADR 0041).

The versioned JSONL audit is lossless and rolls back partial line writes. It
reopens once, then fails over to the state directory. The final JSON report is
serialized to the same handle-owned sibling primitive, synchronized, renamed
by handle, synchronized again, and closed before the terminal audit event is
emitted and durably synchronized. This ordering keeps report-fallback status
consistent with `run_end`.

## Extension seams

The narrow Win32 crate, capability-based core requests, and immutable
`FilesystemPolicy` deliberately isolate filesystem differences and future
ports:

- **UNC/network (implemented):** `win::endpoint` classifies local, generic UNC,
  and WSL access independently of filesystem type. Direct UNC syntax is
  extended-length normalized, legacy `wsl$` canonicalizes to `wsl.localhost`,
  and mapped drives resolve their opened final path. `volume.rs` preserves the
  existing Win32 local-volume path and uses handle-bound native filesystem
  queries only for redirectors; `device.rs` returns an opaque remote device
  record without local IOCTLs. Core consumes one immutable endpoint/filesystem
  policy. `core::transport` owns generic-redirector and WSL identities over the
  bounded two-buffer pipeline; `worker.rs` confines parallel stream scheduling
  and either-side WSL striping; `engine.rs` owns the segmented parallel
  large-file strategy (`segment_plan`/`copy_streamed_segmented`); `file.rs`
  owns sequential handle hints, deferred WSL stamping, and the
  identity-proven `SegmentWriter`. Remote profiles, case matching, projection,
  preallocation, transfer mechanics, and disconnect classification can
  therefore evolve without touching local hot paths.
- **Same-spindle transport (implemented):** `transport.rs` owns topology policy
  data and burst mechanics, `worker.rs` owns phased plain-small batching, and
  `engine.rs` applies it to dense/sparse/named streams behind the existing
  result/accounting contract. Future tuning stays inside this seam.
- **Same-volume cloning:** a separately capability-gated clone transport may
  still be added later; classification, audit, and verification must not change.
- **FAT/exFAT (implemented):** the policy owns timestamp projection, attribute
  projection, final restamping, and FAT size limits; volume capability flags
  govern optional streams, EAs, sparse storage, EFS, ACLs, and POSIX rename.
  `bigcp-win` contains the only 128-to-64-bit identity/enumeration and
  extended-to-legacy rename fallbacks. The exact NTFS/ReFS policy stays a
  branch-free equality path and does not pay FAT-only syscalls.
- **Native Linux/macOS:** replace `bigcp-win` with a platform facade providing
  path identity, enumeration snapshots, completion/publication, streams/xattrs,
  sparse extents, and link equivalents. WSL UNC interoperability is already
  available through the Windows facade, but a native Linux backend would be
  required to preserve uid/gid/mode/xattrs and special files. Core owns no
  UTF-8 assumption; Windows paths remain lossless UTF-16 in audit keys.

Before another backend is enabled, extend these existing narrow policy and
capability seams. Avoid a lowest-common-denominator abstraction: each backend
must state its atomicity, representation, and metadata guarantees.

## Performance model

Local device discovery uses official query-only IOCTLs: physical extents, bus
type, seek penalty, sector sizes, and maximum transfer length. When the
adapter's bus answer is unspecific (Intel VMD/RST reports RAID for NVMe), the
per-device descriptor's bus type is consulted before any conservative
fallback; the adapter maximum transfer length is recorded as a fact but no
longer clamps the composed chunk — it bounds one storport request, which the
I/O manager already honors by splitting buffered transfers (ADR 0055).
Remote discovery
uses provider-returned volume data and independently immutable generic-UNC/WSL
profiles; remote sources cap the composed worker count. Static profiles choose
per-side chunk size and workers. Both remote transports use two buffers per
active transfer, and independent non-checkpointed files may occupy separate workers.
WSL Auto uses 8 MiB/32 workers; either WSL side also stripes small files to
cover Plan 9 latency instead of applying the measured NTFS directory-index
policy, and eligible large one-sided WSL files transfer as bounded parallel
segments to overlap the measured per-handle ceiling (ADR 0052).
Intersecting local disk extents plus
rotational classification select one phased worker and a bounded same-spindle
burst; SSD overlap stays on the standard path. Manual values are range checked.
On standard transport the memory override reserves the two concurrently live
pipelined coordinator chunks before capping threshold-sized workers.
Redirector accounting
reserves the same two coordinator chunks and the larger of one
whole-small-file buffer or two chunks per worker. On same-spindle transport the drain-before-inline
rule permits one direct burst cap. All effective transport facts are reported.

The directory join avoids a destination `stat` per source file. Stream and EA
work is deferred until required. Statistics report application-side rates and
label bottleneck conclusions as hypotheses.

**Fragmentation stance.** On local volumes, parallel writers normally
interleave allocation and shred concurrently growing files into many extents — a real read-performance
cost on seek-penalty media. bigcp counters this structurally, exploiting the
one thing a copier always knows that ordinary writers do not: the final size
before the first byte. Dense large files are preallocated to their full source
size at temp creation (`FileAllocationInfo`, never `SetFileValidData`), so the
allocator reserves the whole run in one decision and later writers cannot
interleave allocations into that file. Small files are read whole and written in a
single shot — one allocation event, one extent (or MFT-resident storage).
Sparse files are deliberately exempt: dense preallocation would destroy the
holes being preserved, so their layout mirrors the source's own. This stance
is evidence-backed, not assumed: `bigcp-testkit extents` measures physical
extent counts (`FSCTL_GET_RETRIEVAL_POINTERS`, read-only) and benchmark
entries record that evidence per `BENCHMARKS.md`, which will catch any future
regression that quietly drops preallocation. Remote endpoints skip the dense
`FileAllocationInfo` hint because provider support and cost are not locally
knowable; this endpoint branch does not alter local allocation behavior.

## Constraints worth preserving

- No source write handles.
- No arbitrary destination deletion primitive.
- Direct final-name writes are limited to plain small files; replacement
  truncation follows same-handle validation of the classification snapshot
  (identity, kind, size, mtime, attributes, reparse tag). ADS/EA, sparse,
  and large files are transactional.
- No unbounded channels.
- No journal-powered skip.
- No audit artifacts inside active trees.
- No filesystem write probes during profiling.
- No alternate product copy backend hidden behind an option.
- Native and redirector output is bounds/order checked before parsing; stream
  suffixes and NUL-terminated Win32 strings cannot redirect a validated path.

Known intentional differences from the governing plan are recorded inline at
their PLAN.md sections and in the ADRs (0027–0052); there is no separate
deviations file.

The terminal layer is a read-only projection of coordinator snapshots and
completed reports. Display policy is selected before the copy starts:
redirected or `--plain` output uses stable progress lines, `--quiet` suppresses
live output, and an interactive terminal uses the dashboard even when color is
disabled. Formatting and contextual next-action logic are shared by live and
saved-report views; neither path can influence copy scheduling or semantics.
Cancellation follows the same read-only direction: the CLI installs
`bigcp-win`'s process-global console handler (`console` module — the first
Ctrl+C/Ctrl+Break latches a flag and is absorbed; a second, or any
close/logoff/shutdown signal, falls through to default termination), and both
display modes merely poll that flag, alongside the dashboard's own
`q`/Esc/Ctrl+C key handling, while the engine checks cancellation between
bounded work units.
