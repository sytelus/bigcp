# ADR 0034: Publish auxiliary-data files transactionally

**Status:** Accepted

## Context

ADR 0030 made small files direct final-name writes for measured throughput.
That recovery argument is structural for a plain file: an interrupted
whole-buffer write is shorter than the source, while a full write has already
completed the only data stream. It is not structural for a file with named
streams or EAs. The ordinary rerun heuristic intentionally does not enumerate
streams on otherwise matching files, so a kill after the unnamed stream but
during auxiliary work could leave a state that only standalone verification
would expose. A delayed timestamp made that outcome unlikely to classify as
Same, but correctness must not depend on timestamp coincidence.

## Decision

Keep direct final-name writes only for plain, non-sparse small files with one
unnamed stream and no EAs. Route every file with a named stream or EA through
`DestinationTemp`, regardless of its size, and publish the completed logical
file with the same revalidation, protected-DACL preservation, and atomic rename
used by the large path. Sparse files already use that transactional path.

`DestinationFinal` therefore has one job: identity-check a plain replacement,
truncate it, perform one whole-buffer unnamed-stream write, and finish. The
direct named-stream/EA helpers and late-stamp branch are removed.

## Consequences

The uncommon ADS/EA case pays the extra create-and-rename cost that ordinary
small-file floods avoid. In return, a process kill cannot expose a partially
updated logical stream/EA set under the final name, the rerun contract no longer
leans on a timestamp marker, and the direct writer has a smaller auditable
surface. This is a pre-1.0 behavioral hardening with no command-line or schema
change.
