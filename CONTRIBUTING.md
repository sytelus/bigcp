# Contributing

New to the repository? Start with [CLAUDE.md](CLAUDE.md) — the one-page entry
point (build commands, binding safety rules, code map, and the
which-document-for-which-change matrix) for human and AI maintainers alike.

Use a focused branch and keep unrelated user changes intact. Treat `PLAN.md`,
`VISION.md`, and `LIMITATIONS.md` as governing inputs: change them only when the
task explicitly authorizes a contract/scope/documentation update, then update
the relevant ADRs and re-pin the checked hashes. Rust code must format cleanly,
compile with all workspace lints, and contain no unsafe outside `bigcp-win`.

Before proposing a change, answer in the description:

- Which invariant I1-I13 can it affect, and which test enforces that invariant?
- Does it add filesystem I/O to a common or hot path? If yes, what measurement
  and write budget justify it?
- Does it change the normative object contract? If yes, include an ADR,
  `docs/SEMANTICS.md`, limitation, and schema review.
- Does it touch either completion strategy in the one product engine or
  journal ordering? If yes, it cannot ship as 1.0 until the chaos gate passes.
- Are all tests inside validated new sandboxes, and are fresh writes bounded?
- Have log/report format changes stayed additive within schema v1?

Run the commands in `docs/TESTING.md`. Tests may touch only the system drive
and the drive holding the code checkout — never other drives, user trees, real
removable media, or raw physical devices. Keep new tests in the routine
correctness tier; performance, stress, many-file, or long-running tests are
disabled by default and follow the opt-in and permission protocol in
`docs/TESTING.md`.
