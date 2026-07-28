# ADR 0015: Separate streaming from checkpoint eligibility

**Status:** Accepted

## Context

A streaming transfer is not automatically expensive enough to justify persistent state.

## Decision

Use independent large-stream and checkpoint thresholds, evaluated per stream.

## Consequences

Moderate files stream efficiently without leaving resumable artifacts.
