# ADR 0013: Commit revalidation and run lock

**Status:** Accepted

## Context

Atomic rename alone cannot prove that the classified target is still the same object.

## Decision

Use CREATE_NEW temps, exact-root machine mutex, and identity/size/mtime revalidation immediately before replacement.

## Consequences

A documented micro-race remains under the exclusive-destination assumption.
