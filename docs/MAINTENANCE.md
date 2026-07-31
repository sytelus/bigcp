# Maintainer guide

## Code map

| Area | Responsibility |
|---|---|
| `crates/win/src/endpoint.rs`, `path.rs`, `metadata.rs`, `volume.rs`, `device.rs`, `extents.rs`, `lock.rs`, `util.rs` | Local/UNC/WSL classification, lossless paths, 128/64-bit handle identity, fast/fallback 256 KiB enumeration, local/handle-bound-remote volume facts, query-only local device/extent facts, run lock, shared fail-closed helpers. |
| `crates/win/src/file.rs`, `streams.rs`, `ea.rs`, `sparse.rs`, `reparse.rs`, `security.rs` | Read-only source and capability-bearing destination primitives; the only unsafe boundary. Interrupted reads are retried here; native/provider lengths, EA records, sparse ranges, and stream suffixes are validated before core sees them. |
| `crates/core/src/model.rs`, `options.rs`, `filesystem.rs`, `classify.rs`, `copy.rs`, `transport.rs`, `worker.rs`, `engine.rs` | Work model, validated options, immutable source/destination semantic policy, endpoint-aware join, terminal outcomes, isolated standard/generic-redirector/WSL/same-spindle transports, bounded scheduling, direct-plain-small and transactional auxiliary/sparse/large copy. |
| `crates/core/src/artifact.rs`, `journal.rs`, `audit.rs`, `report.rs`, `stats.rs`, `devprofile.rs` | Shared exact-handle artifact publication, one-record-lookahead resume replay, disjoint public artifact roles, throughput windows, and exact static-profile buffer budgets. |
| `crates/core/src/verify.rs` | Post-copy and standalone verification. |
| `crates/tui` | Immutable-snapshot live UI and saved report browser. |
| `crates/cli` | Grammar, option validation, exit mapping. |
| `crates/testkit` | Structurally confined generator and independent oracle. |

## Invariant index

| ID | Rule | Mechanical enforcement/evidence |
|---|---|---|
| I1 | Source handles are read-only. | `SourceFile`/`SourceStream` choke points; unsafe denied outside `win`. |
| I2 | Never delete an unowned destination. | Payload and state-artifact delete-on-close is bound to the created handle; resumed temps require journaled file identity; temporary identifiers are one safe component; no path-delete fallback or mirror/purge command. |
| I3 | Every replacement preserves the old destination until safe commitment, except the documented direct-plain-small rerun window. | ADS/EA, sparse, and large files use sibling temps; plain small files validate the destination snapshot on the exact opened handle before truncation. |
| I4 | `copied` only follows data, metadata, optional flush, and close/publication. | Both completion strategies in the one product engine return `EngineResult` only after their protocol succeeds. |
| I5 | Multi-part logical files and large-file final names never contain partial data; interrupted direct plain-file work is repairable by rerun. | Transactional ADS/EA/sparse/large coverage plus direct-plain-small interruption/rerun tests. |
| I6 | Counters reconcile. | `Counters::reconcile` at run end and unit tests. |
| I7 | Every per-object failure is auditable. | Typed `OperationError`, coordinator-only outcome/audit ownership, and preflight rejection of colliding log/report/state/journal roles. |
| I8 | Journal never creates a skip. | Journal API exposes checkpoints only; one-record-lookahead replay, every-byte torn-tail/interior-record tests, and exact-handle atomic compaction preserving only the job plus live hints. |
| I9 | Memory/work queues are bounded. | `crossbeam_channel::bounded`; standard transport reserves one coordinator chunk, redirector transport reserves two chunks per active stream, and same-spindle coordinator/worker activity is serialized under one burst cap. |
| I10 | No source-tree writes. | All write constructors accept destination/audit paths; preflight audit containment. |
| I11 | Destination mutations revalidate targets. | Identity/kind/size/mtime/attributes/reparse-tag snapshot before repair/replacement; directory stream, EA, and metadata updates recheck identity on their write handle; direct READONLY clear/retry restores through that identity and reports rollback failure (ADR 0044). |
| I12 | One writer per exact destination. | Global mutex with exact-root hash. |
| I13 | Resume verifies the prefix. | Source/temp identities (128-bit or driver-backed legacy 64-bit), source size/mtime, prefix reread, and exact xxh3 boundary digest must all match. |

