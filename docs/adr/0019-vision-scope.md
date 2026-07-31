# ADR 0019: Apply the VISION simplification

**Status:** Accepted; local-only clause superseded by ADR 0037

## Context

Broad engine, platform, privilege, and tuning surfaces dilute reliability.

## Decision

Keep local-only, one product semantic path, static profiles, one hash, exactly two verification forms, no SFVD, no backup privilege, and abort/rerun recovery.

ADR 0037 later replaces only the local-only clause with an isolated UNC/WSL
endpoint policy. The single engine, static-profile, verification, privilege,
and recovery decisions remain in force.

## Consequences

Future scope additions require explicit backend capabilities rather than conditionals through core.
