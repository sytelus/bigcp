//! UTF-16 and Win32 error helpers shared by the wrapper modules.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};

/// Encodes an operating-system string as nul-terminated UTF-16.
pub(crate) fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Decodes UTF-16 without requiring it to be valid Unicode.
pub(crate) fn os_from_wide(value: &[u16]) -> OsString {
    OsString::from_wide(value)
}

/// Converts a failed Win32 call into an error while preserving its numeric code.
pub(crate) fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// Converts a Win32 BOOL result into an ordinary result.
pub(crate) fn bool_result(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

/// Returns a structure size as the `u32` DeviceIoControl expects.
pub(crate) fn size_u32<T>() -> io::Result<u32> {
    u32::try_from(size_of::<T>())
        .map_err(|_| io::Error::other("Win32 structure size does not fit u32"))
}
