# ADR 0032: Write-cache policy detection, pre-copy notice, and recommendation

**Status:** Accepted

## Context

Measurement showed the destination's write-cache policy dominates
small-file throughput: flipping an external HDD from Quick removal to
Better performance sped the bounded 20k-file workload ~3.4× for bigcp
(close cost 2,350 → 113 µs) while robocopy barely moved. Cache-state
inference had been deleted as cosmetic (ADR 0027); this measured
consequence revives it under the standing rule that deleted items return
only with fresh evidence.

## Decision

1. `profile_device` regains a query-only `IOCTL_DISK_GET_CACHE_INFORMATION`
   probe (`DeviceInfo::write_cache_enabled`); bigcp never modifies the
   policy. [Amendment 2026-08-01: the probe now uses
   `IOCTL_STORAGE_QUERY_PROPERTY`/`StorageDeviceWriteCacheProperty` — the
   disk-cache IOCTL encodes read access that the zero-access volume handle
   can never satisfy, so this probe had never actually fired.]
2. When the destination reports write caching disabled, the run emits an
   audit warning, the report carries a high-confidence hint with the
   measured factor, and — owner-mandated amendment to the no-prompts rule
   (F19) — the CLI shows a pre-copy notice and, in an interactive terminal
   only, one Continue? [Y/n] confirmation. Non-interactive, `--plain`,
   `--quiet`, and `--dry-run` runs warn without prompting, so scripts never
   block. Mid-run prompts remain forbidden.
3. Recommended user setting, documented in the README: Better performance
   with "Enable write caching on the device" checked (the measured win;
   rerun-repair covers its loss window when paired with Safely Remove) and
   "Turn off Windows write-cache buffer flushing" left unchecked (it
   suppresses the flushes NTFS journaling depends on — power loss can
   corrupt the filesystem itself, which no re-run repairs, for marginal
   additional speed).

## Consequences

Users on the Windows-default Quick-removal policy learn the one setting
that multiplies their small-file throughput before the copy starts, with
the trade-offs stated; automation behavior is unchanged. F19's wording in
PLAN §10.1 is annotated accordingly.
