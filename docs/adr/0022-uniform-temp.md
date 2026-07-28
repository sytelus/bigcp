# ADR 0022: Use temp publication for every file

**Status:** Accepted

## Context

Direct final-name writes leave small files structurally ambiguous after a crash.

## Decision

All sizes and new/replacement cases use DestinationTemp.

## Consequences

Small-file performance pays a rename for a substantially simpler safety proof.
