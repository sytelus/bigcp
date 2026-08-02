# ADR 0038: Publish state artifacts through unique atomic siblings

**Status:** Accepted

**Superseded in part by ADR 0041:** exact-handle publication replaced this
ADR's close-then-path-rename mechanics and path-based cleanup. The unique
atomic-sibling artifact format and compaction policy remain in force.

## Context

The JSON report already used a unique sibling and atomic replacement, while
journal compaction wrote a predictable `journal.compact-tmp`, removed the live
journal, and then renamed the temporary. That could overwrite an unrelated
sibling with the predictable name and exposed a small interval with no journal
path. The journal is only a resume-hint store, but hint-only status does not
justify changing an unrelated file or choosing avoidable non-atomic behavior.

Report and journal publication also duplicated the same create, synchronize,
cleanup, and replacement lifecycle.

## Decision

Use one core artifact helper for reports and compacted journals. It exclusively
creates an unpredictable `.bigcp-<kind>-<uuid>.part` sibling, writes and
synchronizes the bytes, closes the temporary, and calls the narrow
`bigcp-win` atomic-replace wrapper with write-through requested. A failure
removes only the UUID temporary created by that call.

Journal compaction closes its own append handle immediately before publication
because Windows will not replace the path while that handle is open. It then
reopens and seeks the new journal to its append position. Compaction retains
the current job record and every live checkpoint; it remains hygiene rather
than completion authority.

## Consequences

Compaction no longer overwrites a predictable neighboring file and no longer
uses remove-then-rename. Reports and journal compaction now share one tested
publication lifecycle. Serialization remains format-specific, and destination
file publication remains in `bigcp-win`'s handle-owned copy primitives.

The journal format and resume semantics do not change. A focused regression
test preserves a pre-existing `journal.compact-tmp` sentinel, verifies atomic
compaction succeeds, and checks that no UUID journal temporary is stranded.
