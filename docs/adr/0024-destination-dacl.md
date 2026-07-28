# ADR 0024: Preserve protected destination DACLs

**Status:** Accepted

## Context

Replacing with a new object can silently erase explicit destination protection even though source ACL copy is out of scope.

## Decision

Capture and apply an explicitly protected destination DACL to the temp; fail replacement if preservation fails.

## Consequences

Owner/SACL are not copied and ordinary inherited security remains cheap.
