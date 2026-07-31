# ADR 0042: NTFS is the only filesystem certification target

**Status:** Accepted

## Context

The product supports local NTFS, ReFS, FAT/FAT32, and exFAT, plus generic UNC,
mapped-drive, and WSL endpoints. Earlier documents described unrun elevated
VHDX and remote matrices as future certification evidence. That language made
optional compatibility work look like release debt and blurred the difference
between a documented support contract and a certification claim.

On 2026-07-31 the owner clarified that every filesystem except NTFS is
best-effort and need not be certified.

## Decision

NTFS is the sole filesystem certification target. ReFS, FAT/FAT32, exFAT,
generic UNC/provider filesystems, and WSL remain supported according to their
documented capability, projection, warning, and verification contracts, but
are best-effort and never gate release certification.

Disposable VHDX and approved remote-endpoint exercises may still run under the
existing safety and authorization rules. They provide bounded compatibility
evidence for the tested Windows build, provider, or device only. Their absence
is not a release blocker, and passing them does not create a filesystem-wide
certification claim. Important non-NTFS copies should use same-run `--verify`
and a later standalone `bigcp verify` against the actual destination.

This decision does not add another prompt. FAT/exFAT and remote copies retain
their existing one-startup-prompt acceptance rules because they have known
representation or endpoint risks; ReFS remains non-interactive because its
best-effort status is an evidence boundary rather than a new known data-loss
projection.

## Consequences

Release criteria and production-readiness claims apply filesystem
certification only to NTFS. Non-NTFS matrices are labeled optional
compatibility exercises throughout active documentation. ADR 0029 remains the
historical ReFS decision, while this ADR generalizes and supersedes its
matrix-certification framing.
