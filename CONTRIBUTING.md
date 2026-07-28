# Contributing

Use a focused branch, preserve `PLAN.md`, `VISION.md`, and `LIMITATIONS.md`, and
keep unrelated user changes intact. Rust code must format cleanly, compile with
all workspace lints, and contain no unsafe outside `bigcp-win`.

Before proposing a change, answer in the description:

- Which invariant I1-I13 can it affect, and which test enforces that invariant?
- Does it add filesystem I/O to a common or hot path? If yes, what measurement
  and write budget justify it?
- Does it change the normative object contract? If yes, include an ADR,
  `docs/SEMANTICS.md`, limitation, and schema review.
- Does it touch engine finalization or journal ordering? If yes, it cannot ship
  as 1.0 until the chaos gate passes.
- Are all tests inside validated new sandboxes, and are fresh writes bounded?
- Have log/report format changes stayed additive within schema v1?

Run the commands in `docs/TESTING.md`. Do not run tests on F:, G:, H:, user
trees, real removable media, or raw physical devices.
