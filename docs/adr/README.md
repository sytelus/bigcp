# Architecture decision records

ADRs 0001-0025 seed the original plan's decisions. ADR 0026 records the
implemented checkpoint-identity hardening. ADRs are append-only: supersede an
old decision with a new numbered ADR rather than silently rewriting history.
Contract, persistence-format, invariant, and default-profile changes require an
ADR. ADR 0034 narrows direct writes to plain small files so auxiliary data is
published transactionally. ADR 0035 supersedes the NTFS/ReFS-only scope with
an isolated, opt-in FAT/exFAT filesystem policy. ADR 0036 adds a
topology-gated phased transport for same-spindle HDD copies without changing
the standard device path. ADR 0037 supersedes the local-only network boundary
with an isolated UNC/WSL endpoint axis, bounded redirector profiles, and one
explicit remote-copy acceptance without changing local discovery or transport.
ADR 0038 unifies report and journal-compaction publication behind unique,
synchronized, atomic siblings. ADR 0039 makes native/provider parsing and the
testkit path boundary explicitly fail closed on malformed or rooted input.
ADR 0040 extends those guarantees to record-local directory names, disjoint
audit-artifact roles, exact concurrent-buffer accounting, and streaming
journal replay. ADR 0041 supersedes ADR 0038's close-then-path-rename mechanics
with exact-handle state-artifact publication and shared safe temporary naming.
ADR 0042 makes NTFS the sole filesystem-certification target and all other
filesystem/provider paths best-effort. ADR 0043 standardizes the implemented
architecture as one product copy engine with two completion strategies and
two topology transports. ADR 0044 makes a failed READONLY metadata rollback a
first-class reported failure instead of discarding it. ADR 0045 adds a bounded
two-buffer redirector pipeline and safe parallel streaming below the checkpoint
boundary while leaving both local transports unchanged. ADR 0046 supersedes
its shared WSL defaults with a distinct Plan 9 transport identity, striped WSL
destination creates, sequential hints, and fewer metadata round trips. ADR
0047 removes `$RECYCLE.BIN` from the default volume-root OS-artifact exclusion
set while preserving the remaining exclusions and `--include-system`. ADR
0048 gives distinct-drive local NTFS plain-small workers a verified, one-entry
destination-parent handle cache and native relative child creates; all other
filesystem, endpoint, topology, and completion paths remain unchanged. ADR
0049 adds a process-global console cancel handler (first Ctrl+C/Ctrl+Break
graceful, second escalates) plus `--accept-write-cache-policy`, keeping that
acceptance CLI-only because the Quick-removal notice is performance advice,
not a fidelity gate. ADR 0050 makes a failed `--flush` best-effort poison the
destination's last-write stamp so the rerun's skip heuristic detects and
replaces the possibly-non-durable file instead of skipping it forever. ADR
0051 reclaims orphaned resume temporaries only under a four-part journal
proof (opaque shape, recorded name, recorded identity, handle-verified
identity match) while everything unproven stays reported and is never
deleted. ADR 0052 amends ADR 0046 with measured WSL values: 32 workers,
striping on either WSL side, segmented parallel identity-verified transfers
for eligible large one-sided WSL files, and three per-file round-trip cuts,
while keeping 0046's chunk window, sequential hints, and deferred stamp. ADR
0053 extends ADR 0052's source-striping arithmetic to generic redirectors
behind a measured latency gate: the remote probe's existing volume queries
are timed at zero extra I/O, a source at or above the 250 µs round-trip
floor stripes plain-small dispatch while loopback-class shares keep
directory affinity, the once-per-run decision is logged, and SMB single-file
segmentation is deliberately withheld pending H6.

- **0054 — Single authoritative stamp for restamp destinations.** FAT-family,
  generic-UNC, mapped-remote, and WSL destinations drop the create-time stamp
  their mandatory finish-time restamp superseded byte-for-byte: one fewer
  device-visible metadata write per small file (a physical dirent write on
  write-through flash, a network round trip on redirectors) with strictly
  better interrupted-file detectability; strict local NTFS/ReFS keep
  ADR 0031 unchanged, and the flash wall-clock claim is registered as
  hypothesis H9.
- **0055 — Local large-stream overlap and device-bus classification.**
  Standard-transport unnamed large streams move through the same bounded
  two-buffer read/write-overlap pipeline the redirectors and WSL use, ending
  half-duplex alternation (ceiling `1/(1/read + 1/write)`) that measured
  ~25% behind robocopy `/J` on a distinct-NVMe pair; sparse ranges and named
  streams keep request-at-a-time, `REDIRECTOR_PIPELINE_BUFFERS` becomes
  `PIPELINE_BUFFERS`, and `mem` reserves two coordinator chunks on every
  non-same-spindle transport. Bus classification falls back to the
  per-device descriptor when the adapter answer is unspecific (NVMe behind
  Intel VMD no longer demotes to the SATA-SSD row), the adapter
  MaximumTransferLength no longer clamps the composed chunk, and the NVMe
  row moves to 16 MiB — amending ADR 0028's overlap rationale while keeping
  its buffered-I/O decision.

The index is filename ordered. `docs/MAINTENANCE.md` maps decisions to code and release checks.
