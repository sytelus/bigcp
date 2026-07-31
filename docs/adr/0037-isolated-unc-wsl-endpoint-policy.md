# ADR 0037: Isolate UNC and WSL endpoint policy

**Status:** Accepted

**Transfer-mechanics updates:** ADR 0045 supersedes the decision to reuse the
standard sequential transport. ADR 0046 then gives WSL a distinct transport,
8 MiB/16-worker Auto row, and provider-specific scheduling/handle policy.
Endpoint semantics, acceptance, and recovery policy in this ADR remain in
force.

## Context

The original local-only boundary kept SMB behavior out of the copy engine, but
it also prevented reliable use of generic UNC shares, mapped network drives,
and WSL's Windows interoperability paths. Treating all of them as an unknown
local disk would be incorrect: redirectors do not expose local physical disk
topology, server-side durability is outside the client process, and WSL maps a
case-sensitive Linux namespace and metadata model through a 9P provider.

UNC is also independent of filesystem type. A share can advertise NTFS, ReFS,
FAT, exFAT, or a provider-specific name, so adding UNC variants to every
filesystem decision would duplicate policy and risk changing the measured
local hot path.

## Decision

Add one immutable `EndpointKind` axis—`Local`, `Unc`, or `Wsl`—beside the
existing filesystem capabilities and device class:

- Normalize ordinary UNC to extended-length syntax and canonicalize legacy
  `\\wsl$` to `\\wsl.localhost` before deriving root, state, and lock identity.
- Recognize mapped drives as remote from `GetDriveTypeW`, then classify the
  opened final path so a mapped WSL drive retains WSL policy.
- Keep the existing local `GetVolumeInformationW`/disk-IOCTL route unchanged;
  remote roots use handle-bound `NtQueryVolumeInformationFile` queries and
  never issue local disk, bus, cache, extent, or same-spindle IOCTLs.
- Apply known remote NTFS/ReFS/FAT/exFAT semantics using server-advertised
  capabilities. Unknown remote filesystems conservatively preserve regular
  content and exact last-write time without claiming creation/access times or
  Windows attributes.
- Give WSL the same narrow content/last-write projection, exact case-sensitive
  destination joins, no Linux uid/gid/mode/xattr/special-file claims, and hard
  failure for unsupported reparse objects rather than traversal or flattening.
- Retain the standard bounded synchronous engine. Auto profiles use 8 MiB and
  16 workers for generic UNC and 4 MiB and 8 workers for WSL; a remote source
  caps the composed worker count. Remote destinations skip dense local-volume
  preallocation hints. Manual profile/tune bounds remain available.
- Map common network disconnect codes to the existing five-failure
  `device_gone` breaker. Recovery stays abort-and-rerun with no retry or
  reconnect state.
- Require one default-no startup acceptance for a mutating remote copy, or
  `--accept-remote-paths` in automation. FAT/exFAT, remote, and Quick-removal
  notices are emitted before at most one prompt. Dry-run and standalone verify
  are exempt because they do not mutate the destination.

The audit profile records each endpoint, and reports include explicit remote
and WSL hints. The core library repeats the acceptance gate before mutation so
non-CLI callers cannot bypass it accidentally.

## Consequences

Local NTFS/ReFS/FAT/exFAT classification, device discovery, same-spindle
selection, allocation hints, and performance profiles remain on their existing
paths. Remote policy can evolve in `endpoint.rs`, remote volume probing,
`FilesystemPolicy`, and `devprofile.rs` without forking the file engine or
duplicating outcome, resume, verification, and audit logic.

Remote completion cannot prove server-side stable storage, and unknown
providers that cannot round-trip exact last-write time may conservatively
recopy or fail verification. WSL UNC remains slower than native Linux I/O for
sustained work because each operation crosses the Windows/Linux translation
boundary. The default profile is a bounded engineering choice, not a measured
universal optimum; performance claims require separately approved scratch
network/WSL benchmarks.

## Validation

Unit coverage fixes direct/extended UNC and WSL-alias classification, guards
extended local paths against UNC misclassification, verifies mapped-drive
effective endpoint selection, exercises WSL exact-name policy and metadata
projection, confirms remote source profile composition, and classifies common
redirector failures. Routine gates retain the full local suite. Bounded live
smoke coverage uses a read-only WSL source and a newly created local temporary
destination; generic SMB, mapped-drive, WSL-destination, and performance cells
remain explicit certification gaps.
