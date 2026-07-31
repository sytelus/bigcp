# Architecture decision records

ADRs 0001-0025 seed the original plan's decisions. ADR 0026 records the
implemented checkpoint-identity hardening. ADRs are append-only: supersede an
old decision with a new numbered ADR rather than silently rewriting history.
Contract, persistence-format, invariant, and default-profile changes require an
ADR. ADR 0034 narrows direct writes to plain small files so auxiliary data is
published transactionally.

The index is filename ordered. `docs/MAINTENANCE.md` maps decisions to code and release checks.
