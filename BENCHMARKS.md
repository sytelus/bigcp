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

## 2026-07-29 systematic gap analysis (phase instrumentation)

`--analyze` runs now emit a `phase_timing` log line: per-phase worker-time
totals from process-wide atomic accumulators in the engine. First capture
(20,000 × 4 KiB, 64 workers, ~18 s wall ≈ 70 worker-seconds):

| Phase | Total | Mean/file |
|---|---|---|
| create_dst | **50.2 s (72 %)** | **2,509 µs** |
| set_meta | 8.3 s | 413 µs |
| write | 4.2 s | 211 µs |
| open_src | 4.1 s | 206 µs |
| list_streams | 1.9 s | 93 µs |
| read | 1.3 s | 66 µs |

**Destination creation is the entire remaining story.** A/B test of the
leading hypothesis — that requesting `GENERIC_READ` on the create routed
files through the AV pre-read scan path — *falsified it*: write-only access
left create_dst at 2,479 µs (the write-only handle is kept anyway — least
access). With a single worker the whole per-file cost is ~2.4 ms, so the
64-worker create mean of ~2.5 ms means the filter largely serializes
concurrent creates. The same session showed robocopy `/MT:32` itself
swinging 11.2 → 17.0 s across hours (±40 % environmental drift — Defender
state), so single-run ratios are unreliable; the ≥1× gate must be judged by
the median-of-≥5 protocol below.

Ranked remaining levers (none is a code loop to optimize): (1) **binary
signing** — Defender treats unknown unsigned executables to heavier
synchronous evaluation than signed ones (robocopy is Microsoft-signed);
this is a release/packaging action and the top candidate for the residual
create cost; (2) measuring the certified gate on a quiet machine with the
repetition protocol; (3) user-side environment guidance already in the
Hints tab (Dev Drive / temporary Defender exclusion — bigcp never changes
AV settings itself). Filter-visible operations per file are already at the
direct-copy minimum (one create, one close, no rename).

## 2026-07-29 goal met: directory-affine scheduling (final small-file result)

With the owner-approved Defender process exclusion active (the installer
mechanism) and default settings, after four measured, layered changes —
phase instrumentation → direct final-name writes → probe-free coordinator →
directory-affine sharding + deep queues + yielding directory exits:

| Run | bigcp (defaults) | robocopy `/MT:32` |
|---|---|---|
| 1 | **10.24 s** | 15.11 s |
| 2 | **11.98 s** | 16.04 s |

**bigcp 1.3–1.5× faster than robocopy** on the 20,000 × 4 KiB NVMe→NVMe
workload — the meet-or-exceed gate satisfied on this cell in this session.
The causal chain, each step confirmed by the phase table: destination
creation was 72 % of worker time; the residue after the exclusion was NTFS
directory-index convoying (2 ms interleaved vs ~0.5 ms serialized); affinity
alone stalled the coordinator (4-deep queues, 25 s); deep queues alone still
ran one directory at a time (the DFS stack places each leaf's exit atop its
enter, 23 s); yielding exits unlocked cross-directory parallelism (10–12 s).
Negative results are retained above deliberately — they are the regression
map. Certified numbers still require the median-of-≥5 quiet-machine
protocol.

## 2026-07-29 external-drive evidence (H: repaired — Oyen Novus 18 TB USB HDD)

The owner repaired H: (previous CRC failures gone: directory create and
write probes clean). Bounded runs, D: NVMe source → H: destination, new
GUID directory only, all fixtures deleted after; raw reports in
`docs/evidence/2026-07-29/hdd-*.report.json`:

| Workload | bigcp (defaults unless noted) | robocopy |
|---|---|---|
| 2 × 8 GiB, `--verify` | **224.8 MB/s avg, 255.7 MB/s peak**, exit 0, verify 2/2 passed | `/J`, 1 × 8 GiB: 241 MB/s |
| 20,000 × 4 KiB | 31.5 s (8 workers) / **28.4 s** (threads=4) / 33.2 s (threads=2) | `/MT:32`: 23.8 s |

**Large files (the primary scenario): parity within ~7 %** — both tools sit
at the device ceiling (~250 MB/s class), confirming ADR 0028's expectation
that buffered streaming is not the limiter on external drives.

**Small files to USB HDD: 0.84× robocopy at best**, and the phase table
names the bottleneck precisely: `set_meta` (the final timestamp stamp) costs
**2.0–2.7 ms per file at every concurrency level** — a synchronous device
round-trip under the Quick-removal (write-cache-off) USB policy, ~40–53
worker-seconds of pure metadata I/O. Worker-count sweep was flat-to-inverse,
so this is device-bound, not contention; the NVMe directory-affinity design
neither helps nor hurts here. Registered levers: (a) investigate what
robocopy's ~1.2 ms/file total does differently on this stack (likely fewer
filter/metadata round-trips — needs ProcMon-level tracing, post-v1);
(b) users copying many small files to Quick-removal drives can switch the
drive to "Better performance" policy (documented trade-off in README's
removal section). Timestamp fidelity is contract — skipping the stamp is
not a lever.

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
