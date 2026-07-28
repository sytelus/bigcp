# ADR 0019: Apply the VISION simplification

**Status:** Accepted

## Context

Broad engine, platform, privilege, and tuning surfaces dilute reliability.

## Decision

Keep local-only, one product semantic path, static profiles, one hash, exactly two verification forms, no SFVD, no backup privilege, and abort/rerun recovery.

## Consequences

Future scope additions require explicit backend capabilities rather than conditionals through core.
