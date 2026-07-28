# ADR 0002: Two transfer strategies, one semantic path

**Status:** Accepted

## Context

Small-file overhead and large-stream throughput need different scheduling.

## Decision

Use a bounded parallel small-file strategy and a bounded streaming strategy; both call the same finalizer.

## Consequences

Performance code cannot invent different copy semantics. The IOCP transport remains pre-1.0 work.
