# Copy semantics (normative v1 contract)

This document is the single user-facing normative copy contract. `PLAN.md`
contains the governing engineering design; this file describes the implemented
pre-1.0 behavior intended for v1.

## Object equality and outcomes

For an ordinary file, `Same` means exact unnamed-stream size and last-write
time equal under the destination filesystem policy: exact `FILETIME` on
NTFS/ReFS and remote/WSL providers, within the representable 2-second interval
on FAT, and within the 10-millisecond interval on exFAT. Differences only in the destination-copyable attribute mask or
creation time are repaired without rewriting data. Any size or last-write
difference is replaced unless `--replace=false`. Large-file publication is
atomic; small-file replacement is direct and recoverable by rerun. Destination-only
objects are extras and are never changed. A file/directory/reparse type conflict
fails and is never auto-resolved.

Named-stream divergence and same-length EA payload divergence are not queried
for otherwise-`Same` files because that would add per-file I/O to the common
rerun path. A differing EA byte count is cheap enumeration metadata and is
repaired. Full standalone verification detects all payload divergence.

Every discovered file, directory, and link reaches exactly one terminal
counter outcome. Dry-run forecasts use separate `would_*`/`*_planned` counters
and never claim modeled work was completed.

## Preserved fields

| Object | Preserved |
|---|---|
| File | Unnamed bytes always; named `$DATA` streams and EA blob when both endpoint capabilities allow; creation and last-write time at destination granularity; `READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE`, plus `NOT_CONTENT_INDEXED` where supported. WSL/unknown-remote semantic projection guarantees content and last-write only. |
| Directory | Existence; supported named `$DATA` streams and EA blob; the same projected times and attribute mask, applied post-order; root metadata included. WSL/unknown-remote semantic projection does not claim Windows creation/access times or attributes. |
| Symlink/junction | Link target/reparse payload and tag on capable endpoints; supported named `$DATA` streams and EA blob; projected basic metadata. FAT/exFAT and WSL destinations cannot represent links in this engine, so the object fails. Unsupported WSL-source reparse objects also fail rather than being traversed. |

Last-access time is written best-effort but informational in verification
because reads legitimately change it. Sparse allocation is preserved when
supported and enabled; logical content remains the correctness criterion.
Compression and filesystem/HSM-managed attributes follow destination policy.

## Completion and replacement

Every file starts by opening the source read-only and validating its enumeration
snapshot. A replacement target is revalidated for identity, kind, size,
last-write time, attributes, and reparse tag before mutation.

Plain small files with one destination-representable unnamed stream and no
destination-representable EAs are read and
source-revalidated before destination mutation. A new final name is created
exclusively; a replacement is opened non-following, identity-checked on that
same handle, and then truncated in place, preserving its security descriptor.
The engine writes the one unnamed payload, revalidates the source, restamps it
after data I/O on FAT-family and remote destinations, optionally flushes, and only then
reports `copied`. A process kill can leave an incomplete
final-named plain file, but a mid-write file is shorter than the source and a
completed whole-buffer write has already completed its logical data.

Files with destination-representable named streams or EAs, plus large, sparse,
and checkpoint-capable files, use a unique
`.bigcp-<full-run-id>-<nonce>.part` sibling with delete-on-close armed. After
copy and source/target revalidation, a protected destination DACL is preserved,
the temp is atomically published, final metadata is applied, and an optional
flush completes before `copied`. This size-independent auxiliary-data routing
prevents a partially written stream/EA set from becoming visible under the
final name. Checkpointed partials may persist under opaque
names, but resume never trusts them: current source size/mtime, source and
temporary filesystem identities, and the exact temp-prefix digest must match.
Legacy identity-less checkpoint records are parseable but restart from zero.

## Streams, EAs, sparse files, EFS, and reparse points

Only `$DATA` streams are user data; directory `::$INDEX_ALLOCATION` and other
filesystem implementation streams are ignored. A large file ADS promotes its
owner to the journal-aware streaming path and successfully copied file ADS
bytes are included in logical copy accounting. EA transfer uses separate
synchronous buffered handles through `BackupRead`/`BackupWrite`.

