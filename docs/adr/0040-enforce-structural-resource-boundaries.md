# ADR 0040: Enforce structural and resource boundaries exactly

**Status:** Accepted

## Context

The fail-closed parsing decision in ADR 0039 covered byte lengths and unsafe
native inputs, but a second review found higher-level boundaries that also
needed exact enforcement. A directory provider could return a syntactically
rooted or multi-component child name, or claim that a name extended into the
next enumeration record. Independently configured log and report paths could
resolve to each other, the state directory, or its checkpoint journal. The
`mem` override capped individual buffers but did not reserve the coordinator
chunk that can coexist with standard-path worker buffers. Journal replay also
retained every historical line merely to identify the final one.

These cases were uncommon, but accepting any of them made a safe wrapper,
artifact role, or resource budget weaker than its public description.

## Decision

- Validate each native directory record against its own `NextEntryOffset`, and
  accept only one literal, non-NUL child component before joining it to the
  enumerated directory.
- Resolve audit paths through their nearest existing ancestors and reject
  collisions among log, report, state-directory, and journal roles. Log and
  report may remain ordinary children of the state directory, but neither may
  be its ancestor or container.
- On standard transport, reserve one coordinator chunk from `mem` before
  deriving the threshold-sized worker count. Same-spindle transport retains a
  single direct burst cap because coordinator work drains the phased worker
  before starting.
- Replay journals with one-record lookahead, preserving the existing torn-tail
  versus invalid-interior rules without retaining the entire history.
- Reject empty top-level input paths and provider/API result lengths that
  exceed the buffers established by their sizing calls.

## Consequences

Malformed provider output and conflicting artifact configuration now fail
before traversal or copying. An explicitly small standard-path memory budget
may be rejected or select fewer workers because it now represents the maximum
simultaneously live copy buffers it claims to bound. Default profiles and the
same-spindle fast path are unchanged.

Normal directory enumeration adds only constant-time checks over fields and
names already returned. Journal replay memory is independent of record count.
Focused pure tests cover record-local framing, child components, audit-role
collisions, buffer sizing, embedded NUL rename input, memory reservation, and
plain-summary accounting without physical-drive or remote writes.
