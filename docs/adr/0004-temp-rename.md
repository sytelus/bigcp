# ADR 0004: Uniform temp and atomic publication

**Status:** Accepted

## Context

A crash must never leave final-named partial content or truncate a replacement.

## Decision

Write every file to an opaque sibling and publish atomically after revalidation.

## Consequences

Extra name operations buy structural crash safety and one common correctness proof.
