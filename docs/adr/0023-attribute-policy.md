# ADR 0023: Copy only user-owned basic attributes

**Status:** Accepted

## Context

TEMPORARY, OFFLINE, recall, pin, no-scrub, and integrity bits are storage/filter policy.

## Decision

Copy READONLY, HIDDEN, SYSTEM, ARCHIVE, and NOT_CONTENT_INDEXED; sparse/EFS use dedicated operations; ReFS integrity follows destination.

## Consequences

Metadata fidelity is explicit and avoids changing HSM or allocation behavior accidentally.
