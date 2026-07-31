# ADR 0048: Use relative parent handles for distinct-drive NTFS small files

**Status:** Accepted

## Context

The bounded 2026-07-29 local small-file measurements left destination create
as the largest worker phase after directory affinity, deep queues,
create-time metadata stamping, and write-only destination handles had landed.
The candidate register identified `NtCreateFile` with
`OBJECT_ATTRIBUTES.RootDirectory` as the next narrow optimization: an NTFS
worker copying many children of one directory otherwise asks Windows to parse
and resolve the same absolute parent path for every final-name open.

VISION requires reliability first, one product copy engine, and harmless tests.
It also prohibits drive-scale or endurance testing in routine development.
There was no approved pair of disposable physical NTFS drives for this change,
so the mechanism can be implemented and correctness-tested, but its speedup
cannot yet be claimed.

## Decision

- Enable relative destination opens only when both endpoints are local NTFS,
  the static transport is `standard`, source and destination physical extents
  do not intersect, and the file is already eligible for the directory-affine
  plain-small worker path.
- Give each worker a one-entry destination-parent cache. On the first job for a
  directory, open that directory without following a final reparse point,
  deny delete sharing to pin its namespace, and verify the captured
  enumeration identity and directory kind. All following jobs in that
  directory reuse the owned handle.
- Open the final child with `NtCreateFile`, the verified directory handle as
  `RootDirectory`, and only the final UTF-16 component. Reject empty, rooted,
  multi-component, embedded-NUL, separator, and alternate-stream (`:`) names
  before entering the native call.
- Preserve the existing completion semantics exactly: New files use native
  `FILE_CREATE` (the `CREATE_NEW` contract); replacements use `FILE_OPEN`,
  validate identity/kind/size/mtime/attributes/reparse tag on that same opened
  handle, and only then truncate; final reparse points are never followed;
  EFS-at-create, create-time metadata, one whole-buffer write, optional flush,
  and close-before-success remain shared with the absolute-path constructor.
- Treat the capability as an optimization. If a verified parent handle cannot
  be opened, cache that result for the directory and use the prior absolute
  final-path open. This preserves the established behavior and avoids adding a
  new failure mode or retry loop.
- Keep source opens and stream discovery unchanged. Keep inline,
  transactional, sparse, large, same-spindle, ReFS, FAT/exFAT, UNC, and WSL
  paths on their existing constructors and transports.

## Consequences

The common distinct-drive local NTFS small-file path resolves and validates one
destination parent per worker/directory instead of resolving the full parent
for every child create. The optimization remains modular: core selects and
caches a safe capability, while all native layout, lifetime, status conversion,
and handle ownership stay inside `bigcp-win`.

The one cached handle is bounded and is replaced when that worker advances to
another parent. No new queue, buffer, thread, option, prompt, dependency, copy
engine, or filesystem guarantee is introduced. Other transports do not carry
the parent capability and cannot enter the native relative-open branch.

Performance remains a hypothesis until the approved multi-drive NTFS protocol
records a quiesced, rotated, repeated comparison against the absolute-open
baseline and robocopy. A result that is neutral or negative can remove this
selection without touching completion semantics.

## Validation

Bounded temporary-directory tests prove relative New creation, exact bytes,
create-time metadata, collision refusal without truncation, stale parent
identity refusal, and rejection of traversal/stream/NUL names. A pure selection
matrix proves that same-physical-disk, non-NTFS, UNC, WSL, and redirector cases
cannot activate the path. The complete confined suite guards existing direct
and transactional behavior. No physical-drive performance, stress, endurance,
large-scale, forced-disconnect, or machine-stability test is part of this
decision.
