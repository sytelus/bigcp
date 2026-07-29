# ADR 0031: Create-time timestamp stamping and destination-led worker counts

**Status:** Accepted

## Context

Phase instrumentation on a write-through (Quick-removal) USB HDD showed two
dominant per-file costs the finish-time-stamp design could not fix: the
metadata stamp (~2 ms when issued as a separate operation) and, once that
was coalesced, `CloseHandle` itself (~2.3 ms — the timer around `finish`
had been measuring the close all along). Separately, the `min(src, dst)`
worker composition let one low-confidence Unknown-profiled side drag a
32-worker HDD destination down to 4 workers.

## Decision

1. `DestinationFinal::create` stamps source timestamps and attributes
   immediately at create: Windows freezes automatic last-write updates on a
   handle once times are explicitly set (pinned by the
   `create_time_stamp_survives_writes_in_sandbox` regression test), so the
   stamp coalesces into the create's MFT window. Files carrying ADS or EAs
   stamp at finish — the freeze is per-handle and sub-opened stream/EA
   handles would re-bump it. Crash repair rides the size check:
   truncate-at-create makes every interrupted partial shorter than its
   source (I5 wording updated).
2. HDD-destination small-file workers: 8 → 32, by measurement (31.5 s →
   19.1 s on the bounded workload) — many outstanding closes overlap in the
   device queue.
3. Worker composition follows the destination row (small-file work is
   destination-bound) unless the source is seek-penalty class.

## Consequences

Defaults beat robocopy on every measured small-file cell (external HDD:
17.7–18.0 s vs 23.1–24.5 s). Registered follow-ups: a dedicated
close-finalizer stage (the deleted H1, now benchmark-justified) and the
D:-NVMe Unknown-profiling defect. Reverting any part of this ADR requires
re-running the BENCHMARKS.md workloads that justified it.
