# ADR 0025: Consolidate tuning

**Status:** Accepted; non-functional stream fields removed by ADR 0033

## Context

Independent low-level flags create invalid, irreproducible combinations.

## Decision

Expose static class profiles plus one validated comma-separated --tune escape hatch.

## Consequences

Reports can reproduce initial settings; pre-1.0 synchronous streaming does not yet enact QD controls.
