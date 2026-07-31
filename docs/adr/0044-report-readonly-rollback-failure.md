# ADR 0044: Report failed READONLY metadata rollback

**Status:** Accepted

## Context

A direct replacement may initially receive access denied because the existing
destination is READONLY. When classification proved that exact object carried
the flag, bigcp clears it through an identity-checked handle and retries the
open. If the retry failed, the engine attempted to restore the original basic
metadata but discarded any restoration error. A concurrent replacement or a
second permission failure could therefore leave READONLY cleared while the
report mentioned only the create failure.

## Decision

Continue restoring only through `set_basic_at_checked`, bound to the
enumerated destination identity. If both the create retry and restoration
fail, report `restore_dst_metadata` as the primary operation, preserve its raw
error/category, and retain both failures in the message. An identity mismatch
is classified as `destination_changed`.

Do not attempt an unchecked path-based fallback: after identity proof fails,
bigcp has no authority to mutate the current path occupant.

## Consequences

The report now exposes the possible metadata side effect and gives the
operator an actionable destination-writer hint. The focused unit test pins the
dual-error context and destination-change classification. Successful rollback
and the ordinary direct-copy hot path are unchanged.
