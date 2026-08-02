# ADR 0053: Latency-gated remote-source striping

**Status:** Accepted

## Context

ADR 0052 established the scheduling arithmetic for remote *sources*: when a
provider's per-file round trips dominate the per-file cost, directory-affine
dispatch serializes the expensive remote side to protect a cheap local NTFS
directory index, and striping across the worker pool wins. It applied that
conclusion unconditionally to WSL, where every measured configuration showed
Plan 9 round trips dominating.

Generic SMB redirectors do not have one latency class. A loopback or
fast-LAN share answers in tens of microseconds — the 2026-08-02 loopback
`\\localhost\C$` evidence (BENCHMARKS.md) measured query floors of ~2–80 µs
and showed *affine* UNC→local small files at 3,645–4,316 files/s, ahead of
robocopy's interleaved 3,243 on the same tree — while VPN/WAN/typical-LAN
round trips start around ~200 µs–1 ms+, where ADR 0052's arithmetic applies
unchanged. One static answer per endpoint kind is therefore wrong in one
direction or the other.

A zero-cost sample distinguishes the classes: the remote volume probe
already issues three handle-bound native volume queries at preflight, so
timing them adds no I/O, and their minimum is a floor estimate of the
provider round trip.

## Decision

- `VolumeInfo` gains `remote_query_latency: Option<Duration>` — the minimum
  of the three handle-bound native queries the remote probe already issues
  (`bigcp-win::volume`); `None` for local volumes.
- A pure policy `remote_source_striping(endpoint, latency)` in `core::copy`
  decides plain-small source locality once per run: WSL sources always
  stripe (unchanged from ADR 0052); generic-redirector sources stripe only
  when the measured floor is at or above
  `REMOTE_SOURCE_STRIPE_LATENCY_FLOOR` = 250 µs (inclusive); local sources
  never stripe. The floor sits between measurements on both sides: loopback
  at ~2–80 µs, where affinity measured faster, and network RTTs at
  ~200 µs–1 ms+, where source round trips dominate exactly as they did for
  Plan 9.
- `worker_dispatch`'s `source_is_wsl` parameter becomes the run-level
  `stripe_source_reads`; destination-side dispatch policy is untouched.
- The decision is visible: the run-start "remote topology" log line reports
  the measured latency and the chosen locality (e.g. `source round trip
  ~2us (loopback-class), keeping directory affinity`).
- The sample is a static preflight decision under VISION's minor-measurement
  allowance — decided once, logged, never re-tuned mid-run.
- Single-file segmented transfers (ADR 0052) are deliberately NOT extended
  to SMB: the protocol already pipelines writes through its credit window,
  extra handles on one file can break leases and regress real networks, and
  loopback cannot measure any of that. Extension waits on H6's approved
  network-class scratch share (ADR 0045).

## Consequences

Loopback and fast-LAN shares keep the measured directory-affinity behavior
— validated with no regression at ~4,000–4,157 files/s UNC→local small
after the change — while VPN/WAN/LAN-class sources parallelize their
dominating per-file round trips across the pool exactly as WSL sources do.
The floor is a compile-time constant, not a tuning knob: there is no new
user surface to misconfigure, and moving it requires a superseding ADR with
both-sided measurements. The evidence behind the gate is loopback-indicative
only; the network-class benefit is extrapolated from the WSL measurement and
remains unconfirmed until H6's endpoint exists. Local, same-spindle,
FAT-family, and destination-side behavior are byte-for-byte unchanged.

## Validation

`remote_source_striping_is_latency_gated_for_generic_redirectors` pins the
policy: WSL unconditional with or without a sample, generic UNC affine
without a sample or below the floor, the floor inclusive, and local never
striping whatever a stray sample claims. The updated
`wsl_endpoints_stripe_small_files_without_changing_other_affinity` proves
`worker_dispatch` under the renamed run-level flag still stripes for either
WSL side and keeps the non-striping affinity counterfactual. Live checks
(2026-08-02): the loopback `\\localhost\C$` run logged `source round trip
~2us (loopback-class), keeping directory affinity` with small-file
throughput unchanged, and a WSL run logged striped dispatch with 32 workers
(Win→WSL 3,264 files/s, WSL→Win 2,320 files/s single runs, consistent with
the ADR 0052 medians). BENCHMARKS.md's "2026-08-02 generic UNC
(loopback-indicative)" entry records the full evidence and its explicit
loopback-only scope.
