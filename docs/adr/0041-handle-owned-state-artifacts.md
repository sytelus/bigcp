# ADR 0041: Keep state-artifact temporaries handle-owned through publication

**Status:** Accepted

## Context

ADR 0038 introduced unique sibling publication for reports and compacted
journals, but its first implementation closed the exclusively created
temporary before a path-based replace. Failed-publication cleanup also deleted
the temporary by path after closing it. The random name made accidental reuse
unlikely, but neither operation remained bound to the object that bigcp had
actually created: another process could replace the name in the close/rename
or close/delete interval. The lifecycle also duplicated the stronger
delete-on-close and handle-rename machinery already used by transactional
destination files.

The public temporary constructor additionally accepted an unchecked run ID.
Internal callers use UUIDs or fixed labels, but a library boundary must not
allow a separator, parent component, or alternate-stream colon to influence a
supposed sibling path.

## Decision

This ADR supersedes ADR 0038's close-then-path-rename publication mechanics,
not its artifact format or compaction policy. Reports and compacted journals
now use `DestinationTemp`, the common exact-handle ownership primitive:

1. validate the run/kind identifier as nonempty ASCII letters, digits, or
   hyphens and construct one full-UUID sibling component;
2. create it exclusively with delete access, deny delete sharing, and arm
   delete-on-close on the creating handle;
3. write and synchronize through that handle;
4. clear the disposition, replace the final sibling by handle, synchronize the
   published handle, and close it before reporting success.

Failure before publication closes the same delete-pending handle. There is no
path-delete fallback. A rename failure re-arms deletion on that handle where
possible; failure to re-arm may leave only the opaque temporary rather than
risk deleting an object by name. Payload and reparse temporary names use the
same validated candidate-name helper, while their distinct publication rules
remain isolated.

## Consequences

Report and journal formats, paths, retention, and copy hot-path I/O are
unchanged. State publication can no longer be redirected by replacing a
closed temporary name, and cleanup cannot delete a later occupant of that
name. The core loses its duplicate UUID/open/drop implementation and the
path-based `MoveFileExW` wrapper is removed from the safe Win32 surface.

New bounded system-temporary tests prove that an artifact name cannot be
removed while its owner handle is live, drop removes only the owned temporary,
replacement publishes the requested bytes without residue, and unsafe run IDs
are rejected before filesystem access.
