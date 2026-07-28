# High-confidence improvements to `PLAN.md`

> **Historical design-review artifact.** These findings were used to revise
> the now-frozen `PLAN.md`; they are not the current implementation backlog.
> Consult `docs/SEMANTICS.md` for the implemented contract,
> `PLAN_DEVIATIONS.md` for open plan differences, and
> `docs/PRODUCTION_READINESS.md` for live gate status.

- **P0 — Make the work model stream-aware and include the metadata its algorithms require.** `FileEntry` does not contain `file_id`, `ea_size`, or allocation size, while replacement revalidation, EA detection, and sparse handling all require them; `DstState::Replace` contains only `old_attrs` even though commit safety needs destination ID, size, and mtime (`PLAN.md` §5.4). More seriously, scheduling uses only the unnamed-stream size. A zero-byte file can contain a 100 GB ADS, so it would be sent to the small engine, omitted from checkpoint thresholds, undercounted in progress/free-space estimates, and potentially exceed the 4 MiB buffer model. Introduce `StreamEntry{name,size,allocation_size}` and either schedule each stream independently or allow a file to be promoted to the streaming engine after stream discovery. Counters, hashes, resume keys, progress, and free-space forecasting must include all streams. Microsoft confirms that stream size and allocation size are separate per-stream values in [`FILE_STREAM_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_stream_info).

- **P0 — Correct the EA implementation around `BackupRead`/`BackupWrite` handle requirements.** The plan says EAs are copied through `BackupRead` on the already-open handle (`PLAN.md` §4.2), but streaming handles are opened with `FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING` (`PLAN.md` §5.9). Microsoft explicitly requires a synchronous handle for `BackupRead` and warns that it can fail on a no-buffering handle; using an overlapped handle may cause subtle errors ([`BackupRead` documentation](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-backupread)). Specify separate synchronous buffered handles for EA processing, parse only `BACKUP_EA_DATA`, and test directory, EFS, NTFS, and ReFS cases. This means EA copying is not always “on the already-open handle.”

- **P0 — Define a distinct sparse-file pipeline.** The plan simultaneously requests full-size `FileAllocationInfo` preallocation and promises sparse preservation by writing only allocated ranges (`PLAN.md` §4.2 and §5.9). A full allocation request conflicts with the reason for creating a sparse file. The sparse path should set `FSCTL_SET_SPARSE` first, avoid full logical-size preallocation, set logical EOF, and write only allocated ranges. It must also define how zero holes participate in offset-ordered hashing and checkpoint watermarks; otherwise the digest is not the hash of the logical file. Microsoft documents that sparse files allocate space as regions are written, while `FILE_ALLOCATION_INFO` requests a total allocation size ([sparse operations](https://learn.microsoft.com/en-us/windows/win32/fileio/sparse-file-operations), [`FILE_ALLOCATION_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_allocation_info)).

- **P0 — Snapshot the hash at the exact checkpoint watermark.** A checkpoint is permitted when `next_hash_offset >= W` (`PLAN.md` §6.3), but the active rolling hasher may already include bytes beyond `W`. Recording its current digest as `prefix_digest([0,W))` would be wrong. Associate a cloned/finalized hasher state with each contiguous write boundary or select `W` from a retained hash snapshot. Also place a checkpoint barrier around issuing writes and flushing so the exact relationship among outstanding writes, contiguous write watermark, hash snapshot, and journal record is unambiguous.

- **P0 — Add durable ownership records for every temporary file.** The journal format contains only job, checkpoint, part-done, and end records (`PLAN.md` Appendix C). That leaves no ownership record for:

  - Small replacements, which use temporary files.
  - Streamed files below the 16 GiB checkpoint threshold.
  - Large files killed before their first checkpoint.

  Yet the crash matrix calls these temps “journal-owned” and says they can be deleted safely (`PLAN.md` §7.2). Add a `part_start{rel,temp,source snapshot}` record whose durability is ordered before creating a persistent temp, or use a delete-on-close/nonpersistent temp scheme for files that are not resumable. Without this, crashes can leave permanent opaque files that bigcp may only report, contradicting the no-unexpected-files goal.

- **P0 — Make incomplete new small files structurally distinguishable from completed files.** New small files are written directly under their final names, with correctness relying on their natural size or current mtime differing after a crash (`PLAN.md` §4.3). That is not a structural guarantee: source and destination mtimes can coincide at the system clock’s effective resolution, and a power loss can leave full logical size but incomplete data. The safest design is temp+rename for all new files. If that costs too much on the primary small-file metric, the plan needs another persistent “incomplete” marker whose absence is part of `Same`; relying on an incidental timestamp mismatch is weaker than invariant I5 claims.

