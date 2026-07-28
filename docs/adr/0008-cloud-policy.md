# ADR 0008: Hydrate cloud placeholders by default

**Status:** Accepted

## Context

Raw placeholder reparse data is not a portable file copy.

## Decision

Read normally, which may hydrate; --skip-cloud excludes explicitly.

## Consequences

Network use can occur through the provider and is visible through warnings/counts.
