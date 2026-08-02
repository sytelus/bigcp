# ADR 0055: Local large-stream overlap and device-bus classification

**Status:** Accepted (amends ADR 0028's overlap rationale — buffered I/O is
retained and no unbuffered path returns; only the claim that the cache
manager alone supplies read/write overlap is superseded — and removes the
adapter-MTL chunk clamp from profile composition)

## Context

Three independent mechanisms were holding distinct-drive local NTFS
large-stream throughput below robocopy `/J` on this project's own benchmark
pair (D: 4 TB NVMe → C: KIOXIA EG6 1 TB NVMe, both behind Intel VMD; all
figures from clean interleaved pairs with per-run deletion + retrim —
BENCHMARKS.md "2026-08-02 distinct-drive NTFS large-stream overlap"):

- **Half-duplex alternation.** The standard transport moved every large
  stream request-at-a-time on one thread: read a chunk, then write it. With
  read bandwidth R and write bandwidth W that alternation's ceiling is
  `1/(1/R + 1/W)` — about half of either device when R ≈ W — while
  overlapping one read with one write targets `min(R, W)`. Measured on
  8 GiB clean pairs: bigcp 1,788–2,038 MB/s against robocopy `/J`'s
  2,264–2,802 MB/s (~25% behind). ADR 0028 expected the cache manager's
  read-ahead and write-behind to supply the overlap a hand-built pipeline
  would; at multi-GB/s NVMe rates it does not — the alternating thread is
  itself the bottleneck.
- **VMD-hidden misclassification.** Both NVMe drives composed the SATA-SSD
  row: behind Intel VMD/RST the ADAPTER-level `STORAGE_ADAPTER_DESCRIPTOR`
  answers an unspecific bus (RAID), and the R6 rule (2026-07-29,
  BENCHMARKS.md) deliberately falls from an unrecognized bus plus a
  no-seek-penalty answer to the moderate SATA-SSD row. The per-DEVICE
  `STORAGE_DEVICE_DESCRIPTOR.BusType` on the same hardware still answers
  `BusTypeNvme` — evidence the adapter-only classification was discarding.
- **The adapter-MTL chunk clamp.** Profile composition clamped the chunk to
  every nonzero adapter `MaximumTransferLength`. The VMD adapter reports
  2 MiB, silently forcing 2 MiB requests on a Gen4 pair. The MTL bounds one
  storport request — the I/O manager already splits buffered application
  transfers into adapter-sized requests itself — so the clamp only added
  syscalls and pipeline handoffs: 2 MiB requests measured ~1.9–2.0 GB/s
  where 16 MiB requests measured ~2.7–2.8 GB/s.

## Decision

- Standard-transport **unnamed** streams large enough to reach the
  temp+rename streaming path move through the same bounded two-buffer
  read/write-overlap pipeline every redirector and WSL stream already uses
  (`StreamBuffers::for_unnamed_stream` in `core::engine`). Sparse ranges and
  named streams deliberately keep request-at-a-time (`StreamBuffers::new`):
  their segments are typically small, hole-bounded, or ADS-sized, where a
  reader thread per stream costs more than the overlap returns. The
  pipeline's checkpoint ordering invariant is unchanged — boundaries only at
  segment ends. Same-spindle transport is untouched.
- `REDIRECTOR_PIPELINE_BUFFERS` is renamed `PIPELINE_BUFFERS`
  (`core::transport`): the two-buffer constant now describes every
  non-same-spindle transport, not a redirector special case.
- `mem` accounting reserves two coordinator chunks on every non-same-spindle
  transport (`core::devprofile`); redirector workers keep their additional
  `max(large-threshold, 2 × chunk)` per-worker reservation.
- Bus classification consults the per-device descriptor when the adapter
  answer is unspecific (`classify_bus`/`query_device_bus` in `win::device`),
  in a fixed preference order: a specific adapter answer wins exactly as
  before; otherwise a specific device-descriptor answer; two unspecific
  answers stay `Other`; no answer at all stays `None`, so the conservative
  Unknown profile applies unchanged.
- The adapter `MaximumTransferLength` no longer clamps the composed chunk;
  it stays in `DeviceInfo` as a reported fact only.
- The NVMe profile row moves 8 → 16 MiB: measured through the pipeline,
  16 MiB beat 8 MiB by ~12–40% across clean interleaved pairs (fewer
  handoffs and syscalls per stream).

## Consequences

- Memory: one additional chunk in flight per coordinator-inline large stream
  on the standard transport (two instead of one). With `mem`, the budget
  arithmetic reserves exactly those two chunks before capping
  threshold-sized workers, so the override remains an aggregate bound.
- Sparse ranges, named streams, plain-small files, and the same-spindle
  transport are byte-for-byte unchanged; redirector and WSL behavior is
  unchanged.
- ADR 0028's buffered-streaming decision is retained: the overlap is
  buffered on both handles — no unbuffered I/O and no sector-alignment
  machinery return. Its 2026-07-29 reopening finding (buffered at ~44% of
  robocopy `/J` on the fastest internal pair) is dispositioned *within*
  buffered I/O: after this change bigcp measured ahead of robocopy `/J` on
  the same class.
- Hosts whose NVMe hides behind VMD/RST now compose nvme/nvme (16 MiB,
  `min(64, 4×cores)` workers) instead of sata-ssd, verified in run reports.
  The measured small-file gain on this host (5,771–8,494 files/s across the
  session vs robocopy `/MT:32`'s 3,852 on 10,000 × 4 KiB) comes entirely
  from that classification/worker-row correction — the small-file path
  itself was not changed, and ADR 0048's H8 stays unmeasured.
- All numbers are indicative, not certified: clean interleaved pairs with
  per-run deletion + retrim, medians/ranges of small sets. Destination
  SLC-cache state swings consumer-NVMe results 2–4× and one tool's writes
  degrade the next run's cache, so the interleaved delete+retrim protocol is
  mandatory methodology (recorded in BENCHMARKS.md); the certified ≥5-run
  quiesced protocol with recorded Defender/commit state remains pending.

## Validation

- `composition_is_deterministic_and_ignores_the_adapter_mtl` — composition
  stays deterministic and the composed chunk holds the measured 16 MiB row
  against a fixture advertising a 4 MiB MTL.
- `standard_memory_budget_reserves_the_pipelined_coordinator_chunks` — an
  8 MiB budget with a 4 MiB worker buffer leaves (8−4)/2 = 2 MiB per
  coordinator chunk and one worker.
- `bus_classification_prefers_any_specific_answer_over_unspecific_ones` —
  the full fallback table, including the VMD case (unspecific adapter,
  specific device descriptor), the two-unspecific `Other` case, and the
  no-answer conservative path.
- Interleaved benchmark evidence (BENCHMARKS.md "2026-08-02 distinct-drive
  NTFS large-stream overlap (indicative)"): baseline 8 GiB clean pairs
  1,788–2,038 MB/s vs robocopy `/J` 2,264–2,802; post-change 2 GiB clean
  pairs 2,656–2,808 MB/s vs 1,773–2,758 in the same pairs — bigcp ahead in
  both final pairings, content hash-verified, standalone verification green.
  Full 221-test workspace suite green with fmt/clippy and the safety scripts
  clean.
