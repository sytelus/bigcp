# ADR 0007: Exclude volume-root OS artifacts

**Status:** Amended by ADR 0047

## Context

Copying live OS artifacts is noisy, permission-heavy, and rarely intended.

## Decision

Exclude the documented root artifact set unless --include-system.

## Consequences

Every exclusion is counted and audited.
