# ADR 0006: Exact size and last-write skip

**Status:** Accepted

## Context

Hashing every destination destroys rerun performance.

## Decision

Skip on exact unnamed size and exact last-write FILETIME; repair copied metadata separately.

## Consequences

ADS/EA or same-size/same-time content divergence requires standalone verify.
