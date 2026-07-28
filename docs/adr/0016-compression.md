# ADR 0016: Do not copy compression state

**Status:** Accepted

## Context

Compression is a storage policy with significant write cost.

## Decision

Copy logical bytes and leave destination compression policy unchanged.

## Consequences

Compressed sources can occupy more destination space and are counted.