- **P0 — Reconcile `Same` with the promised metadata/data contract.** A `Same` result ignores ADS divergence, EA contents, and last-access time (`PLAN.md` §4.1). At minimum, compare source/destination `EaSize`, which already arrives in both enumeration records, and dispatch EA reconciliation whenever either side is nonzero. ADS has no equivalent free enumeration hint: `FILE_ID_EXTD_DIR_INFO` contains `EaSize` but no ADS indicator ([Microsoft structure definition](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_extd_dir_info)). Therefore Appendix E’s proposed test asserting “no stream calls on plain trees” is incompatible with preserving ADS by default. Either pay one stream query, make ADS preservation optional, or rename the outcome to something honest such as `skipped_metadata_match` rather than claiming the file is known correct.

- **P0 — Resolve the ReFS replacement contradiction before calling the design approved.** The plan states that POSIX rename is NTFS-only in its platform and research sections, but later requires the POSIX flag on both NTFS and ReFS and rejects any volume where that exact combination fails (`PLAN.md` §2.3, §3.2, §4.3, and §5.2). A normal supported ReFS volume must not be rejected merely because an NTFS-specific optimization is absent. Pin the exact behavior in the elevated ReFS matrix and either provide a ReFS-supported atomic replacement path or narrow the supported-filesystem requirement. Microsoft notes that `SetFileInformationByHandle` behavior is implemented by the underlying filesystem driver and can vary ([API documentation](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle)).

