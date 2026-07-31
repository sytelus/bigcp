# ADR 0030: Direct final-name writes for small files

**Status:** Accepted; auxiliary-data detail superseded by ADR 0034

## Context

Benchmarks showed the atomic temp+rename protocol costs ~2× robocopy's
per-file AV-filter work on small-file floods (two filter evaluations per
file vs one), capping bigcp at ~0.45× robocopy. The owner redefined the
reliability bar: the only hard guarantee is that a *completed* run's
reported successes and failures are exactly true; interrupted runs are
recovered by re-running, and partial files at final names in that window
are acceptable. VISION.md was amended accordingly, and it now also states
the throughput goal: meet or exceed robocopy by default, automatically.

## Decision

Plain small files (the unnamed stream below the large threshold, no EAs) write **directly to
their final name** via the new `DestinationFinal` primitive: create (or
truncate-in-place for replacements, which preserves the existing security
descriptor), write the one unnamed stream, and finish. ADR 0031 moved the
timestamp operation to create time; its regression test proves the timestamp
survives writes on the same handle, while a mid-write file's shorter size still
forces repair. ADR 0034 subsequently routed all ADS/EA-bearing files through
temp publication so a logical file's auxiliary data cannot be exposed
partially. Large and auxiliary-data files keep temp+rename: the cost is
amortized for large files, rare for auxiliary data, and checkpointed resume
requires a temp identity to verify.

The large threshold default moves from 4 MiB (citation) to 16 MiB
(measurement: the direct path ran 8 MiB files ~1.85× faster than the temp
path; 16 MiB keeps whole-file worker buffering ≤1 GiB transient). The
principled follow-up — streaming *directly* to the final name for all
non-checkpoint-eligible sizes, removing the buffering/destination-strategy
conflation entirely — is registered as designed future work.

## Consequences

Small-file throughput measured 0.8–0.87× robocopy `/MT:32` after this
change (from 0.45×), with the remaining gap under investigation. Invariants
I3/I5 are rescoped to the transactional and direct paths. Mid-run and
post-interrupt destinations may contain partial **plain** files at final names
— stated plainly in LIMITATIONS.md and the README, with "don't consume the
destination until the run reports success" guidance. Files with auxiliary
data are structurally excluded from that window by ADR 0034.
