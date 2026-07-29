# Benchmarks

## 2026-07-29 bounded evidence run (owner-approved, one-time)

**Environment:** Windows 11 Pro 26200 · 64 GB RAM · commit `e647a36` release
build (`bigcp.exe` 2026-07-29) · Defender real-time on (not disabled) ·
source volume D: (NVMe SSDM2E12V 4 TB, NTFS) · destination volume C: (KIOXIA
EG6 NVMe 1 TB, NTFS) · fresh GUID-named sandboxes, deleted after evidence
capture · raw reports and extent counts in `docs/evidence/2026-07-29/`.
Single run per cell (a one-time permission, not the ≥5-repetition median
protocol — treat every number as indicative, not certified). Sources were
freshly generated, so reads were partially or fully cache-served in every
cell including the competitor's; the write path is what these numbers bound.

| Workload | Tool | Result |
|---|---|---|
| W1s: 20,000 × 4 KiB (80 MB), NVMe→NVMe | bigcp (defaults) | 29.66 s = **674 files/s**, exit 0, all verified counters reconciled |
| W1s same | robocopy `/MT:32` | 13.33 s = **1,500 files/s** |
| W2 (scaled): 2 × 8 GiB (16 GiB), NVMe→NVMe | bigcp (defaults, buffered, hashing always on) | **1,109 MB/s average, 1,180 MB/s peak**, exit 0 |
| W2 single 8 GiB | robocopy `/J /MT:8` (unbuffered, no hashing, no temp+rename) | 3.28 s = **~2,497 MB/s** |
| Fragmentation | `bigcp-testkit extents` | **2 × 8 GiB: one extent per file (zero fragmentation)**; 20,000 small files: all single-extent |

**Honest findings, recorded verbatim:**

1. The aspirational ≥3× small-file KPI is **not met**: bigcp measured ~0.45×
   robocopy `/MT:32` on this cell. Follow-up investigation (same day, same
   fixture): worker-count sweeps and single-threaded floors localized the
   gap into two parts. (a) The coordinator's inline per-file probes cost
   ~0.5 ms/file of serial time; they were moved into the workers with a
   promote-back sentinel for hidden huge-ADS files (rsync's pipelining
   lesson: the discovering stage must never do per-item blocking work),
   improving the single-thread floor from 57.9 s to 47.9 s with all 72
   tests green. (b) The dominant remainder is **structural**: per file,
   bigcp's atomic-publication protocol is ~1.6× robocopy's direct write
   single-threaded (2.40 vs 1.50 ms/file), and parallel throughput plateaus
   at almost exactly 2× robocopy's best time (24–28 s vs 12.2 s) —
   consistent with every bigcp file paying two Defender filter evaluations
   (temp-name create + rename-to-final rescan) where a direct write pays
   one. This is the measured price of the crash-safety contract (I5), now
   restated in PLAN §8.7 (H2 falsified as originally worded). One
   benchmark-gated mitigation is registered: temp creation with
   `FILE_FLAG_DELETE_ON_CLOSE` to shed one filter-visible syscall per file.
   Run-to-run noise on this workload is ±20-30 % (Defender), so the
   single-run numbers above bound, not certify.
2. Buffered streaming reached ~44 % of unbuffered robocopy `/J` on the
   NVMe→NVMe large-file cell (1.1 vs ~2.5 GB/s). This meets ADR 0028's
   stated reopening condition **for the fastest internal-to-internal class**;
   whether that cell justifies unbuffered I/O's return is an owner decision —
   the primary USB-C external scenario is device-bound at a fraction of
   either figure, and the comparison tool does no hashing or temp+rename.
3. The anti-fragmentation stance is proven on this run: preallocated 8 GiB
   copies landed as exactly one extent each.

**External-drive cell (H:, Oyen Novus 18 TB USB HDD): aborted before any
write.** The first metadata operation (creating the new evidence directory)
failed with Win32 error 23 `Data error (cyclic redundancy check)` — a
hardware-level integrity failure. Zero bytes were written, nothing was
created, and all H: activity was halted per the owner's no-harm instruction.
Management status reads Healthy/OK, which does not clear a CRC on a fresh
metadata write; the drive, cable, or USB bridge needs owner investigation
before any evidence run touches it.

Write budget actually spent: ~16.2 GiB on D: (fixtures, deleted after),
~24.3 GiB on C: (destinations + robocopy references, deleted after),
**0 bytes on H:** — negligible against either SSD's rated endurance.

## 2026-07-29 redesign result (ADR 0030) — same environment and fixture shape as above

After the owner rescoped the reliability contract (completed-run truth +
rerun-repair; VISION amended) small files switched to direct final-name
writes with timestamps stamped strictly last:

| Workload | Tool | Result |
|---|---|---|
| W1s 20,000 × 4 KiB, NVMe→NVMe, two interleaved reps | bigcp defaults (ADR 0030) | 16.37 s / **13.62 s** (≈1,225–1,470 files/s) |
| same | robocopy `/MT:32` | 11.18 s / 11.85 s (≈1,690–1,790 files/s) |
| 200 × 8 MiB files | bigcp via temp path (old 4 MiB threshold) | 1.91 s = 878 MB/s |
| same | bigcp via direct path (`large-threshold=32MiB`) | **1.03 s = 1,625 MB/s** |

Findings: the redesign moved bigcp from ~0.45× to **~0.8–0.87× robocopy** on
the small-file cell; the ≥1× default-settings release gate (PLAN §8.7) is not
yet met and the remaining ~15–20 % gap is the open item (candidates: source
open-time revalidation cost, worker/coordinator channel overhead — each to be
profiled before any further change). The 8 MiB boundary measurement moved the
large-threshold default from the 4 MiB citation to a measured 16 MiB
(whole-file worker buffering ≤1 GiB transient), and the registered follow-up
decouples buffering from destination strategy so streaming also writes direct
below the checkpoint threshold. Worker-count sweep (16/32/64) was flat within
noise — the automatic default stands.

## Outstanding

The elevated ReFS matrix and the repeated-run certified benchmark protocol
below remain unexecuted. Endurance, million-entry, and competitor sweeps
stay prohibited (VISION).

Future entries must record OS build, CPU/RAM, source/destination volume and

Future entries must record OS build, CPU/RAM, source/destination volume and
filesystem, controller/transport, device policy, workload, warmup, repetitions,
logical and physical bytes written, average/variance, observed ceiling, command
line, commit, and thermal/AV context. Each entry must also record the
destination fragmentation evidence produced by `bigcp-testkit extents`
(files, total/max physical extents, fragmented-file count — read-only
measurement of test-owned trees): parallel copies claiming performance must
show they did not fragment large destination files, since preallocation is
the designed counter-measure (DESIGN.md) and a regression there would
otherwise be invisible until HDD read-back slowed. All benchmark workloads are bounded
(low-GB budgets); endurance/TB-class measurement is prohibited outright by
VISION — there is no approval path — so million-file and TB-class figures, when
stated, are extrapolations from bounded results plus design analysis and must
be labeled as such.