- **P0 — Define an explicit copyable-attribute mask and ReFS integrity policy.** The plan copies `TEMPORARY` and `OFFLINE` through `FileBasicInfo` (`PLAN.md` §4.2). Microsoft says `TEMPORARY` can cause the filesystem to defer writes because the file is expected to be deleted, while applications should not arbitrarily change `OFFLINE`. Conversely, the plan omits ReFS `INTEGRITY_STREAM` and `NO_SCRUB_DATA`, which cannot be treated as ordinary basic attributes ([file attribute documentation](https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants)). Define three groups:

  - User-copyable basics: read-only, hidden, system, archive, not-content-indexed.
  - Explicit storage features: sparse, EFS, and ReFS integrity through their dedicated APIs.
  - System/HSM-managed attributes: offline, temporary, recall, pinned/unpinned, and no-scrub—preserve only under a deliberate documented policy.

  ReFS integrity streams deserve an explicit decision because they improve corruption detection but Microsoft documents allocate-on-write, fragmentation, and latency costs on performance-sensitive workloads ([ReFS integrity streams](https://learn.microsoft.com/en-us/windows-server/storage/refs/integrity-streams)).

- **P0 — Define what happens to destination security on replacement.** Source ACL copying is intentionally out of scope (`PLAN.md` §2.4), but temp+rename replaces the destination object with a newly created object that inherited the parent directory’s security. Therefore an existing file’s custom DACL/owner can disappear even though bigcp was told not to copy source security. Decide explicitly whether replacement preserves the old destination security descriptor, resets it to inherited security, or fails when it cannot preserve it. Preserving existing destination security is the least surprising interpretation and costs extra work only on replacements.

- **P0 — Expand verification into a complete object-contract verifier.** The current verifier explicitly covers streams, EAs, creation time, and last-write time, but does not clearly require basic attributes, directory metadata, root metadata, or raw reparse payloads (`PLAN.md` §5.17). Define a matrix for files, directories, and links:

  - Unnamed and named stream sets and contents.
  - EAs.
  - Required basic attributes and timestamps.
  - Directory ADS/EAs and post-order timestamps.
  - Reparse tag and target/buffer.
  - Missing and extra objects/streams.

  The independent oracle and both verification forms should consume the same semantic matrix, while retaining independent implementation code.

- **P0 — Reject log/report/state locations inside either active tree.** The plan permits audit artifacts on the source drive and exempts configured paths from the source-write guard (`PLAN.md` §5.12 and I10). If a log is placed under `SRC`, bigcp mutates its supposedly stable source and may enumerate/copy its own growing log. If placed under `DST`, it violates destination exclusivity and appears as an extra. Allow the same physical volume, but reject `--state-dir`, `--log`, and `--report` paths located inside either source or destination root.

- **P1 — Finish the directory outcome and bounded-walk design.** The prose precisely defines tracker pending counts, but normative pseudocode still says `pending = |children|` (`PLAN.md` §6.2). Directory counters also do not say how a successfully created directory whose later ADS/EA/timestamp finalization fails is classified. Define one terminal directory outcome after `DirStamp`. In addition:

  - Enforce the metadata budget by actual allocated bytes, not the estimate that one million entries cost 200–300 MB.
  - Bound the work-stealing directory deque; a root containing one million child directories can otherwise enqueue one million tasks despite I9.
  - Explain how the beyond-budget per-name fallback detects destination extras—this requires a second pass or a bounded/disk-backed matched-name structure.

- **P1 — Define deterministic, asymmetric profile composition.** The static table contains `2–4` concurrent streams for NVMe and says the two sides are combined with “min/merge,” which is not a deterministic algorithm (`PLAN.md` §8.2). The streaming state machine already models separate `QD_src` and `QD_dst`, but the CLI exposes only one `--qd`. Specify exact profile composition for every source/destination pair, retain asymmetric QDs, and make every range resolve deterministically from documented inputs such as core count and memory. Otherwise F29’s “same devices → same settings” test cannot be written.

- **P1 — Correct the statistics/TUI model.** The plan removed the dedicated hash pool but still diagnoses `cpu-bound` from “hash pool saturated” (`PLAN.md` §5.3 and §5.14). Use process CPU utilization, IOCP completion backlog, and worker runnable time instead. Likewise, the Devices tab should say “bigcp I/O occupancy” rather than “utilization %,” since the plan correctly acknowledges that in-flight operations are not physical device utilization (`PLAN.md` §11).

- **P1 — Make the performance test volume consistent with the drive-lifespan charter.** W2 is 200 GiB per copy. Sweeping eight robocopy configurations plus bigcp, CopyFile2, and FastCopy across at least five repetitions implies roughly 11 TiB of destination writes before warmups and ceiling measurements (`PLAN.md` §8.7). That conflicts with the harmless/bounded-write charter. Separate:

  - Small recurring regression workloads.
  - Infrequent full competitor sweeps on explicitly replaceable endurance-budgeted hardware.
  - Cached competitor configuration selection so every release does not repeat the full sweep.

  Put a hard per-suite TBW ceiling in `TESTING.md`.

- **P1 — Remove unsafe-removal and endurance-heavy tests from mandatory harmless testing.** The release-gated real-hardware checklist still requires physically yanking a cable during a 100 GB write, and the plan retains a quarterly 10 TB soak (`PLAN.md` §12.5 and §12.7). Microsoft warns that unsafe removal under Better Performance risks data loss ([Windows removal-policy documentation](https://learn.microsoft.com/en-us/windows/client-management/client-tools/change-default-removal-policy-external-storage-media)). Replace the mandatory cable-yank test with fault injection or detaching a disposable VHDX. If a physical-yank experiment remains, classify it as optional destructive research on a sacrificial, reformattable device—not a harmless release test.

- **P1 — Treat the fixed competitor multipliers as stretch objectives until demonstrated.** The topology-matched ceiling methodology is now sound, but ≥3× and ≥1.3× are still unvalidated fixed release gates (`PLAN.md` §8.7). First establish repeatable baselines across the supported drive classes, then pin achievable regression floors. A correct implementation should not be unshippable because a competitor happens to perform unusually well on one controller.

- **P2 — Remove remaining stale contradictions.** At minimum:

  - Research says bigcp “matches block-clone,” while the product explicitly has no clone path (`PLAN.md` §3.1 versus §5.9).
  - Several sections still say `FlushFileBuffers` is universally honored, contradicting H5 and the honest best-effort durability contract (`PLAN.md` §8.6).
  - E37 says audit-device failure leaves copying unaffected, while the correct audit policy stops new dispatch when both log paths fail.
  - The adversarial suite still tests the withdrawn attribute-clear fallback E39 (`PLAN.md` §12.8).
  - `--dry-run` claims “zero writes,” although it produces logs/reports; say “zero destination-tree writes” (`PLAN.md` §10.1).
  - Exit code 6 is described only as an invariant breach even though it also represents fatal audit failure.
  - The CLI already exposes 23 distinct flags, conflicting with the statement that config files should be reconsidered once the surface exceeds roughly 20.


Recommended: simplify low-level tuning flags. Replace --qd, --chunk, --streams, and several worker-count controls with a small set such as --profile auto|hdd|usb-ssd|sata-ssd|nvme plus one clearly advanced override mechanism. This reduces invalid combinations and makes support reports reproducible without eliminating expert control. By default plan should assume auto style defaults as users may not be expert to specify any of these explicitly.

Recommended: replace the fixed 3×/1.3× release requirements with evidence-derived per-class targets. Keep the multipliers as aspirational KPIs. Make release gates relative to the implementation’s established rolling baseline and the best competitor result actually observed on each supported topology.

Recommended: distinguish logical-content fidelity from destination storage policy on ReFS. Rather than copying source integrity/no-scrub state, let the destination directory/volume policy control ReFS integrity streams and report the resulting state. This avoids unexpectedly enabling an allocate-on-write performance cost merely because the source used it.

Consider allowing bounded passive adaptation from real copy traffic. Static profiles simplify implementation, but USB bridges, SSD firmware, HDD zones, thermal states, and source/destination asymmetry vary too much for one class table to maximize throughput consistently. A limited controller that adjusts only QD/stream count within safe bounds—using traffic already occurring, with no probe I/O—would better serve the throughput objective while remaining deterministic enough to log and test.

Recommended for throughput: make checkpoint watermarks tentative rather than destination-durable. Because every resumed prefix is re-read and hashed, the journal can safely be ahead of data that survived a power loss: a short or mismatching temp simply restarts from zero. Removing periodic FlushFileBuffers(temp) would eliminate potentially expensive device-cache flushes while preserving integrity. The altered promise would be “resume near the checkpoint after process termination; after power/device loss, verify and resume when possible, otherwise restart safely.”
