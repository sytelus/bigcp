# ADR 0022: Use temp publication for every file

**Status:** Superseded by ADR 0030

## Context

Direct final-name writes leave small files structurally ambiguous after a crash.

## Decision

All sizes and new/replacement cases use DestinationTemp.

## Consequences

Small-file performance pays a rename for a substantially simpler safety proof.
