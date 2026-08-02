# Error categories

This table is a hand-maintained mirror of `crates/core/src/error.rs` (no
generator exists yet — PLAN §14.1's generated form is release work); any change
to the code table must update this file in the same commit. Optional
capability shortfalls surface as explicit warnings; hard representation limits
use `fs_limit` before destination mutation.

| Category | Typical condition | Resolution hint |
|---|---|---|
| `permissions` | Access denied (5). | Check/repair ACLs or ownership, then rerun. |
| `locked` | Sharing/lock violation (32/33). | Close the process using the object, then rerun. |
| `path` | Missing, invalid, or too-long path (206). | Correct it or shorten the destination root. |
| `space` | Disk full (39/112). | Free destination space, then rerun to resume. |
| `media` | CRC/device I/O error (23/1117). | Stop and check drive health first. |
| `device_gone` | Local device unavailable (21/55/433/1167) or remote redirector/share disconnected (53/59/64/67/121/1222/1231/1236/2250). | Reconnect the device, share, or WSL distribution and rerun to resume. |
| `fs_limit` | A required object cannot be represented (for example a link on FAT/exFAT/WSL) or a file exceeds FAT's 4,294,967,295-byte limit. | Use a destination that represents the object, or omit the unsupported source object. |
| `source_changed` | Source identity/size/mtime changed. | Quiesce source writers and rerun. |
| `destination_changed` | Target changed after classification or during verification, or a new-name collision occurred (80/183). | Stop destination writers and rerun. |
| `unsupported_reparse` | Unknown link/filter tag. | Use `--raw-reparse` only with the owning filter and understood risk. |
| `parent_dir_failed` | Parent could not be prepared. | Resolve the parent error first. |
| `type_conflict` | File/directory/link types disagree. | Resolve manually; bigcp never deletes it. |
| `cloud` | A Win32 `ERROR_CLOUD_FILE_*` operation failed (for example provider not running 362, authentication 386, network unavailable 388, provider terminated 404, request timeout 426, or provider message timeout 475). | Restore cloud connectivity/provider health or use `--skip-cloud`. |
| `internal` | Unexpected API/state invariant. | Retain log/report and file a bug. |

Each per-object log failure carries all of: category, operation, relative path,
original Win32 code when available, OS/semantic message, and hint.
