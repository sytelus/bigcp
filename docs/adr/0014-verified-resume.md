# ADR 0014: Verified tentative checkpoints

**Status:** Accepted

## Context

Durable flushes at each watermark are expensive and hardware may still lie.

## Decision

Record tentative exact-prefix digests and always reread/verify before continuing.

## Consequences

Process kills resume near the watermark; power loss may safely restart from zero.
