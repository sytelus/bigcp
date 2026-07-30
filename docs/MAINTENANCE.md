# Maintainer guide

## Code map

| Area | Responsibility |
|---|---|
| `crates/win/src/path.rs`, `metadata.rs`, `volume.rs`, `device.rs`, `lock.rs`, `util.rs` | Lossless paths, handle identity, 256 KiB enumeration, supported-volume and query-only device facts, run lock, shared error helpers. |
| `crates/win/src/file.rs`, `streams.rs`, `ea.rs`, `sparse.rs`, `reparse.rs`, `security.rs` | Read-only source and capability-bearing destination primitives; the only unsafe boundary. |
| `crates/core/src/model.rs`, `options.rs`, `classify.rs`, `copy.rs`, `worker.rs`, `engine.rs` | Work model, validated options, join, terminal outcomes, bounded scheduling, streaming, sparse/ADS copy, common finalizer. |
| `crates/core/src/journal.rs`, `audit.rs`, `report.rs`, `stats.rs`, `devprofile.rs` | Resume hints, public artifacts, throughput windows, static profiles. |
| `crates/core/src/verify.rs` | Post-copy and standalone verification. |
| `crates/tui` | Immutable-snapshot live UI and saved report browser. |
| `crates/cli` | Grammar, option validation, exit mapping. |
| `crates/testkit` | Structurally confined generator and independent oracle. |

## Invariant index

| ID | Rule | Mechanical enforcement/evidence |
|---|---|---|
| I1 | Source handles are read-only. | `SourceFile`/`SourceStream` choke points; unsafe denied outside `win`. |
| I2 | Never delete an unowned destination. | Delete-on-close is bound to the created handle; resumed temps require journaled file identity; no path-delete fallback or mirror/purge command. |
| I3 | Never truncate a replacement in place. | Replacement always constructs a sibling temp. |
| I4 | `copied` only follows commit. | `EngineResult` is returned after rename, metadata, optional flush, and close. |
| I5 | Final names never contain partial file data. | Uniform temp/rename; end-to-end atomic replacement test. |
| I6 | Counters reconcile. | `Counters::reconcile` at run end and unit tests. |
| I7 | Every per-object failure is auditable. | Typed `OperationError`, coordinator-only outcome/audit ownership. |
| I8 | Journal never creates a skip. | Journal API exposes checkpoints only; every-byte torn-tail test. |
| I9 | Memory/work queues are bounded. | `crossbeam_channel::bounded`; per-stream buffers/profile caps. |
| I10 | No source-tree writes. | All write constructors accept destination/audit paths; preflight audit containment. |
| I11 | Destination mutations revalidate targets. | Identity/kind/size/mtime/attributes/reparse-tag snapshot before repair/replacement; directory stream, EA, and metadata updates recheck identity on their write handle. |
| I12 | One writer per exact destination. | Global mutex with exact-root hash. |
| I13 | Resume verifies the prefix. | Source/temp identities, source size/mtime, prefix reread, and exact xxh3 boundary digest must all match. |

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
2. Map relevant raw Win32 codes in `category_for`; preserve the original code.
3. Add an actionable sentence in `hint_for`.
4. Update `docs/ERRORS.md` and report/log schemas if their enum is closed.
5. Test raw-code classification and grouping by top-level folder.

### Add a scenario field

Keep every path relative and pass it through `SandboxRoot::child`. Add checked
byte accounting before allocation or creation. A scenario may never expand its
declared write budget. Update `docs/TESTING.md` and a bounded example YAML.

### Change copy semantics

Update `docs/SEMANTICS.md`, add an ADR, review `LIMITATIONS.md` (do not edit the
frozen input for ordinary implementation work), update schemas, and run the
independent oracle. A finalizer or journal change also requires future chaos
coverage before release.

## Artifact debugging

JSONL records contain `ts` and `ev`. Find `run_start`, then follow `file` and
`error` events; file and link failures carry their error inline, while other
non-file failures use the dedicated event. Each contains category, operation,
relative path, raw Win32 code, message, and hint. `run_end` is the authoritative
counters/audit/integrity closure. A missing `run_end` means interruption or
audit failure, not failure of already committed files.

Journal records are not user reports. Each line contains version, tagged event,
and CRC. A torn last line is ignored; an unsupported version is left untouched
and disables checkpointing for that run. A new checkpoint records a temp
sibling, source and temp filesystem identities, stream key, source size/mtime,
watermark, and prefix digest. Older identity-less records load but cannot
authorize resume. Never repair a journal manually or infer completion from
`part_done`; rerun normal copy.

Reports are versioned aggregate JSON. `bigcp report FILE --plain` provides a
stable terminal summary; the full document contains devices, timeline,
replacements, warnings, grouped failures, extras, hints, and verification.

## Glossary

- **ADS:** NTFS/ReFS alternate `$DATA` stream attached to an object.
- **EA:** opaque extended-attribute set copied via the backup stream protocol.
- **Join:** case-insensitive matching of one source and destination directory
  listing without per-source-item destination stats.
- **Engine:** a bounded file transfer strategy; both paths share one finalizer.
- **Watermark:** contiguous temp prefix eligible for a checkpoint.
- **QD:** queue depth — in-flight I/O count per device side; always 1 per
  stream in the shipped sequential engine (aggregate parallelism comes from
  streams and workers).
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
- **FMEA:** explicit failure-mode/effect analysis behind crash invariants.
- **Reparse point:** filesystem object carrying a tagged buffer (symlink,
  junction, cloud placeholder); never traversed, copied by tag policy.
- **Junction:** directory reparse point with an absolute local target; copied
  verbatim, never followed.
- **Stream:** one `name:$DATA` payload of an object; the unnamed stream is the
  file's ordinary data.
- **Ring:** a historical term from two deleted streaming designs (the IOCP
  overlapped ring, ADR 0027, and the unbuffered reader/writer pair, ADR
  0028). No ring exists: the shipped engine is a sequential buffered chunk
  loop (PLAN §5.9).

## Release checklist

1. Ensure `git status` contains only intended changes and frozen inputs are
   byte-identical.
2. Run format check, clippy `-D warnings`, full tests in a validated C: sandbox,
   `cargo deny check`, and `cargo audit`.
3. Confirm both JSON schema files parse and carry the expected `$id`/version
   (the in-repo test); full emitted-instance-vs-schema validation tooling is
   release work — do not claim it before it exists.
4. Run the final production-validation pass (PLAN §12.10 — chaos/kill
   convergence, adversarial set, sentinel/schema checks, certified benchmark
   protocol); it executes only on explicit owner request.
5. Build `--release --locked`, smoke `--help`, copy, rerun, both verification
   forms, and report reopening.
6. Update `CHANGELOG.md`, `BENCHMARKS.md`, and version; tag only a clean commit.

Until step 4 exists and passes, label binaries pre-1.0 and do not make v1.0
reliability/performance certification claims.

The live gate status and the difference between routine confidence and release
certification are maintained in `docs/PRODUCTION_READINESS.md`.
