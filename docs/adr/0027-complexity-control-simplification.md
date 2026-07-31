# ADR 0027: Complexity-control simplification

**Status:** Accepted; unbuffered-engine decision superseded by ADR 0028

## Context

The plan carried substantial machinery whose payoff was unmeasured or
cosmetic: an IOCP overlapped ring with adversarial completion-order testing,
per-side queue-depth knobs and a bounded runtime governor, a free-space
forecast, Restart Manager lock-owner naming, profiler vendor/hotplug/cache
extras, a deferred-close finalizer pool, a per-device scheduler, decorative
TUI widgets, a persisted verification-report kind, a modeled audit-drain
state, orphan-scan cleanup, and differential-copier release gates. The
owner's direction: when payoff is minimal or questionable, declare a
limitation instead of adding complexity.

## Decision

Delete the items above from the plan (recorded inline at each former
section). The 1.0 large-file design becomes a sequential unbuffered
reader/writer pipeline (double-buffered aligned chunks, queue depth 1 per
side by construction), which preserves robocopy-`/J` semantics while
collapsing the completion-order test burden. In the same pass, build the
small high-value pieces the deletions expose: the device/disk-full circuit
breaker (exit 4), mid-file graceful cancellation, the ETA display, and the
verify error-vs-mismatch distinction.

## Consequences

The tune surface loses `qd-src`/`qd-dst`; the log's profile event loses its
queue-depth fields (pre-1.0 schema change); user-visible consequences are
stated in LIMITATIONS.md. Remaining 1.0 work shrinks to: the pipeline plus
its benchmark, the verification matrices, the elevated ReFS matrix, and the
operator hardware checklist. Deleted items may return only through the
standing benchmark gate, as post-v1 backlog.
