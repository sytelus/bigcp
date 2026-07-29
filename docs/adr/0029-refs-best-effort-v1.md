# ADR 0029: ReFS is best-effort at v1

**Status:** Accepted

## Context

VISION includes ReFS in scope. The dedicated ReFS certification matrix
requires an elevated environment with Hyper-V VHDX tooling, which the
implementation environment lacks, and the owner has decided not to gate v1
on it.

## Decision

Ship v1 with ReFS accepted and supported as **best-effort, verified by code
review only**: the capability-flag design, `FileRenameInfoEx` publication,
and degrade-with-warning paths (no EAs on ReFS, version-dependent features)
are reviewed but not matrix-certified. The elevated graceful-VHDX matrix
moves to post-v1; a capability-gated legacy rename fallback is added there
if the probe ever demands it.

## Consequences

LIMITATIONS.md and the README state the asymmetry plainly: NTFS is the
fully verified path, and users wanting maximum assurance on ReFS run
`--verify` plus a standalone `bigcp verify`, which validates the actual
copy regardless of filesystem. No code changes; this is a verification-
claim boundary, not a behavior change.
