# ADR 0018: Exact-root machine lock

**Status:** Accepted

## Context

A lock must stop accidental duplicate writers without inventing global nesting policy.

## Decision

Hash the resolved source/destination pair into a global mutex for the exact destination root.

## Consequences

Nested different roots remain under the documented exclusivity assumption.
