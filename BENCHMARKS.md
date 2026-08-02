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

## 2026-07-29 first-principles pass: goal exceeded on the external HDD too

Three findings, each measured on H: (Oyen Novus USB HDD) with the phase
instrumentation, took the small-file cell from 0.84× to **1.3× robocopy
with pure default settings (17.7–18.0 s vs 23.1–24.5 s, two interleaved
reps)**:

1. **Create-time timestamp stamping** (`DestinationFinal::create` now
   stamps immediately after create): Windows freezes automatic last-write
   updates on a handle once times are explicitly set — validated by a new
   regression test — so the stamp coalesces into the create's MFT window.
   Crash repair rides the size check (files truncate at create). Files with
   ADS/EAs stamp at finish (the freeze is per-handle; sub-opened stream/EA
   handles would re-bump it — caught by e2e verify during development).
2. **The dominant per-file cost on write-through USB is `CloseHandle`
   (~2.3 ms)**, not the stamp — the timer around `finish` had been
   measuring the close all along. More outstanding closes overlap in the
   device queue, so the HDD-destination worker row was raised 8 → 32
   (measured 31.5 s → 19.1 s). A dedicated close/finalizer stage (the
   deleted H1, now benchmark-justified) remains the registered next lever.
3. **Worker composition bug**: `min(src, dst)` workers let a low-confidence
   (Unknown) side drag an HDD destination to 4 workers. Composition now
   follows the destination row (destination-bound work) unless the source
   is itself seek-penalty class. Also observed: D: (internal NVMe) profiles
   as Unknown — device-query failure on that controller, registered for
   investigation.

Scoreboard after this pass, defaults vs robocopy best-known: NVMe→NVMe
small files **1.3–1.5×**, NVMe→USB-HDD small files **1.3×**, NVMe→USB-HDD
large files **parity at device ceiling**, NVMe→NVMe large files parity
(buffered default vs buffered default). All cells single-session
indicative; certified median-of-5 protocol still pending.

## 2026-07-29 bandwidth analysis: what "device-bound" means on the USB HDD

From the H: evidence already gathered (no new runs):

- **Sequential bandwidth ceiling of H: ≈ 240–256 MB/s** (bigcp large-file
  peak 255.7 MB/s, robocopy `/J` 241 MB/s, bigcp average 224.8 MB/s across
  file transitions). Both tools sit at this ceiling for large files.
- **Small files do not touch that ceiling — at all.** Best small-file run:
  80 MB in 17.7 s = **4.6 MB/s ≈ 1.9 % of the sequential bandwidth**
  (robocopy: 3.4 MB/s ≈ 1.4 %). Moving the 80 MB of data at ceiling speed
  would take 0.33 s — **over 98 % of small-file wall time is per-file
  overhead**, chiefly the create and close metadata round-trips that
  write-through (Quick-removal) USB volumes force to the device
  (~0.9 ms/file effective at ~1,130 files/s across 32 overlapped workers).
- **The small-vs-large throughput gap is therefore ~49×** (4.6 vs
  224.8 MB/s), and it is a property of the destination's metadata-latency
  regime, not of either tool's data path: robocopy lives in the same regime,
  ~25 % behind bigcp. Further gains here need fewer device round-trips per
  file — the user-side "Better performance" drive policy is the big lever
  (documented in the README removal section); tool-side scheduling is
  already saturated, as the R7 experiment below confirms.

## 2026-07-29 R6/R7 dispositions

- **R6 fixed:** both internal NVMe drives were misclassified as *Unknown*
  because this board's Intel VMD/RST controller reports `BusTypeRAID`.
  `detected_class` now trusts a positive "no seek penalty" answer as
  definitively solid-state (→ moderate SATA-SSD profile) even when the bus
  is unrecognized; live profile events confirm `SataSsd/SataSsd, workers 32`
  where `Unknown/Unknown, workers 4` appeared before. Regression test pins
  the rule.
- **R7 measured — no advantage:** a thread-per-close deferral prototype ran
  the H: small-file workload at parity-to-slightly-worse versus baseline in
  two interleaved pairs (20.9→22.1 s, 15.5→15.8 s). With 32 workers the
  device queue already receives all the close overlap it can use, so the
  close-finalizer stage stays retired; revisit only if worker counts ever
  drop materially.

