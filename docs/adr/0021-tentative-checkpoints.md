# ADR 0021: Do not flush each checkpoint

**Status:** Accepted

## Context

Checkpoint flushing can collapse throughput and is not the integrity guarantee.

## Decision

Treat checkpoint state as tentative and verify bytes before use.

## Consequences

--flush applies to completed final files, not intermediate resume progress.
