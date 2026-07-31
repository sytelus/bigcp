# ADR 0046: Specialize WSL Plan 9 throughput

**Status:** Accepted

## Context

ADR 0045 gave every remote endpoint the same bounded two-buffer transport but
left WSL on a deliberately conservative 4 MiB/8-worker row. It also retained
the NTFS-measured rule that all small-file creates in one destination directory
use one worker. That rule prevents an NTFS directory-index convoy, but WSL's
`\\wsl.localhost` path is not NTFS: Microsoft documents that a Plan 9 file
server exposes the Linux filesystem to Windows and describes Windows access to
that filesystem as a slow cross-filesystem boundary intended for occasional
access. Serializing every create therefore exposes provider latency without
protecting a local NTFS index.

The plain-small path also set projected WSL metadata at create and again after
data because remote writes can update timestamps. Only the latter call is
authoritative, so the initial Plan 9 metadata operation had no success-path
value. A new `CREATE_NEW` handle likewise cannot be an existing reparse point
or directory; querying its type immediately after creation was redundant.

The project has no operator-approved disposable WSL path, so these mechanisms
can be correctness-tested but their speedup and optimum numeric values cannot
be measured in this change.

## Decision

- Add a stable `Wsl` transport kind. It reuses ADR 0045's ordered, bounded
  two-buffer transfer implementation, exact hashing/checkpoint semantics,
  cancellation, and memory accounting. Generic UNC continues to report
  `redirector`; local standard and same-spindle kinds are unchanged.
- Give WSL independent named Auto constants of 8 MiB and 16 workers. Their
  current equality with generic UNC is incidental, not shared policy.
- When the destination endpoint is WSL, assign plain-small jobs round-robin so
  files in one Linux directory can occupy the bounded worker pool. Local and
  generic-UNC destinations retain directory affinity. WSL-source/local-
  destination small files therefore retain the measured NTFS policy.
- Add `FILE_FLAG_SEQUENTIAL_SCAN` to WSL destination temp, resumed-temp, and
  direct-file handles. This is a cache hint only; handles remain synchronous
  and buffered and writes remain ordered.
- Defer WSL's projected last-write stamp until the existing required
  post-write step. Replacement identity validation and final stamping remain
  handle-bound. An interrupted file is still repaired by size/mtime
  reclassification on rerun.
- Skip `metadata_from_file` only for a newly created direct final name.
  `CREATE_NEW` already proves that handle is the regular file just created.
  Replacement opens still query and compare identity, kind, size, timestamp,
  attributes, and reparse tag before truncation. This shared simplification is
  safe for every endpoint and removes one provider/filesystem call.
- Keep the existing one-time `--accept-remote-paths` gate. No new prompt,
  tuning key, helper process, native Linux copy engine, IOCP path, or mutable
  runtime governor is introduced.

## Consequences

WSL policy can now evolve independently without adding WSL branches to the
generic transfer loop. Directories with many small WSL destination files can
cover multiple Plan 9 operations, and each successful new file avoids
redundant metadata work. The cost is a larger but still bounded default window:
at the default 16 MiB small-file threshold and 8 MiB chunks, each active worker
reserves at most 16 MiB of copy buffers and the coordinator reserves two 8 MiB
chunks. `mem=` continues to cap the aggregate.

This does not make a Windows process the fastest Linux-to-Linux copier. A
single WSL file handle still has synchronous request depth one, Linux metadata
outside bigcp's projected contract remains unsupported, and provider/VM/VHDX
caching can dominate. Microsoft recommends keeping files and tools in the same
filesystem for fastest work. Entirely Linux-side copies should still use
native Linux tools.

## Validation

Pure tests pin WSL profile and transport selection, stable `"wsl"` audit
serialization, WSL-destination striping versus local affinity, deferred final
stamping under a sequential handle, and all existing redirector ordering,
cancellation, checkpoint, and memory invariants. The complete confined suite
guards local and generic-UNC behavior. A performance claim requires the exact
approved WSL protocol in `docs/TESTING.md` and evidence in `BENCHMARKS.md`.

References:

- Microsoft, WSL troubleshooting (the Linux-side 9P file server exposes WSL
  files to Windows): https://learn.microsoft.com/en-us/windows/wsl/troubleshooting
- Microsoft, WSL interop (cross-filesystem access and performance boundary):
  https://learn.microsoft.com/en-us/windows/dev-environment/wsl-interop
- Microsoft, `CreateFile` caching behavior and `FILE_FLAG_SEQUENTIAL_SCAN`:
  https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
