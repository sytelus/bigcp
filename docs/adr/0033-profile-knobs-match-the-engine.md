# ADR 0033: Keep profile knobs aligned with the engine

**Status:** Accepted

## Context

The shipped large-file engine is one synchronous coordinator-owned chunk loop,
and directory enumeration is iterative on that coordinator. Nevertheless,
profiles, reports, audit events, and `--tune` still exposed concurrent-stream
and enumeration-thread values inherited from deleted scheduler designs. Those
values changed no behavior, so they were misleading configuration rather than
useful compatibility surface.

## Decision

Remove the `streams` tune key and the unused stream/enumeration-thread profile
fields. Keep only settings that directly affect execution: chunk size,
small-file workers, memory budget, and the large/checkpoint thresholds. The
large-file path remains explicitly single-stream unless a future measured
design implements real parallelism end to end.

## Consequences

Pre-1.0 callers passing `--tune streams=...` now receive an unknown-key
configuration error instead of a false promise. New reports and profile audit
events omit the unused fields. Parsers continue accepting older report fields
because Serde ignores unknown object properties and the JSON schemas allow
additional properties.
