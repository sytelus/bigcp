# ADR 0006: Exact size and last-write skip

**Status:** Amended by ADR 0035

## Context

Hashing every destination destroys rerun performance.

## Decision

Skip on exact unnamed size and exact last-write FILETIME; repair copied metadata separately.
ADR 0035 retains this rule for NTFS/ReFS and applies only the destination's
documented representable interval to FAT/exFAT.

## Consequences

ADS/EA or same-size/same-time content divergence requires standalone verify.