Changes to an invariant require a focused test and an ADR. The current suite is
not a substitute for the plan's future fault-injection and chaos release gates.

## Build and dependency policy

The pinned toolchain lives in `rust-toolchain.toml`. `Cargo.lock` is committed.
Runtime dependencies must be mature, bounded in scope, and compatible with the
dual license. Win32 calls use `windows-sys`; do not hand-copy constants when the
binding provides them. New unsafe code belongs only in `bigcp-win`, with a
`SAFETY` comment discharging pointer, length, alignment, ownership, and lifetime
obligations.

The repository launcher statically links the CRT and locates servicing versions
of MSVC/SDK. Do not bake an installed minor version into project files.

## Common changes

### Add an error category

1. Add the serialized variant in `ErrorCategory`.
2. Map relevant raw Win32 codes in `category_for`; keep official constant
   families such as `ERROR_CLOUD_FILE_*` in `bigcp-win`; preserve the original
   code.
3. Add an actionable sentence in `hint_for`.
4. Update `docs/ERRORS.md` and report/log schemas if their enum is closed.
5. Test raw-code classification and grouping by top-level folder.

### Add a scenario field

Keep every path relative and pass it through `SandboxRoot::child`. Add checked
byte accounting before allocation or creation. A scenario may never expand its
declared write budget. Update `docs/TESTING.md` and a bounded example YAML.

### Change copy semantics

Update `docs/SEMANTICS.md`, add an ADR, review `LIMITATIONS.md` (governing-file
edits require explicit owner authorization and refreshed frozen hashes),
update schemas, and run the independent oracle. A completion/publication or
journal change also requires future chaos coverage before release.

## Artifact debugging

JSONL records contain `ts` and `ev`. Find `run_start`, then follow `file` and
`error` events; file and link failures carry their error inline, while other
non-file failures use the dedicated event. Each contains category, operation,
relative path, raw Win32 code, message, and hint. A requested same-run read-back
adds one bounded `verification` event before `run_end`, including every retained
mismatch detail. `run_end` is the authoritative counters/audit/integrity
closure. A missing `run_end` means interruption or audit failure, not failure
of already committed files.

Journal records are not user reports. Each line contains version, tagged event,
and CRC. Replay retains one line of lookahead and at most one MiB of any one
record; it streams past an oversized record without allocating its full size.
A torn/invalid last line is truncated; an invalid interior line is skipped
without trusting it or deleting later valid records; an unsupported version is
left untouched and disables checkpointing for that run. A new checkpoint
records a temp sibling, source and temp filesystem identities, stream key,
source size/mtime, watermark, and prefix digest. Older identity-less records
load but cannot authorize resume. Never repair a journal manually or infer
completion from `part_done`; rerun normal copy. Clean-end compaction atomically
retains the current job header plus live checkpoints; audit artifact retention
is operator managed. Report and compaction siblings retain the creating handle,
deny delete sharing, and carry delete-on-close until handle-bound publication
(ADR 0041).

Reports are versioned aggregate JSON. `bigcp report FILE --plain` provides a
stable terminal summary; the full document contains devices, timeline,
replacements, warnings, grouped failures, extras, hints, and verification.

## Glossary

- **ADS:** NTFS/ReFS alternate `$DATA` stream attached to an object.
- **EA:** opaque extended-attribute set copied via the backup stream protocol.
- **Join:** destination-aware exact (WSL) or Windows-ordinal case-insensitive
  matching of one source and destination directory listing without
  per-source-item destination stats.
- **Engine:** the single product-owned `copy_file` execution path. Its direct
  plain-small and transactional auxiliary/sparse/large strategies share result
  and accounting contracts but have distinct completion protocols.
- **Watermark:** contiguous temp prefix eligible for a checkpoint.
- **QD:** queue depth — in-flight I/O count per device side; the shipped
  large-file loop issues one synchronous request at a time. Small-file
  parallelism comes from the worker pool.
- **MTL:** adapter-reported maximum transfer length used to clamp chunks.
- **VDL:** valid data length; bigcp never uses `SetFileValidData`.
- **UASP/BOT:** USB storage transports; BOT-like uncertainty selects a
  conservative profile.