## 2026-07-29 first-principles study of the 49× small-vs-large gap

The owner's tar thought-experiment ("tar the tree and it copies 49× faster
with metadata intact") probes exactly the right spot, and the analysis plus
two new measurements pin down where the gap lives and what can move it:

**Why tar is 49× — and why that bound doesn't transfer.** Tar wins because
the container *stays one file*; materializing 20,000 real files on the
destination pays the identical per-file tax (an untar onto this drive would
crawl exactly like a copy). The tool-reachable bound is therefore the
per-file device floor, which the traces decompose precisely: ~0.885 ms/file
wall ≈ **three synchronous USB round-trips per file** (create transaction,
data write, close/cleanup flush) at ~0.3 ms bridge latency, with the Oyen
bridge sustaining only ~3 effective concurrent operations — which is why
extra software overlap (R7) bought nothing.

**Every app-level lever has now been enumerated and measured or excluded:**

- *More overlap* — R7, measured: no gain (bridge concurrency is the cap).
- *Fewer ops per file* — excluded by the platform: Windows has no
  create-with-data call, and MFT residency only absorbs files ≤ ~700 bytes
  (our 4 KiB fixture and most real small files exceed it).
- *Lazy data via `FILE_ATTRIBUTE_TEMPORARY`* — measured (A/B ×2 on H:,
  create-with-TEMPORARY then final stamp clearing it; semantics verified
  exact): 23.9→23.2 s and 18.8→17.0 s — **3–10 %, inside session noise**.
  Consistent with the decomposition: data writes were already cheap
  (~170 µs); the cost is metadata transactions, which the hint doesn't
  touch. Not shipped.
