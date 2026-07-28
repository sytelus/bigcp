# Error categories

This table mirrors `crates/core/src/error.rs`; code changes and this generated
reference must remain synchronized.

| Category | Typical condition | Resolution hint |
|---|---|---|
| `permissions` | Access denied (5). | Check/repair ACLs or ownership, then rerun. |
| `locked` | Sharing/lock violation (32/33). | Close the process using the object, then rerun. |
| `path` | Missing, invalid, or too-long path (206). | Correct it or shorten the destination root. |
| `space` | Disk full (39/112). | Free destination space, then rerun to resume. |
| `media` | CRC/device I/O error (23/1117). | Stop and check drive health first. |
| `device_gone` | Device unavailable (433/1167). | Reconnect and rerun to resume. |
| `fs_limit` | Destination lacks a required feature. | Use capable NTFS/ReFS storage. |
| `source_changed` | Source identity/size/mtime changed. | Quiesce source writers and rerun. |
| `destination_changed` | Target changed after classification or a new-name collision occurred (80/183). | Stop destination writers and rerun. |
| `unsupported_reparse` | Unknown link/filter tag. | Use `--raw-reparse` only with the owning filter and understood risk. |
| `parent_dir_failed` | Parent could not be prepared. | Resolve the parent error first. |
| `type_conflict` | File/directory/link types disagree. | Resolve manually; bigcp never deletes it. |
| `cloud` | Placeholder hydration failed. | Restore connectivity or use `--skip-cloud`. |
| `internal` | Unexpected API/state invariant. | Retain log/report and file a bug. |

Each per-object log failure carries all of: category, operation, relative path,
original Win32 code when available, OS/semantic message, and hint.
