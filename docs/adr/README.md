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
journal replay.

The index is filename ordered. `docs/MAINTENANCE.md` maps decisions to code and release checks.
