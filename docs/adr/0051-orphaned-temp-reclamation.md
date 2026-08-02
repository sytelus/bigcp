# ADR 0051: Orphaned resume-temporary reclamation

**Status:** Accepted

## Context

A checkpointed large-file partial persists on purpose as an opaque
`.bigcp-{run}-{nonce}.part` destination sibling so a rerun can resume it.
That partial became a permanent destination resident whenever its checkpoint
turned unreachable: (a) the source file was deleted before the rerun, (b)
`--fresh` truncated the journal, or (c) a job-signature change cleared the
resume hints. Every rerun then reported bigcp's own leftover as a destination
"extra" forever. VISION pulls in both directions: the tool "should also not
produce any extra files that user didn't expect", yet must "never delete
files … when they also didn't exist in source". The README FAQ already stated
the resolution: anything the journal can prove bigcp created may be
reclaimed; anything unproven is reported, never deleted.

## Decision

Before any deletion, all four parts of the proof must hold:

1. The destination-only child name is opened through
   `bigcp_win::DestinationTemp::resume`, which structurally enforces bigcp's
   opaque temp shape, rejects `:` (alternate-stream escapes), opens the final
   component non-following with DELETE access, and requires an ordinary file.
2. This destination's journal carries a checkpoint whose `temp_name` equals
   the child name and whose `temp_identity` is present. Identity-less
   version-one records are never reclaimable — they cannot be proven.
3. The opened handle's filesystem identity equals the recorded
   `CheckpointFileIdentity`, proving it is the very object bigcp created.
   Deletion is `DestinationTemp::discard` — delete-on-close through that
   verified handle, never a path-based delete, so there is no TOCTOU window.
4. Never in dry-run: dry-run opens no journal, so no candidate can classify,
   preserving the zero-destination-write promise.

The journal supplies the proof through two channels. Live resumable
checkpoints whose source file is absent classify directly (and are retired
with `PartDone` after a successful discard; a failed retirement append
degrades checkpointing exactly like every other journal append failure).
Checkpoints invalidated wholesale — `--fresh` truncation (harvested before
the truncate, with a load failure skipping the harvest rather than failing
`--fresh`) and job-signature clears (both at replay and at `begin_job`) — are
harvested into reclaimable-temp records instead of being dropped silently. A
checkpoint speaks only for the directory holding its final path, so a
same-named object elsewhere can neither claim nor consume the proof.

The extras scan in `enter_directory` applies a three-way decision table,
evaluated against the join-time journal state (the entry loop may consume a
live checkpoint before extras accounting runs): LIVE (a source child will
consume the temp) is skipped silently; a proven ORPHAN is reclaimed and
surfaced as a `temp_reclaimed` warning, never counted as an extra; everything
UNKNOWN — including identity mismatches and any open failure — takes exactly
the ordinary extra accounting and is never deleted.

## Consequences

Extras stay truthful in both directions: bigcp's own provable leftovers no
longer inflate the count on every rerun, and user files that merely resemble
temps remain reported, byte-for-byte untouched. A reclaim failure of any kind
falls back to reporting, so the failure mode is always "extra file survives",
never "wrong file deleted". Reclamation adds no I/O to runs whose journal
holds no temp records.

## Validation

`orphaned_temp_with_deleted_source_is_reclaimed` (deleted source: reclaim,
warning, zero extras, clean rerun),
`temp_name_reuse_with_wrong_identity_is_never_deleted` (identity mismatch:
file survives as a reported extra),
`fresh_run_reclaims_the_previous_partial` (`--fresh`: full recopy plus
reclaim), journal unit tests
`fresh_open_harvests_identity_bearing_checkpoints_only`,
`job_signature_change_moves_checkpoints_to_reclaimable`, and
`reclaimable_take_requires_the_recording_directory`, while the unchanged
`verified_checkpoint_resume_completes_an_interrupted_large_file` pins that
live resume is unaffected.
