# ADR 0047: Copy the Recycle Bin by default

**Status:** Accepted

## Context

ADR 0007 established a small, volume-root-only set of operating-system
artifacts that bigcp excludes by default. That set included `$RECYCLE.BIN`.
Unlike paging, hibernation, dump-stack, and system-volume state, the Recycle
Bin can contain user data that the owner expects a whole-volume copy to
attempt. Classifying the entire directory as an OS artifact prevented ordinary
copy and per-object error reporting for those contents.

On 2026-07-31 the owner directed that `$RECYCLE.BIN` must not be excluded as an
OS artifact by default.

## Decision

Remove `$RECYCLE.BIN` from the default volume-root exclusion set. It follows
the same enumeration, copy, verification, accounting, and audit paths as any
other source directory. Protected or concurrently changing contents fail and
are reported through the existing ordinary error semantics.

Keep the remaining default exclusions unchanged: `System Volume Information`,
`pagefile.sys`, `swapfile.sys`, `hiberfil.sys`, and `DumpStack.log.tmp`.
`--include-system` continues to opt those artifacts in. This decision adds no
new option, prompt, special Recycle Bin parser, restore operation, or deletion
behavior; destination-only objects remain untouched.

## Consequences

A volume-root copy now attempts the Recycle Bin and may therefore report
permission, source-change, or unsupported-object failures that the former
directory-wide exclusion hid. This is intentional and keeps potentially
valuable user content visible to the ordinary audit trail. Copying the
directory reproduces its filesystem tree under bigcp's normal semantics; it is
not a Windows Recycle Bin restore workflow and does not reconstruct original
paths from `$I` metadata.

## Validation

A pure case-insensitive classification regression proves both that
`$RECYCLE.BIN` is not a default exclusion and that every remaining artifact is
still excluded. The confined workspace suite protects `--include-system`,
root-only exclusion, accounting, and audit behavior without reading or writing
the machine's live Recycle Bin.
