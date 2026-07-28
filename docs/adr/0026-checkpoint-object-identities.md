# ADR 0026: Bind checkpoints to source and temporary identities

**Status:** Accepted

## Context

A CRC-valid checkpoint previously named its opaque temporary and described the
source by size and last-write time. Those facts detect torn records and common
source changes, but a stale name can later be reused by an unrelated object,
and a replacement source can coincidentally retain the same size and timestamp.
Neither case grants bigcp ownership of the named object or permission to splice
an older prefix onto a new source.

## Decision

Each new checkpoint records the volume serial and 128-bit filesystem file ID
of both the source file and the opened temporary. Resume opens the final temp
component with `FILE_FLAG_OPEN_REPARSE_POINT`, requires an ordinary file, and
compares both identities before reading, truncating, writing, or deleting it.
The existing source size/mtime and exact-prefix digest checks still apply.

The identity fields are optional in journal JSON only for additive parsing of
older version-one records. A checkpoint missing either identity is a cache
miss and is never resumed. Unsupported journal versions fail closed without
truncating the journal. CRC remains corruption detection, not authentication.

Path-based fallback deletion after an owned temp handle closes is prohibited;
delete-on-close is attached to the exact handle so a replacement name cannot
be removed accidentally.

## Consequences

Old partials safely restart and may remain as reported opaque orphans until the
planned ownership-proven cleanup exists. New checkpoints add small fixed JSON
fields and one metadata query at checkpoint creation. Resume becomes stricter
without changing completed-file classification, publication, or the frozen
plan's prefix-verification integrity guarantee.