- *Metadata on a background thread* (the owner's suggestion) — this is
  R7/close-offload and the stamp coalescing already shipped (ADR 0031);
  what remains per file is transaction work the filesystem itself commits
  synchronously on this mount policy.

**The one lever that actually collapses the gap is the volume's
write-caching policy.** "Quick removal" (the Windows default for external
drives) makes NTFS commit each file's metadata transactions through to the
device; "Better performance" lets the NTFS log batch thousands of creates
per flush — that batching is what tar's single-stream speed is made of.
This is user-side by design (bigcp never changes system settings) and needs
one owner-approved measurement with the policy flipped to quantify the
collapse factor honestly; until then the README's removal section carries
the qualitative guidance. A report hint ("this destination looks
metadata-bound; consider the Better-performance policy + Safely Remove")
is registered as a candidate — it needs behavior-based detection, since
policy inference via IOCTL was deliberately deleted (ADR 0027).

## 2026-07-29 the collapse factor, measured: Better-performance policy on H:

The owner switched H: from Quick-removal to Better-performance (OS write
caching on) and the small-file workload was rerun — same fixture shape, two
interleaved pairs:

| Tool | Quick removal (before) | Better performance (after) |
|---|---|---|
| bigcp (defaults) | 17.7–18.0 s | **5.19 s / 5.18 s (3,861 files/s, 15.8 MB/s)** |
| robocopy `/MT:32` | 23.1–24.5 s | 12.7 s / 15.3 s |

Findings:

1. **The policy flip alone bought bigcp 3.4×**, exactly as the
   first-principles analysis predicted: with the NTFS log batching metadata
   transactions, the per-file close flush vanished (phase table: close cost
   2,350 → 113 µs) and creates fell to ~58 µs effective.
2. **bigcp is now 2.4–3.0× faster than robocopy on this cell** — robocopy
   barely improved. Once the device stops serializing every tool equally,
   the directory-affine scheduling, deep queues, create-time stamping, and
   32-worker profile convert directly into throughput. This is the
   algorithmic advantage the design carries; the write-through policy had
   been masking it.
3. **The small-vs-large gap collapsed 49× → ~14×** (15.8 vs 224.8 MB/s).
   The remainder is per-file software cost across both trees plus NTFS
   transaction work that caching amortizes but cannot eliminate.
4. Durability note, unchanged: with Better performance the standard
   "Safely Remove Hardware" step (or `--flush`) matters before unplugging —
   exactly what the README's removal section already instructs.

This gives the registered metadata-bound report hint its honest wording:
"switching this destination to the Better-performance policy sped this
workload up ~3.4× in measurement; use Safely Remove before unplugging."

## 2026-07-29 what could make small files faster still (candidate register)

With the cached-policy result in hand (5.18 s, 3,861 files/s, ~14× from
sequential), the phase table re-ranks the field: create is again the top
item (37.2 worker-seconds — now NTFS log/MFT-allocation contention, since
the device round-trips are gone), write 9.0, everything else small; workers
are still idle ~70 % of wall, pointing at dispatch cadence. Candidates were
registered in recommended experiment order; later dispositions are recorded
inline rather than rewriting the original evidence:

1. **Worker-count sweep under the cached policy** — 32 was tuned for the
   write-through regime; the contention profile changed (trivial to run).
2. **Sharding policy by destination regime** — directory affinity was the
   write-through win; on a cached destination the per-directory
   serialization may now be the limiter. A/B affinity vs round-robin.
3. **Relative-handle creates** (`NtCreateFile` with `RootDirectory`) —
   skips per-create path resolution; matters most on the NVMe cells where
   effective create cost is already ~58 µs. **Implemented 2026-07-31 under
   ADR 0048; performance remains unmeasured.** The implementation is confined
   to distinct-drive local-NTFS plain-small workers and falls back to the
   former absolute open when a verified parent capability is unavailable.
4. **The unmeasured cell that matters next: HDD *source*** — source open +
   read is trivial from NVMe (125 µs/file) but will dominate reading many
   small files off a spinning source; the prefetch-pipeline idea belongs to
   that cell's evidence, not this one's.
5. Excluded with reasons: IoRing (no create/metadata ops — only the small
   write phase would batch); container/split tricks (no NTFS API to
   materialize files from a stream — the tar bound stays unreachable for
   real files); NTFS log resizing (obscure user-side tuning, thin
   evidence).

Rough headroom estimate: per-file wall budget is ~259 µs; a cached-create
floor near ~100–150 µs suggests up to ~2× may remain reachable before the
irreducible per-file cost; beyond that only container semantics (not real
files) go faster.

## 2026-07-29 candidate experiments 1–2 executed: defaults confirmed, methodology finding

Ran the first two registered candidates on the cached-policy H: — a
worker-count sweep (16/32/48/64) crossed with dispatch policy (directory
affinity vs an env-gated round-robin prototype), then an interleaved
repetition round of the four contenders:

- **Defaults win:** directory affinity at 32 workers posted **3.31 s and
  4.02 s** — the fastest small-file results recorded on this drive (up from
  5.18 s in the policy-flip session). No configuration beat it with any
  consistency; no change to defaults is justified.
- **Round-robin showed no reliable advantage** on the cached regime (its
  best single runs were matched or beaten by affinity's, and its worst were
  3× slower); the write-through rationale for affinity stands unchanged.
- **Methodology finding, the real yield:** on a write-cached destination,
  each run's lazy flush backlog drains into the *following* run, producing
  order-dependent swings up to ~3× that dwarf any policy difference.
  Back-to-back A/Bs on cached drives are not a valid instrument.
  Consequence recorded for the certified protocol (§12.10): every
  repetition must be preceded by a quiesce step (flush wait or settle
  interval) and orderings must be rotated. The round-robin prototype was
  reverted, not committed.

## 2026-08-02 WSL Plan 9 evidence and optimization (H7 dispositioned)

**Environment:** same machine as the 2026-07-29 entries (Windows 11 Pro
26200, 64 GB RAM); Windows side on C: (KIOXIA EG6 NVMe, NTFS); WSL side the
real `\\wsl.localhost\u2` endpoint — WSL 2 distribution `u2` (kernel
6.18.33.2), ext4 on VHDX backed by NVMe. Release build, `Measure-Command`
wall clock, all Linux-side writes confined to a disposable `/tmp/bigcp-bench`
subtree per the `docs/TESTING.md` remote-endpoint protocol. Workloads:
small = 2,000 × 4 KiB files in 20 directories; large = 1 × 512 MiB file.
**Warm/cold discipline:** the first touch of a distribution pays VM/9P
cold-start (identical small runs measured 1.8 s → 0.97 s Win→WSL and
2.77 s → 0.97 s WSL→Win), so every published number is the median of 3 warm
runs. That is below the certified ≥5-repetition protocol — these numbers
bound, not certify. robocopy was swept over `/MT` (default 8 and 16) on the
small-file cells.

### Baseline (pre-optimization, warm)

| Cell | bigcp | robocopy | Standing |
|---|---|---|---|
| Win→WSL small | 2,062 files/s (0.97 s) | `/MT:16` 2,426 files/s (0.82 s); `/MT` (8) 1,955 files/s | −15% vs `/MT:16` |
| Win→WSL 512 MiB | 224 MB/s (2.29 s) | 560 MB/s (0.91 s) | 2.5× behind |
| WSL→Win small | ~720 files/s cold; 2,056 warm | 2,611 files/s (warm) | 3.6× behind cold |
| WSL→Win 512 MiB | 286 MB/s | 563 MB/s | 2× behind |

### The three experiments that drove the design

1. **Worker scaling (Win→WSL small, `--tune threads=N`, warm):** 1 → 502
   files/s · 4 → 2,673 · 8 → 3,216 · 32 → 3,917 · 64 → 4,192. The knee is at
   32; 64 workers add only ~7% for double the outstanding provider handles,
   so the WSL Auto row moved 16 → 32 workers.
2. **Chunk sweep (Win→WSL 512 MiB, warm):** 1 MiB 226 · 4 MiB 267 · 16 MiB
   248 · 32 MiB 265 · 64 MiB 261 MB/s. Request size is not the lever: one 9P
   handle (fid) caps at ~230–290 MB/s regardless of request size.
3. **Two concurrent 512 MiB files: 408 MB/s aggregate** — the cap is
   per-handle, and parallelism across fids scales. robocopy reaches 560 MB/s
   on ONE file, so intra-file parallel I/O demonstrably works through p9rdr.
   This is the measurement behind ADR 0052's segmented parallel transfers.

### Phase-timing evidence (`--analyze`, 16 workers, warm)

| Cell | Phase means per file |
|---|---|
| Win→WSL small | create_dst **2,787 µs** · set_meta 488 µs · write 347 µs · open_src 54 µs; coordinator ≈ 0. Four destination round trips per new small file (create, write, stamp, close). |
| WSL→Win small | open_src **1,491 µs** · read 812 µs · create_dst 329 µs · set_meta 56 µs. ~95% of the ~2.4 ms/file cost (~2.3 ms) is source-side 9P round trips, which directory-affine scheduling was serializing per directory. |

### What shipped (ADR 0052)

1. WSL Auto workers 16 → 32 (the measured knee; generic UNC unchanged).
2. Small-file striping now applies when **either** side is WSL — a 9P source
   pays ~2.3 of its 2.4 ms/file in source round trips, so directory affinity
   was forfeiting most of the pool on WSL→Win copies.
3. Segmented parallel large-file transfer: an eligible file (single WSL side,
   non-sparse, unnamed-stream-only, ≥64 MiB, not checkpoint-eligible, no
   resume candidate) moves its single opaque temp as
   `K = clamp(size/64 MiB, 2, 8)` parallel identity-verified segments; the
   whole-file xxh3-128 digest is preserved by a local-side pass.
4. Round-trip cuts: the pre-commit destination probe is skipped for NEW
   streamed files (the non-replacing rename already detects collisions
   atomically), the preflight root stat is reused, and checkpoint temp
   identity is captured once per temp instead of per append.

### Post-optimization (medians of 3 warm runs, same workloads)

| Cell | bigcp before | bigcp after | robocopy best | Standing |
|---|---|---|---|---|
| Win→WSL small | 2,062 files/s | **3,704 files/s** | 2,426 (`/MT:16`) | **53% ahead** |
| Win→WSL 512 MiB | 224 MB/s | **517.7 MB/s** (best 563) | 560 | within 8%; best run ties |
| WSL→Win small | ~720 cold / 2,056 warm | **2,387 files/s** | 2,611 (warm) | ~9% behind (was 3.6× cold) |
| WSL→Win 512 MiB | 286 MB/s | **514 MB/s** | 563 | within 9% |

Correctness through the real 9P stack: standalone `bigcp verify` passed on
the segmented large copies in both directions and on the small tree
(2,021/2,021 objects); destination hashes were byte-identical to the sources;
a local→local regression check ran 7,419 files/s (no regression). The full
workspace suite (218 tests) stayed green, with fmt/clippy and the safety
scripts clean.

Fragmentation note: `bigcp-testkit extents` reads NTFS retrieval pointers,
which the Plan 9 provider does not expose, so the Win→WSL cells carry no
extent figures; the WSL→Win path reuses the same preallocated temp
publication whose single-extent evidence is recorded in the 2026-07-29
entries, and no new extent capture was taken this session.

## 2026-08-02 generic UNC (loopback-indicative)

**Environment:** same machine and day as the WSL entry above; the generic-UNC
endpoint is the loopback admin share `\\localhost\C$` — the complete SMB
client/server stack with no physical network underneath it. Loopback answers
in microseconds where a real network answers in hundreds, so **every number
in this entry is loopback-indicative only; network-class SMB remains
unmeasured and H6 stays open.** Workloads and warm discipline as in the WSL
entry: small = 2,000 × 4 KiB files in 20 directories, large = 1 × 512 MiB,
warm runs, release build — ranges below span runs and sit below the certified
≥5-repetition protocol.

### Baseline (pre-change): bigcp already led 3 of 4 loopback cells

| Cell | bigcp | robocopy | Standing |
|---|---|---|---|
| local→UNC small | 3,504–3,750 files/s | `/MT:16` 980 files/s | 3.6–3.8× ahead |
| local→UNC 512 MiB | ~610 MB/s median | 495 MB/s | ~23% ahead |
| UNC→local small | 3,645–4,316 files/s | 3,243 files/s | 12–33% ahead |
| UNC→local 512 MiB (cache-hot) | ≈2,500 MB/s | 3,302 MB/s | behind — loopback artifact |

The one trailing cell is a loopback artifact, not a redirector result: the
"remote" 512 MiB source is served from the local page cache at memory speed,
so the cell measures per-syscall overhead rather than anything a network
would show.

### What shipped (ADR 0053): a latency gate for remote-source striping

ADR 0052 established that when a remote *source*'s per-file round trips
dominate, directory affinity serializes the expensive side; whether that
arithmetic applies to a generic UNC source depends entirely on its latency
class. The remote volume probe already issues three handle-bound native
queries at preflight, so timing them costs zero extra I/O: `VolumeInfo` now
publishes the minimum of the three as `remote_query_latency` (`None` for
local volumes), and the pure policy `remote_source_striping` stripes
plain-small dispatch for a generic-UNC source only when that floor is
network-class (`REMOTE_SOURCE_STRIPE_LATENCY_FLOOR` = 250 µs, inclusive).
WSL sources stripe unconditionally as before; local sources never do. The
run-start "remote topology" log line records the measured latency and the
chosen locality.

The floor sits between measurements on both sides: loopback query floors
measured ~2–80 µs, where affinity is measurably better (the affine UNC→local
baseline above beats robocopy's interleaved creates), while LAN RTTs start
around ~200 µs–1 ms+, where the Plan 9 arithmetic — source round trips
dominate the per-file cost — applies unchanged.

### Gate validation (loopback, plus WSL revalidation)

- The loopback round-trip floor measured ~2 µs and the run log showed the
  affinity branch: `source round trip ~2us (loopback-class), keeping
  directory affinity`.
- No regression: UNC→local small files stayed at ~4,000–4,157 files/s after
  the change (baseline 3,645–4,316).
- WSL revalidated after the change: the log confirmed striped dispatch and
  32 workers; single runs measured Win→WSL 3,264 files/s and WSL→Win 2,320
  files/s, consistent with the 2026-08-02 medians above.

### Deliberately not shipped: SMB single-file segmentation

Segmented parallel transfers (ADR 0052) stay WSL-only. SMB already pipelines
writes through its credit window, extra handles on one file can break leases
and regress real networks, and loopback cannot measure any of that — so
extending segmentation to UNC on loopback evidence would be exactly the
unfounded claim the H6 gate exists to prevent. H6 remains open pending an
approved network-class scratch share.

## Outstanding

**H6 — redirector overlap (registered 2026-07-31; network-class unmeasured,
loopback evidence indicative):** ADR 0045's
fixed two-buffer pipeline should reduce idle time by overlapping one source
read with one destination write, while parallel non-checkpointed streamed files
should cover independent-file SMB latency better than the former
coordinator-only path. Correctness tests prove actual stage overlap, ordering,
bounded memory, cancellation, and checkpoint routing; they do not prove a
speedup. Loopback SMB evidence (the "2026-08-02 generic UNC
(loopback-indicative)" entry above) is indicative only, though bigcp already
led robocopy in three of four loopback cells there, and single-file
segmentation is deliberately NOT extended to SMB (lease-break risk,
credit-pipelined writes, untestable without a real network; ADR 0053). The
first approved remote experiment must compare the same bounded
fixture and verification policy across the pre-ADR baseline, current defaults,
manual thread/chunk sweeps, and robocopy's best applicable `/MT`/`/J` settings,
with signing/encryption/compression and endpoint topology recorded.

**H7 — WSL Plan 9 specialization (registered 2026-07-31; measured and
dispositioned 2026-08-02):** ADR 0046's mechanisms were benchmarked on a real
`\\wsl.localhost` endpoint (the 2026-08-02 entry above). The measurement
confirmed the hypothesis's direction and refined its values: per-9P-handle
throughput caps near ~230–290 MB/s while concurrency across handles scales,
so ADR 0052 moved the profile to 8 MiB/32 workers, extended striping to WSL
sources, and added segmented parallel transfers for eligible large one-sided
WSL files. Post-change medians: small files ahead of robocopy's best swept
`/MT` setting in the Win→WSL direction, large files within ~9% both ways.
The numbers are medians of 3 warm runs, below the certified ≥5-repetition
protocol — indicative, not certified.

**H8 — local NTFS relative final-name creates (registered 2026-07-31,
unmeasured):** ADR 0048 caches one verified destination-parent handle per
directory-affine worker/directory and opens plain-small final names with
`NtCreateFile(RootDirectory=parent)`. This should reduce repeated absolute
parent parsing on distinct-drive local NTFS copies without changing source
opens, stream discovery, data I/O, completion, or any other transport. Bounded
tests prove create-new collision safety, replacement validation reuse,
metadata correctness, stale-parent refusal, hostile-component rejection, and
selection isolation; they do not prove a speedup. The first approved run must
compare the current implementation with an otherwise-identical absolute-open
baseline and robocopy, use the certified quiesce/rotation protocol, and report
results by directory shape because the one-parent-open amortization depends on
files per directory.

The elevated filesystem matrix, repeated-run certified benchmark protocol,
ADR 0036 same-spindle HDD comparison, and ADR 0037's generic-UNC profile
comparison remain unexecuted (its WSL half was measured in the 2026-08-02
entry; the generic half has only the loopback-indicative 2026-08-02
baseline). The same-spindle
implementation is covered by deterministic topology/transport tests and a
small verified same-volume integration case, but those tests prove correctness
and phase ordering—not a speedup. Running the `[HW]` cell requires separate
owner approval under `docs/TESTING.md`, including its exact bounded workload,
target scratch root, write volume, duration, and drive impact. The remote
generic-UNC 8 MiB/16-worker Auto row remains an independently bounded static
default, not a measured speedup claim (its loopback-indicative 2026-08-02
baseline does not change that); the WSL 8 MiB/32-worker row is
measured (2026-08-02, warm medians of 3 — indicative, not certified); any
SMB/network benchmark additionally requires an approved scratch share path.
ADR 0045's two-buffer and parallel-stream mechanics and ADR 0048's local
relative-create path are likewise benchmark-pending rather than measured
speedup claims; ADR 0046/0052's WSL scheduling, round-trip, and segmented
mechanisms are covered by the 2026-08-02 entry. Endurance,
million-entry, and competitor sweeps stay prohibited (VISION).

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
