//! Read-only physical-extent measurement for fragmentation evidence.
//!
//! Benchmark evidence must show that parallel copies do not fragment large
//! destination files (preallocation is the counter-measure, DESIGN.md). This
//! module turns that claim into a number: the count of physically discontiguous
//! disk runs backing a file, from `FSCTL_GET_RETRIEVAL_POINTERS` — a query-only
//! FSCTL that never changes the file or the volume.

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_MORE_DATA};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_GET_RETRIEVAL_POINTERS;

use crate::util::{last_error, size_u32};

/// 4 KiB query buffer: 16-byte header plus ~255 extent records per call.
const BUFFER_WORDS: usize = 512;
/// `RETRIEVAL_POINTERS_BUFFER` header: `ExtentCount` (padded) + `StartingVcn`.
const HEADER_WORDS: usize = 2;
/// Each extent record is `NextVcn` + `Lcn`.
const WORDS_PER_EXTENT: usize = 2;
/// LCN value marking a hole (sparse or compressed) rather than a disk run.
const HOLE_LCN: i64 = -1;

/// Counts the physically discontiguous disk runs of an open file.
///
/// Empty and MFT-resident files report zero extents. Holes in sparse files
/// occupy no disk run and are not counted. Two data runs count as one extent
/// when they are physically adjacent on disk (no seek lies between them),
/// which also heals artificial splits at query-buffer boundaries.
pub fn extent_count(file: &File) -> io::Result<u64> {
    let mut extents = 0_u64;
    let mut vcn_cursor = 0_i64;
    let mut expected_lcn: Option<i64> = None;
    let mut buffer = vec![0_u64; BUFFER_WORDS];
    loop {
        let mut returned = 0_u32;
        // SAFETY: the input is the documented STARTING_VCN_INPUT_BUFFER (one
        // i64), the output buffer is u64-aligned and its byte length is
        // reported exactly, the handle is live, and the call is synchronous.
        let succeeded = unsafe {
            DeviceIoControl(
                file.as_raw_handle().cast(),
                FSCTL_GET_RETRIEVAL_POINTERS,
                (&raw const vcn_cursor).cast(),
                size_u32::<i64>()?,
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len().saturating_mul(size_of::<u64>()))
                    .map_err(|_| io::Error::other("extent output buffer is too large"))?,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        let more = if succeeded == 0 {
            let error = last_error();
            match error.raw_os_error() {
                // No extents at or beyond the cursor: empty or MFT-resident
                // file, or a cursor that reached end of the extent list.
                Some(code) if code == ERROR_HANDLE_EOF.cast_signed() => break,
                Some(code) if code == ERROR_MORE_DATA.cast_signed() => true,
                _ => return Err(error),
            }
        } else {
            false
        };
        let returned_words = usize::try_from(returned)
            .map_err(|_| io::Error::other("extent byte count is too large"))?
            / size_of::<u64>();
        if returned_words < HEADER_WORDS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem returned a truncated retrieval-pointers header",
            ));
        }
        let count = usize::try_from(buffer[0] & u64::from(u32::MAX))
            .map_err(|_| io::Error::other("extent record count is too large"))?;
        let capacity = returned_words
            .saturating_sub(HEADER_WORDS)
            .checked_div(WORDS_PER_EXTENT)
            .unwrap_or(0);
        if count > capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem reported more extent records than it returned",
            ));
        }
        if count == 0 {
            if more {
                // ERROR_MORE_DATA with zero records would loop forever; treat
                // the driver contract violation as data corruption.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesystem reported more extents but returned none",
                ));
            }
            break;
        }
        let mut previous_vcn = vcn_cursor;
        for record in buffer[HEADER_WORDS..]
            .chunks_exact(WORDS_PER_EXTENT)
            .take(count)
        {
            let next_vcn = record[0].cast_signed();
            let lcn = record[1].cast_signed();
            if next_vcn <= previous_vcn {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "extent enumeration made no progress",
                ));
            }
            let clusters = next_vcn.wrapping_sub(previous_vcn);
            if lcn == HOLE_LCN {
                // A hole occupies no disk run; physical adjacency of the
                // surrounding data runs is judged purely by their LCNs.
            } else if lcn < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesystem returned a negative extent location",
                ));
            } else {
                if expected_lcn != Some(lcn) {
                    extents = extents.saturating_add(1);
                }
                expected_lcn = Some(lcn.checked_add(clusters).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "extent location overflow")
                })?);
            }
            previous_vcn = next_vcn;
        }
        vcn_cursor = previous_vcn;
        if !more {
            break;
        }
    }
    Ok(extents)
}

#[cfg(test)]
mod tests {
    use super::extent_count;
    use std::fs;

    #[test]
    fn counts_extents_of_test_owned_files_in_system_temp_sandbox() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };

        // Empty files own no disk run on any supported filesystem.
        let empty = sandbox.path().join("empty.bin");
        assert!(fs::write(&empty, b"").is_ok());
        let file = fs::File::open(&empty);
        assert!(file.is_ok());
        assert_eq!(file.ok().map(|f| extent_count(&f).ok()), Some(Some(0)));

        // A tiny file is MFT-resident on NTFS (0 extents) and a single run
        // on filesystems without resident storage.
        let tiny = sandbox.path().join("tiny.bin");
        assert!(fs::write(&tiny, b"resident").is_ok());
        let file = fs::File::open(&tiny);
        assert!(file.is_ok());
        let count = file.ok().and_then(|f| extent_count(&f).ok());
        assert!(count.is_some_and(|value| value <= 1), "{count:?}");

        // 256 KiB is always non-resident; it cannot occupy more runs than it
        // has clusters (64 at the smallest common 4 KiB cluster size).
        let data = sandbox.path().join("data.bin");
        assert!(fs::write(&data, vec![0xA5_u8; 256 * 1024]).is_ok());
        let file = fs::File::open(&data);
        assert!(file.is_ok());
        let count = file.ok().and_then(|f| extent_count(&f).ok());
        assert!(
            count.is_some_and(|value| (1..=64).contains(&value)),
            "{count:?}"
        );
    }
}
