# bigcp — Known Limitations

Every limitation below is **deliberate** — a scope decision from `VISION.md` or an engineering trade-off from `PLAN.md` — not an accident. Each entry says what the limitation is, why it exists, and what to do about it when it matters. Section references point into `PLAN.md`.

## Platform and environment

- **Windows 11 22H2 or later only.** Older Windows versions are unsupported by design; the tool assumes modern APIs unconditionally and carries no OS-version fallbacks (§2.3). *Workaround: use robocopy on older systems.*
- **NTFS and ReFS volumes only, source and destination.** exFAT, FAT32, UDF, and any other filesystem are rejected pre-flight with a clear error (§4.4, F15). This decision is what makes exact timestamp comparison, guaranteed file IDs, atomic POSIX renames, and a single code path possible. *Workaround: reformat external drives to NTFS, or use robocopy for legacy media.*
- **Local volumes only.** Network (UNC) paths and mapped drives to shares are rejected pre-flight with a clear error (§4.5, F27). Network copying brings SMB semantics the tool deliberately does not own. *Workaround: copy to a local staging drive, or use robocopy for network legs.*
- **x64 only at v1.** ARM64 is a stretch goal; nothing in the design precludes it, but it is not built or tested (§2.3).
- **Elevation confers nothing.** There is no backup-privilege mode: files the current user cannot read fail with a repair hint (fix ACLs / take ownership) rather than being force-copied (§5.13, F-table). This keeps the privilege model trivial.

## What is not copied or preserved

