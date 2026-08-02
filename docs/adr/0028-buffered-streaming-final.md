# ADR 0028: Buffered streaming is the final engine

**Status:** Amended by ADR 0055 (standard-transport unnamed large streams now
overlap one buffered read with one buffered write through the shared
two-buffer pipeline — measurement showed the cache manager alone does not
supply that overlap at distinct-NVMe rates; this ADR's core decision stands:
I/O remains buffered, no unbuffered path or sector-alignment machinery
returns, and the 2026-07-29 reopening finding is dispositioned within
buffered I/O)

## Context

VISION originally listed robocopy's `/J` (unbuffered I/O) among the expressed
defaults, which kept an unbuffered large-file path — most recently a
sequential reader/writer pipeline (ADR 0027) — as mandatory remaining work.
The owner has clarified that `/J` was not intentional: the plan is free to
choose any mechanism that satisfies the performance goals, and VISION.md now
omits `/J`.

## Decision

Adopt the shipped buffered sequential chunk loop as the final 1.0 large-file
engine. The OS cache manager's read-ahead and write-behind supply the
read/write overlap a hand-built pipeline would; queue depth is 1 by
construction. Delete the unbuffered pipeline from the plan, delete the
`--no-unbuffered` flag and `CopyOptions.no_unbuffered`, and delete all
sector-alignment machinery and unbuffered verify read-back with them.

## Consequences

1.0 now requires no further product code — only verification matrices and
evidence. Very large copies pass through the file cache (declared in
LIMITATIONS.md; mildest on the large-RAM machines VISION targets), and
same-run verify read-back may be cache-served (standalone verify later is
the cold-cache form). The bounded §8.7 benchmark evidence arbitrates the
decision: only a measured material shortfall — e.g. sustained dirty-page
throttling stalls — reopens unbuffered I/O, as post-v1 backlog behind the
standing benchmark gate.
