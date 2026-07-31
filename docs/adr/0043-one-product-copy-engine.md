# ADR 0043: One product copy engine with isolated strategies and transports

**Status:** Accepted

## Context

Historical documents used “small-file engine” and “transactional engine” as
shorthand for two completion strategies. The implementation already has one
product-owned semantic entry point, `bigcp_core::engine::copy_file`, and one
result/accounting path. The old vocabulary could be mistaken for multiple copy
engines or for delegation to operating-system copy engines.

## Decision

Describe bigcp as one product copy engine. It selects between:

- a direct, whole-buffer plain-small completion strategy; and
- a transactional temporary-file strategy for auxiliary-data, sparse, large,
  and checkpoint-capable work.

Standard and same-spindle behavior are transport policies beneath those
strategies. Both strategies return `EngineResult`; only the coordinator owns
terminal outcomes, counters, audit, reporting, and verification scheduling.
Operating-system copy engines remain test-harness comparators only.

## Consequences

This is a terminology and architectural-boundary clarification, not a copy
behavior or file-format change. New optimizations must fit behind the strategy
or transport seams without creating an independent semantic/accounting path.
ADR 0002 remains historical evidence for the original two-strategy decision;
this ADR supersedes its “engine” terminology.
