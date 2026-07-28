# ADR 0011: Query-only profiling

**Status:** Accepted

## Context

Synthetic writes can harm or contaminate real volumes.

## Decision

Derive profiles only from official query APIs and observed copy traffic.

## Consequences

Profiles may be conservative when bridges misreport facts.
