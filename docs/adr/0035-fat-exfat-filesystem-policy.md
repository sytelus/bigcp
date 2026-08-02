# ADR 0035: Isolated FAT and exFAT filesystem policy

**Status:** Accepted; amended by ADR 0054 (per-file operation sequence: the
create-time stamp is gone; the finish-time stamp is the only one)

## Context

ADR 0020 rejected FAT-family filesystems to keep exact NTFS/ReFS timestamp,
identity, metadata, and publication semantics simple. External media commonly
uses FAT32 or exFAT, but those filesystems cannot represent all NTFS features
and some drivers reject the newer information classes used by the strict path.
Applying a global timestamp tolerance or lowest-common-denominator metadata
contract would weaken correctness and rerun performance on NTFS/ReFS.

## Decision

Support local FAT/FAT32 and exFAT sources and destinations through one immutable
destination `FilesystemPolicy`. NTFS/ReFS keep exact FILETIME comparison, their
full attribute mask, the 128-bit enumeration/identity fast path, and
capability-gated extended POSIX rename. FAT uses 10 ms creation and 2 s
last-write comparison intervals plus a 4,294,967,295-byte per-file ceiling;
exFAT uses 10 ms creation and last-write intervals. FAT-family attributes are
projected to `READONLY`, `HIDDEN`, `SYSTEM`, and `ARCHIVE`, and final timestamps
are restamped after data writes.

Optional behavior is selected from volume capability flags, not filesystem
names. Unsupported streams and EAs are counted and warned, sparse files become
dense, EFS state is lost with a warning, protected DACL preservation is
skipped, and reparse objects fail rather than being followed or flattened.
These unsupported features do not force a regular file onto the transactional
path. Verification compares the destination-representable projection and marks
its result as projected.

Where a FAT-family driver rejects `FileIdInfo` or
`FileIdExtdDirectoryInfo`, the Win32 layer falls back to the legacy 64-bit FAT
file ID and one-pass `FileIdBothDirectoryInfo` enumeration, never a per-child
open. Where extended rename is rejected, handle-bound publication falls back
to legacy `FileRenameInfo` without claiming POSIX semantics.

A real copy to FAT/exFAT requires one default-no startup confirmation. The
explicit `--accept-degraded-filesystem` flag bypasses that confirmation and is
required for noninteractive use. Dry-run is exempt because it mutates nothing.
The core library enforces acceptance independently of the CLI.

## Consequences

FAT/exFAT become useful, fast copy targets without adding timestamp tolerance,
extra metadata syscalls, or fallback enumeration to the NTFS/ReFS hot path.
Losses and hard limits are visible before and during the run, and report fields
identify the source/destination filesystem and projected verification.

FAT-family media still has no metadata journal, FAT cannot hold files above its
ceiling, links cannot be copied, and source features absent from the destination
cannot be preserved. A successful projected verification proves the unnamed
content and every representable field, not the survival of unsupported source
metadata. Dedicated elevated disposable-VHDX matrix evidence remains a release
gate; routine CI covers policy boundaries without formatting existing media.
