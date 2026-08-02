# ADR 0002: Two transfer strategies, one semantic path

**Status:** Accepted; shared-finalizer detail superseded by ADR 0030; "engine" terminology superseded by ADR 0043

## Context

Small-file overhead and large-stream throughput need different scheduling.

## Decision

Use a bounded parallel small-file strategy and a bounded streaming strategy; both call the same finalizer.

## Consequences

Performance code cannot invent different copy semantics. The IOCP transport remains pre-1.0 work. [Amendment: the IOCP ring was deleted before adoption (ADR 0027); the shipped large-file path is the buffered sequential loop of ADR 0028.]
