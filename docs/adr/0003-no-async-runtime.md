# ADR 0003: No general async runtime

**Status:** Accepted

## Context

The design needs controlled Windows I/O and predictable memory rather than unrelated reactor abstractions.

## Decision

Use scoped threads, bounded crossbeam channels, and direct platform wrappers.

## Consequences

Thread and queue ownership remain visible; cancellation is explicit.
