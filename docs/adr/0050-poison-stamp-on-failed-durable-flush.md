# ADR 0050: Poison-stamp detectability on failed durable flush

**Status:** Accepted (sentinel corrected 2026-08-02 — see the bracketed
amendment in Decision)

## Context

The skip heuristic is unnamed-stream size plus last-write time (ADR 0006).
With `--flush`, the durable flush runs after data and metadata already match
the source, so a flush failure left a destination file the next run would
classify `Same` and skip forever: the file was reported failed once, but the
user's durability request was silently dropped on every rerun. That violates
the product promise that a rerun detects and repairs everything unfinished.

## Decision

When a requested per-file flush fails — in `DestinationFinal` completion and
in `DestinationTemp` post-publication finish — best-effort restamp the
destination's last-write time through the same handle to a sentinel
`FILETIME` before returning the error. The file is still reported failed; the
poison stamp only guarantees the rerun's size/mtime heuristic sees a mismatch
and replaces the file, retrying the flush.

[Amended 2026-08-02: the original tick-1 sentinel (1601-01-01) was rejected
by FastFAT, whose timestamp range starts at 1980 and whose `FatSetBasicInfo`
fails out-of-range values rather than clamping — making the poison a silent
no-op exactly where write-through media make it matter most. The sentinel is
now 1980-01-02 00:00 UTC, inside every supported filesystem's representable
range and still decades away from any real source mtime under every
quantization window. Also admitted here: the stamp travels through the shared
basic-info setter, whose zero attribute mask maps to `FILE_ATTRIBUTE_NORMAL`,
so a successful poison additionally clears the failed file's attributes —
acceptable and detectability-positive, since the rerun fully replaces the
file.]

The poison is best-effort by design: if the device is gone, the restamp fails
with the flush, and recovery remains the documented reconnect, rerun, and
standalone-verify path. No new option, prompt, extra I/O on the success path,
or change to the skip heuristic itself is introduced.

## Consequences

A failed durability request now converges under the ordinary rerun contract
instead of being permanently absorbed by the skip heuristic. The sentinel
timestamp is visible on the failed destination file until the rerun replaces
it; that is intentional — detectability is the point. Runs without `--flush`
are unaffected.

## Validation

Forcing `FlushFileBuffers` to fail requires fault injection, which the
standing test prohibitions reserve for the deterministic fault-simulation
release gate; until that gate runs, the poison call sites are code-reviewed
rather than routinely gated. The behavior they rely on — any size/mtime
mismatch makes the rerun replace the file — is already pinned by the
existing classification and rerun suites.
