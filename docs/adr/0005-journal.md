# ADR 0005: CRC resume-hint journal

**Status:** Accepted

## Context

Large files need restart progress without trusting stale state.

## Decision

Use append-only CRC32C JSONL checkpoints bound to job/source identity; never use the journal for skip.

## Consequences

Torn tails are harmless and every resumed prefix is reread and hashed.
