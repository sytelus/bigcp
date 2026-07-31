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
set while preserving the remaining exclusions and `--include-system`.

The index is filename ordered. `docs/MAINTENANCE.md` maps decisions to code and release checks.
