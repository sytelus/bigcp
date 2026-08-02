# 0054 — Single authoritative stamp for post-write-restamp destinations

**Status:** Accepted (amends ADR 0031's scope note and ADR 0035's per-file
operation sequence)

## Context

The direct small-file writer stamped destination timestamps twice on every
FAT, exFAT, generic-UNC, and mapped-remote copy: once at create time
(ADR 0031's fast path) and once from `finish`, because those destinations'
drivers update time and archive fields while data is written
(`FilesystemPolicy::requires_post_write_stamp`). Both writes carried the
identical projected `BasicMetadata`, so the create-time stamp was superseded
byte-for-byte on every file. Only WSL already deferred (ADR 0046).

The waste is not symmetric across media. FAT-family volumes are
overwhelmingly removable flash under Windows' default "Quick removal"
write-through policy, where every metadata write is a physical device write
(ADR 0031 measured the ~2 ms class of such operations; ADR 0032 measured the
policy's ~3.4× workload effect). On generic UNC the redundant stamp is a full
network round trip per small file.

## Decision

The create-time stamp is passed exactly when the destination does **not**
require a post-write restamp (`initial_small_file_stamp` in
`crates/core/src/engine.rs`). Consequences by class:

- Strict local NTFS/ReFS: unchanged — the create-time stamp remains the only
  one (ADR 0031's measured fast path, byte-identical behavior).
- FAT/exFAT, generic UNC, mapped remote, WSL: the mandatory finish-time
  restamp becomes the only stamp. Per new small file this removes one
  `SetFileInformationByHandle(FileBasicInfo)` — one device-visible dirent
  write on write-through flash, one network round trip on a redirector.

Crash detectability strictly improves on the deferred classes: an
interrupted file that never reached `finish` keeps the driver's current
last-write time, which differs from the source beyond the comparison quantum,
so a rerun sees a second Replace signal in addition to short size. The
residual window (interruption between the final stamp and close) is identical
to the previous window between the create-time stamp and close.

## Consequences

- FAT/exFAT plain-small destination syscalls drop from five to four
  (create, write, stamp, close); the sequence in ADR 0035 reads one shorter.
- Mid-copy observers of a redirector destination see provider-assigned
  timestamps until `finish`; in-progress files carry no metadata contract,
  and completed-run semantics are unchanged (verification compares only the
  post-restamp result under the destination's quantization).
- The throughput effect on write-through flash is registered as hypothesis
  H9 (BENCHMARKS.md): no FAT/exFAT medium was attached when this landed, so
  the ops-per-file arithmetic is code-proven but the wall-clock claim awaits
  the elevated FAT/exFAT VHDX matrix cells (docs/TESTING.md) or a physical
  stick.

## Validation

- `post_write_stamp_destinations_defer_the_create_time_stamp` pins the
  predicate for both classes; `deferred_stamp_is_applied_after_sequential_writes`
  (bigcp-win) pins the deferred mechanics; `create_time_stamp_survives_writes_in_sandbox`
  pins the untouched NTFS contract.
- Live loopback SMB check (2026-08-02): 2,000-file tree copied through
  `\\localhost\C$`, standalone verification 2,021/2,021 passed, destination
  last-write tick-equal to the source — the finish-time stamp is
  authoritative with no create-time stamp preceding it.
