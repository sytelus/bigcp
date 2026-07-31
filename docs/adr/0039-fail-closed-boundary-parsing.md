# ADR 0039: Fail closed at native and sandbox boundaries

**Status:** Accepted

## Context

`bigcp-win` is a safe Rust facade over filesystem and redirector output. A safe
wrapper must not assume that a kernel driver or remote provider returned
ordered records, an in-bounds byte count, or a path-safe stream suffix. It must
also reject embedded NULs before calling NUL-terminated Win32 APIs; otherwise
the kernel can operate on a shorter path than the caller validated and logged.

The testkit has an analogous boundary. On Windows, a path beginning with a root
separator can reset a `PathBuf::push` to the current drive root without carrying
a drive prefix. Rejecting only drive prefixes and `..` therefore did not fully
prove sandbox containment.

## Decision

- Centralize Win32 NUL termination in a fallible helper that rejects embedded
  NUL code units.
- Treat provider byte counts as untrusted: reject responses outside the
  initialized output buffer or shorter than required fixed fields.
- Validate reparse declared lengths, sparse-range ordering/non-overlap, and
  retrieval-pointer word alignment before consuming records.
- Validate stream suffixes before concatenation and refuse separators,
  embedded NULs, and non-data stream types in public stream open/create APIs.
- Reject `RootDir` as well as prefixes and parent traversal in every testkit
  sandbox child.

## Consequences

Malformed provider output and abuse-shaped library input now fail with an
ordinary error instead of being rounded, truncated, accepted out of order, or
interpreted as another path. Normal local and remote hot paths retain the same
syscalls and allocations; the checks operate only on data already returned.

Focused pure parser tests cover malformed lengths, overlapping sparse ranges,
stream traversal, embedded NUL, and rooted sandbox children. Routine tests stay
inside newly created system-temporary sandboxes and perform no drive-level or
remote writes.