- **4Kn/512e:** physical sector presentation modes (informational only —
  buffered I/O has no alignment rules, ADR 0028).
- **SLC cache:** SSD write cache whose exhaustion can cause sustained slowdown.
- **SMR/CMR:** magnetic recording layouts; SMR can collapse on long writes.
- **Tunneling:** NTFS name reuse behavior; final metadata is applied after
  rename to override it.
- **Oracle:** independent, simple full-tree comparator in `bigcp-testkit`.
- **Breaker:** stop-dispatch policy for device loss or capacity exhaustion.
- **Filesystem policy:** one immutable source/destination contract for timestamp
  and attribute representation, hard limits, name matching, preallocation, and
  post-write metadata behavior; optional operations remain driven by probed
  capability flags.
- **Endpoint:** the access route independent of filesystem type: local, generic
  UNC/mapped drive, or WSL UNC. Only local endpoints enter device/topology
  IOCTLs; WSL owns exact name matching and projected Linux interoperability.
- **Same-spindle transport:** the static policy selected only when physical
  extents intersect and media is rotational; source reads and destination
  writes run in bounded phases to reduce mechanical head switching.
- **Relative NTFS create:** distinct-drive local NTFS plain-small workers may
  cache one identity-verified destination-parent capability and open children
  by final component. Selection belongs in `copy.rs`, the bounded one-entry
  cache in `worker.rs`, and all native handle mechanics in `file.rs`; never
  spread this path into another filesystem, endpoint, or transport without a
  separate decision and evidence.
- **Generic redirector transport:** generic UNC/mapped paths use two bounded
  buffers; one synchronous source read overlaps one destination write, and
  only non-checkpointed independent streams may use parallel workers.
- **WSL transport:** the separate Plan 9 profile reuses the ordered two-buffer
  mechanics, uses its own 8 MiB/16-worker constants, stripes WSL destination
  creates, marks destination handles sequential, and stamps projected metadata
  only after data. Keep those choices behind `EndpointKind::Wsl`; never spread
  WSL checks into the standard or generic-UNC data loops.
- **FMEA:** explicit failure-mode/effect analysis behind crash invariants.
- **Reparse point:** filesystem object carrying a tagged buffer (symlink,
  junction, cloud placeholder); never traversed, copied by tag policy.
- **Junction:** directory reparse point with an absolute local target; copied
  verbatim, never followed.
- **Stream:** one `name:$DATA` payload of an object; the unnamed stream is the
  file's ordinary data.
- **Ring:** a historical term from two deleted streaming designs (the IOCP
  overlapped ring, ADR 0027, and the unbuffered reader/writer pair, ADR
  0028). No completion ring exists: the standard transport is a sequential
  buffered chunk loop, ADR 0036 adds bounded synchronous same-spindle phases,
  ADR 0045 adds a fixed two-buffer redirector pipeline, and ADR 0046 gives WSL
  its own profile and scheduling seam over that pipeline (PLAN §5.8–§5.9).

## Release checklist

1. Ensure `git status` contains only intended changes and governing inputs
   match the reviewed hashes in the frozen-input checker.
2. Run format check, clippy `-D warnings`, full tests in a validated C: sandbox,
   `cargo deny check`, and `cargo audit`.
3. Confirm both JSON schema files parse and carry the canonical repository `$id`/version
   (the in-repo test); full emitted-instance-vs-schema validation tooling is
   release work — do not claim it before it exists.
4. Run the final production-validation pass (PLAN §12.10 — chaos/kill
   convergence, adversarial set, sentinel/schema checks, and the bounded NTFS
   benchmark protocol); it executes only on explicit owner request.
5. Build `--release --locked`, smoke `--help`, local copy/rerun, both
   verification forms, report reopening, and any available bounded UNC/WSL
   scratch endpoint. Never substitute an unapproved production share.
6. Update `CHANGELOG.md`, `BENCHMARKS.md`, and version; tag only a clean commit.

Until step 4 exists and passes, label binaries pre-1.0 and do not make v1.0
NTFS reliability/performance certification claims. ReFS, FAT/exFAT, generic
UNC/provider filesystems, and WSL are permanently best-effort under ADR 0042;
optional compatibility evidence does not certify them.

The live gate status and the difference between routine confidence and release
certification are maintained in `docs/PRODUCTION_READINESS.md`.