When a destination cannot represent named streams or EAs, their source counts
are reported as dropped and they do not force the transactional path. A source
volume that advertises neither capability is never queried for it. This keeps
the ordinary FAT/exFAT and WSL data paths fast without weakening capable destinations.

Every source/destination named-stream handle is non-following and must report
the expected base-object identity before I/O. Directory EA and final metadata
updates perform the same identity check on the handle used for mutation.
Destination-only streams are not deleted: a full verify reports them as
divergence, preserving the product's no-delete contract.

Sparse files are marked sparse before EOF is established, are not densely
preallocated, and copy allocated ranges only. Holes participate in hashing as
logical zero bytes in offset order. When sparse storage is unavailable or
disabled, a dense representation with identical bytes is correct.

EFS source data is read as plaintext. The destination is asked to encrypt; a
failure is an explicit `efs_downgrade` warning. Symbolic links are recreated
with Windows' unprivileged-create flag (when Developer Mode permits it), while
junctions use their reparse payload. Unknown tags fail unless `--raw-reparse`
is given. On FAT/exFAT and WSL destinations, all reparse objects fail rather
than being followed or flattened; WSL-source Linux reparse objects that do not
map to supported Windows link data fail the same way. FAT's
4,294,967,295-byte file limit is checked before destination
mutation.

## Roots, exclusions, and audit artifacts

Roots are resolved through handles. Equal/nested roots and unsupported local
filesystems fail preflight. Local paths, generic UNC shares, mapped network
drives, and WSL's `\\wsl.localhost`/legacy `\\wsl$` paths are accepted; the
legacy WSL spelling canonicalizes before lock/state identity is derived. A real
FAT/exFAT destination requires `--accept-degraded-filesystem`, and any real
remote copy requires `--accept-remote-paths`, unless the interactive default-no
startup confirmation supplies the acceptance. All known limitations share at
most one prompt. Dry-run and standalone verification need neither acceptance.
An existing destination
root is pinned. A missing destination is created component-by-component except
in dry-run, where only its nearest existing ancestor is pinned.

Generic remote shares use handle-bound provider capabilities rather than local
disk IOCTLs. Known remote NTFS/ReFS/FAT/exFAT names use their filesystem policy;
an unknown remote name uses content plus exact last-write projection. WSL uses
exact, case-sensitive destination name matching and the same content/last-write
projection. Linux uid/gid/mode/xattrs and special-file semantics are never
invented by the Win32 engine. Network/WSL disconnect codes feed the ordinary
device-gone breaker; recovery remains abort-and-rerun with no mid-run prompt.

At a source volume root, `System Volume Information`, paging, swap,
hibernation, and dump-stack files are excluded unless `--include-system`.
`$RECYCLE.BIN` is not part of that policy and is attempted through ordinary
copy and error-reporting behavior by default. Cloud placeholders hydrate
unless `--skip-cloud`.

State, log, and report paths may share a volume with either tree but may not be
inside either tree. JSONL is the complete event audit; the JSON report is an
atomic aggregate. The report is published before the terminal `run_end`, whose
bytes are durably synchronized. If both primary and fallback logging fail, no
new work is claimed.

## Verification

There are exactly two verification forms:

- `--verify` hashes source buffers during copy, then rereads destination files
  written by that run, including destination-representable named streams, EAs,
  attributes, and timestamps.
- `bigcp verify SRC DST` reads both entire trees and compares shape, types,
  bytes, all destination-representable named data streams (including streams
  on links), EA blobs, required metadata, directory/root fields, and raw
  reparse buffers. Extras and omissions fail. On FAT/exFAT, WSL, and unknown
  remote filesystems the comparison is explicitly projected: success validates
  every field in that endpoint contract, while warnings/counters describe
  unsupported source features that could not be preserved.

xxh3-128 detects accidental corruption; it is not a cryptographic integrity
mechanism. Verification bypasses no honest drive-internal cache guarantee.
