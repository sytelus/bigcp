//! Safe wrappers for sparse allocation-range discovery and destination setup.

use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FILE_ALLOCATED_RANGE_BUFFER, FILE_SET_SPARSE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES,
    FSCTL_SET_SPARSE,
};

use crate::util::{bool_result, last_error};

const RANGE_BATCH: usize = 1024;

/// One allocated logical byte interval in a sparse source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatedRange {
    /// Inclusive logical starting offset.
    pub offset: u64,
    /// Number of logical bytes in the interval.
    pub length: u64,
}

/// Queries every allocated range covering `[0, logical_size)`.
pub(crate) fn query_allocated_ranges(
    file: &File,
    logical_size: u64,
) -> io::Result<Vec<AllocatedRange>> {
    if logical_size == 0 {
        return Ok(Vec::new());
    }
    let mut ranges = Vec::new();
    let mut cursor = 0_u64;
    while cursor < logical_size {
        let input = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: i64::try_from(cursor)
                .map_err(|_| io::Error::other("sparse offset exceeds i64"))?,
            Length: i64::try_from(logical_size - cursor)
                .map_err(|_| io::Error::other("sparse length exceeds i64"))?,
        };
        let mut output = vec![FILE_ALLOCATED_RANGE_BUFFER::default(); RANGE_BATCH];
        let mut returned = 0_u32;
        // SAFETY: input and output point to exact documented structures, the
        // file handle is live, and the operation is synchronous.
        let succeeded = unsafe {
            DeviceIoControl(
                file.as_raw_handle().cast(),
                FSCTL_QUERY_ALLOCATED_RANGES,
                (&raw const input).cast(),
                size_u32::<FILE_ALLOCATED_RANGE_BUFFER>()?,
                output.as_mut_ptr().cast(),
                u32::try_from(output.len() * size_of::<FILE_ALLOCATED_RANGE_BUFFER>())
                    .map_err(|_| io::Error::other("sparse output buffer is too large"))?,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        let more = if succeeded == 0 {
            let error = last_error();
            if error.raw_os_error() != Some(ERROR_MORE_DATA.cast_signed()) {
                return Err(error);
            }
            true
        } else {
            false
        };
        let record_size = size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        let returned = usize::try_from(returned)
            .map_err(|_| io::Error::other("sparse range byte count is too large"))?;
        if returned % record_size != 0 || returned / record_size > output.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem returned malformed sparse ranges",
            ));
        }
        let count = returned / record_size;
        if count == 0 {
            break;
        }
        for raw in &output[..count] {
            let offset = u64::try_from(raw.FileOffset).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "negative sparse range offset")
            })?;
            let length = u64::try_from(raw.Length).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "negative sparse range length")
            })?;
            let end = offset.checked_add(length).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "sparse range overflow")
            })?;
            if length == 0 || offset < cursor || end > logical_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesystem returned an invalid sparse range",
                ));
            }
            ranges.push(AllocatedRange { offset, length });
        }
        let Some(last) = ranges.last() else {
            break;
        };
        let next = last.offset.saturating_add(last.length);
        if next <= cursor {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sparse range enumeration made no progress",
            ));
        }
        cursor = next;
        if !more {
            break;
        }
    }
    Ok(ranges)
}

/// Marks a destination handle sparse before its logical EOF is established.
pub(crate) fn mark_sparse(file: &File) -> io::Result<()> {
    let input = FILE_SET_SPARSE_BUFFER { SetSparse: true };
    let mut returned = 0_u32;
    // SAFETY: input has the documented structure, the handle is live, and the
    // call has no output buffer.
    unsafe {
        bool_result(DeviceIoControl(
            file.as_raw_handle().cast(),
            FSCTL_SET_SPARSE,
            (&raw const input).cast(),
            size_u32::<FILE_SET_SPARSE_BUFFER>()?,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        ))
    }
}

fn size_u32<T>() -> io::Result<u32> {
    u32::try_from(size_of::<T>())
        .map_err(|_| io::Error::other("Win32 structure size does not fit u32"))
}
