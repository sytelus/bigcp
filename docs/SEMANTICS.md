# Copy semantics (normative v1 contract)

This document is the single user-facing normative copy contract. `PLAN.md` is
the frozen engineering input; this file describes the implemented v1 behavior.

## Object equality and outcomes

For an ordinary file, `Same` means exact unnamed-stream size and exact
last-write `FILETIME`. Differences only in the copyable attribute mask or
creation time are repaired without rewriting data. Any size or last-write
difference is atomically replaced unless `--replace=false`. Destination-only
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
| File | Unnamed bytes; named `$DATA` streams; EA blob; creation and last-write time; `READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE`, `NOT_CONTENT_INDEXED`. |
| Directory | Existence; named `$DATA` streams; EA blob; the same times and attribute mask, applied post-order; root metadata included. |
| Symlink/junction | Link target/reparse payload and tag; named `$DATA` streams; EA blob; copied basic metadata. |

Last-access time is written best-effort but informational in verification
because reads legitimately change it. Sparse allocation is preserved when
supported and enabled; logical content remains the correctness criterion.
Compression and filesystem/HSM-managed attributes follow destination policy.

## Completion and replacement

Every file, including a new small file, follows one protocol:

1. Open the source read-only and validate its enumeration snapshot.
2. Create a unique `.bigcp-<full-run-id>-<nonce>.part` sibling with
   delete-on-close armed.
3. Copy data, named streams, EAs, sparse layout, and destination DACL policy.
4. Revalidate the source. For replacement or in-place metadata repair,
   revalidate destination identity, kind, size, last-write time, attributes,
   and reparse tag, then preserve a protected destination DACL when replacing.
5. Clear delete-on-close, atomically rename to the final name, then set final
   basic metadata (the ordering defeats NTFS name tunneling).
6. With `--flush`, flush after rename and metadata; only then report `copied`.

A crash before publication leaves no final-named partial. Checkpointed partials
may persist under opaque names, but resume never trusts them: current source
size/mtime, source and temporary filesystem identities, and the exact temp
prefix digest must match before continuation. Legacy identity-less checkpoint
records are parseable but intentionally restart from zero.

## Streams, EAs, sparse files, EFS, and reparse points

Only `$DATA` streams are user data; directory `::$INDEX_ALLOCATION` and other
filesystem implementation streams are ignored. A large file ADS promotes its
owner to the journal-aware streaming path and successfully copied file ADS
bytes are included in logical copy accounting. EA transfer uses separate
synchronous buffered handles through `BackupRead`/`BackupWrite`.

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
is given.

## Roots, exclusions, and audit artifacts

Roots are resolved through handles. Equal/nested roots, UNC paths, remote
volumes, and non-NTFS/ReFS filesystems fail preflight. An existing destination
root is pinned. A missing destination is created component-by-component except
in dry-run, where only its nearest existing ancestor is pinned.

At a source volume root, OS artifacts such as `$RECYCLE.BIN`, `System Volume
Information`, and page/hibernation files are excluded unless
`--include-system`. Cloud placeholders hydrate unless `--skip-cloud`.

State, log, and report paths may share a volume with either tree but may not be
inside either tree. JSONL is the complete event audit; the JSON report is an
atomic aggregate. The report is published before the terminal `run_end`, whose
bytes are durably synchronized. If both primary and fallback logging fail, no
new work is claimed.

## Verification

There are exactly two verification forms:

- `--verify` hashes source buffers during copy, then rereads destination files
  written by that run, including named streams and EAs.
- `bigcp verify SRC DST` reads both entire trees and compares shape, types,
  bytes, all named data streams (including streams on links), EA blobs,
  required metadata, directory/root fields, and raw reparse buffers. Extras
  and omissions fail.

xxh3-128 detects accidental corruption; it is not a cryptographic integrity
mechanism. Verification bypasses no honest drive-internal cache guarantee.