- **No ACLs, owner, or auditing information copied from the source.** Per the `/COPY:DTA` contract, destination files receive default inherited security (§4.2). One exception protects *destination* data: when replacing an existing destination file that carries explicit (non-inherited) DACL protection, that protection is preserved across the replacement — owner and SACL are not (privilege-gated). If preservation fails, the replacement fails rather than silently downgrading protection (§4.3). *Use robocopy `/COPY:S` workflows if source-security copying is required.*
- **System- and storage-managed attributes are never copied.** `TEMPORARY`, `OFFLINE`, cloud-recall/pinned flags, and `NO_SCRUB_DATA` are owned by the filesystem/HSM/cloud filters — setting them blind changes system behavior. Only the user-copyable mask (`READONLY, HIDDEN, SYSTEM, ARCHIVE, NOT_CONTENT_INDEXED`) transfers (§4.2).
- **ReFS integrity streams follow destination policy, not the source.** A file copied onto ReFS gets whatever integrity setting the destination directory/volume dictates; bigcp neither replicates nor overrides the source's setting (integrity has allocate-on-write costs the destination owner should control). The resulting state is logged when it differs (§4.2).
- **NTFS compression state is not carried over.** Content is copied (decompressed on read); the destination file is stored uncompressed even on NTFS. Re-compressing costs throughput for a storage-layout attribute, and VISION explicitly drops it (§4.2, F22). Compressed-source counts appear in the report.
- **Hard links are not preserved, detected, or reported.** Each linked name copies as an independent full file (robocopy's default behaves the same). Consequence: a heavily hard-linked tree occupies more space at the destination than at the source (§4.2, §5.6). 
- **EFS-encrypted files are copied as plaintext content.** bigcp reads through EFS (getting plaintext) and asks the destination to re-encrypt; where it cannot, the file lands unencrypted with a per-file `efs_downgrade` warning (§4.2).
- **Sparse preservation is best-effort.** On destinations without sparse support the file is expanded dense (with an up-front space check); a dense copy is defined as *correct* — sparseness is a storage optimization, not content (§4.1, §4.2).
- **Unknown third-party reparse points fail by default.** Only symlinks and junctions are recreated. HSM/ProjFS/App-Exec-Link and other filter-owned tags fail as `unsupported_reparse` because a verbatim buffer without its owning filter driver is not meaningfully a copy; `--raw-reparse` opts into verbatim copying at the user's risk (§4.6, E31).
- **Symlink and junction targets are copied verbatim, never rewritten.** Absolute targets keep pointing at their original absolute location — a link that made sense on the source machine may dangle at the destination (§4.6). Symlink creation requires Developer Mode or the symlink privilege; otherwise each symlink fails with a hint.
- **Cloud placeholders (OneDrive etc.) hydrate by default.** Copying a dehydrated placeholder downloads it; large placeholder trees can consume significant bandwidth and disk. The count is reported prominently, and `--skip-cloud` excludes them instead (§4.6). Placeholders are never copied as raw reparse points (that corrupts them off-volume).
- **Root-level OS artifacts are excluded by default** (`$RECYCLE.BIN`, `System Volume Information`, page/hibernation files). Every exclusion is notified (banner, log, summary); `--include-system` restores them (§4.7, F17).

## Copy-semantics trade-offs

- **The skip heuristic is size + last-write time, not content.** A destination file with identical size and mtime is skipped without reading it. Content that changed while size and mtime were preserved (deliberate tampering, or a program that restores timestamps) is not detected at copy time — this is the industry-standard trade (robocopy/rsync/rclone) for zero-I/O re-runs. *Standalone `bigcp verify` catches it* (§4.1, §5.17, E-catalog).
- **ADS/EA divergence on an otherwise-identical file is not detected at copy time.** Checking streams on every skipped file would cost a per-file query for a vanishingly rare case (F11/F21). *Standalone `bigcp verify` compares full stream sets and EA blobs* (§4.1 scope note, E43).
- **Last-access time is set but never compared or strictly verified.** Reading a file rewrites its atime; using it as an equality key would cause perpetual re-copy churn. Verification reports it separately as informational (§4.1, §5.17).
- **Destination files not present in the source are never touched — or removed.** bigcp has no mirror/purge capability *by design* (it structurally cannot delete user files). Consequence: re-running against an evolving source accumulates stale extra files at the destination. Extras are counted and sampled in the report (§4.1, §2.4).
- **Names differing only by case collide.** The destination join is case-insensitive (Windows semantics). Two source files distinguishable only by case (possible in WSL case-sensitive directories) are detected as a duplicate-key conflict and reported as errors rather than silently overwriting each other (§4.5, E26).
- **Type conflicts are errors, not resolutions.** A destination directory where the source has a file (or vice versa, or an unexpected reparse point) fails with `type_conflict`; bigcp will not delete or write through the conflicting object — the user resolves it manually (§4.1, E27/E36).

## Durability and verification caveats

- **Default completion is logical, not power-loss durable.** "Copied" means data written and acknowledged, renamed, metadata set — the same contract as robocopy and Explorer. Recently completed files may still sit in OS or drive caches; power loss can lose them. This is always *detectable and repaired on the next run* (torn files never masquerade as complete). `--flush` upgrades to per-file durable completion (§7.5). There is no volume-level flush (VISION).
- **Even `--flush` is bounded by hardware honesty.** Some USB bridges and drives do not fully honor cache-flush semantics; bigcp reports what it did, not what the hardware guarantees (§7.5, H5).
- **Verification defeats the OS cache, not the drive's internal cache entirely.** Read-back verify catches bus, filesystem, and logic corruption reliably; media-decay detection is only as good as the drive allows (§5.17).
- **xxh3-128 is not cryptographic.** Digest equality is overwhelming statistical evidence against accidental corruption and omissions — the VISION threat model — but offers no protection against a deliberate adversary crafting collisions. Tamper-evidence is explicitly out of scope (§5.11, F30).
- **Final names never hold partial data — but crash artifacts can still exist briefly.** Every file lands via temp+atomic-rename, and in-flight temps self-delete when a killed process's handles close; only checkpointed resume partials persist deliberately, and only a *power loss* (no handle close) can strand other temps — cleaned on the next run when provably bigcp's (embedded run-id + journal); anything unprovable is reported, never auto-deleted (§4.3, §5.12, §7.2).
- **Checkpoint watermarks are tentative.** No destination flush accompanies a checkpoint (a deliberate throughput choice); after a power loss the journal may describe more data than survived. This is safe because resume *always* re-reads and digest-verifies the partial: after process termination you resume near the checkpoint; after power/device loss you resume if the data survived verification, otherwise the file restarts safely (§5.12).
- **The replacement commit has a microsecond race window.** Windows offers no compare-and-rename; the target is revalidated immediately before the atomic rename, but an object swapped in between check and rename in that window would be replaced. The destination-exclusivity assumption (F16) covers this residue (§4.3).

## Concurrency and stability assumptions

- **Both trees are assumed exclusive and stable during a run.** Concurrent writers are the user's rule violation, not a supported mode (no VSS). Violations are cheaply detected where possible — vanished/changed sources, changed destination targets, reparse swaps at examination time — and fail those files; mid-run races inside the detection gaps are not guaranteed caught (§4.8, §4.5, F16).
- **In-use files fail immediately.** No retries, no waiting: a locked file is reported with the locking process's name (Restart Manager) and addressed by re-running after closing it (§5.13).
- **One run per exact destination root per machine.** A second run on the same root is refused (machine-wide lock). Nested or overlapping destination roots are *not* detected — that situation falls under the exclusivity assumption (§5.12, F26/E33).
- **Abort-and-rerun is the only recovery model.** On device disconnect or fatal conditions the run stops resumably; there is no in-run reconnect flow. Resume is cheap by design (skip heuristic + verified checkpoints), which is what makes this acceptable (§5.13, F31).

## Performance boundaries

- **Tuning is static per drive class, with one narrow exception.** Settings come from class profiles chosen at startup (`--profile`, `--tune` to override). A bounded passive governor may adjust only queue depth and stream count, only within profile-declared safe bounds, using only signals from real traffic — chiefly to back off misbehaving USB bridges. Chunk sizes, buffers, workers, and thresholds never change mid-run; every governor move is logged with its reason (§6.5, §8.2, F29).
- **Same-volume ReFS copies do not block-clone.** bigcp always streams; on Dev-Drive-style same-volume duplication the OS copy engines (Explorer/robocopy) are dramatically faster and bigcp says so in a hint rather than competing (§5.9, F28).
- **Very large single directories degrade.** Directories beyond the ~1 M-entry optimization target fall back to per-name probing — correct, bounded memory, slower (§5.6, F33).
- **The report's ceiling figure is "best observed sustained throughput".** It is the best sustained window measured during the run — a labeled, honest measurement, not a theoretical device limit; no device probing exists (§5.14, VISION's own terminology).
- **Bottleneck verdicts are confidence-rated hypotheses.** They derive from application-side I/O occupancy, which approximates but does not equal physical device utilization (§5.14).
- **Some hardware behavior is invisible.** DM-SMR drives cannot be reliably detected in software (their write-collapse is inferred from throughput signatures); USB bridges frequently fail or misreport capability IOCTLs (profiles fall back conservatively); removal-policy readings are inferences (§3.4, §5.5).
- **Free-space forecasting is approximate.** The early warning uses a conservative range (cluster rounding, replacement double-occupancy); the authoritative stop is the actual disk-full breaker (§5.5, E15).
- **Same-spindle HDD copies are inherently slow.** Alternating-burst mode amortizes seeks but cannot eliminate the physics of one head serving reads and writes (§8.3).

## Filesystem-capability degradation (NTFS ↔ ReFS deltas)

- **ReFS versions vary in capability.** Where a destination volume reports no support for named streams or EAs (capability flags, not FS names), those items are dropped with per-file warnings and counted — never silently; the file still counts as copied-with-warnings (§4.4). EFS files land decrypted on ReFS (`efs_downgrade` warning) since ReFS has no EFS.
- All FAT-family limitations (4 GiB cap, timestamp granularity, DST shifts, missing file IDs, no journal) are gone from this list because those filesystems are **rejected outright** rather than degraded — see the scope section above.

## Reporting and audit

- **A run that can no longer write its log stops.** After reopen and failover attempts fail, bigcp refuses to continue un-audited (drains in-flight work, exits with the audit-failure code) — claims without a log would be unverifiable (§5.15, I7).
- **Audit artifacts may never live inside either tree.** `--state-dir`, `--log`, and `--report` paths under the source or destination root are rejected pre-flight (same volume is fine) — a log inside SRC would mutate the "stable" source and be copied by its own run (§5.12, E46).
- **Error samples in the report are capped** (first N per category, plus counts); the JSONL log always holds every event (§10.3).
