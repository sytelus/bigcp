# ADR 0049: Console cancellation and non-interactive write-cache acceptance

**Status:** Accepted

## Context

Graceful cancellation existed only inside the live dashboard (`q`/Esc), and
Windows consoles deliver both press and release key events, so every key
fired twice and Tab navigation skipped every other tab. In `--plain`,
`--quiet`, and redirected runs Ctrl+C hard-killed the process; the
abort-and-rerun contract covered it, but the run lost its clean exit-3
summary and audit closure for no reason. Separately, ADR 0032's Quick-removal
write-cache prompt had no acceptance flag, contradicting VISION's rule that
explicit arguments must make automation non-interactive. Release builds also
use `panic = "abort"`, so RAII Drop alone could never restore raw
mode/alternate screen on a panic, and a mid-run dashboard failure left the
terminal silent with no hint the copy was still running.

## Decision

- Add `bigcp-win::console`: one process-global `SetConsoleCtrlHandler`
  callback and latched atomic flag. The first Ctrl+C/Ctrl+Break is absorbed
  and sets the flag; a second, or any close/logoff/shutdown signal, falls
  through to default termination so a stuck cancellation can always be
  escalated. The CLI installs it and injects `cancel_requested` into both
  display modes as a plain function — `bigcp-tui` stays free of Win32 — and
  the engine keeps polling cancellation between bounded work units (exit 3).
  If installation fails, Ctrl+C keeps its default hard-kill behavior.
- In the dashboard, act only on key **press** events and treat Ctrl+C as the
  same graceful-cancel gesture as `q`/Esc (raw mode disables the console's
  default Ctrl+C processing). Install a terminal-restoring panic hook while
  the session is live; report a dashboard failure on stderr with the fact
  that the copy continues and how to stop it.
- Add `--accept-write-cache-policy`: it suppresses only the interactive
  Continue? prompt; the Quick-removal notice, audit warning, and report hint
  still fire. The gate stays CLI-only — core does not re-gate on the
  acceptance because a slow write-cache policy is a performance notice, not a
  fidelity or safety loss, unlike the degraded-filesystem and remote-path
  acceptances core enforces.

## Consequences

Every output mode now ends a Ctrl+C run with the ordinary graceful-cancel
protocol — final summary, published report, durably synchronized `run_end` —
instead of an interruption, and scripts can pre-accept the Quick-removal
notice without losing it from the log. No mid-run prompt is added, and copy
semantics, scheduling, and the ADR 0032 recommendation text are unchanged.

## Validation

Unit tests pin the handler decision table (first cancel absorbed, second
escalates, close/shutdown never absorbed and never latch the flag), the
press-only key handling with tab-cycling reachability, and the flag's
rejection before the verify/report subcommands. Live console-signal delivery
is manual-smoke territory; no forced process kills are added to the routine
suite.
