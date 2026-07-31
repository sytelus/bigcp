//! UTF-16 and Win32 error helpers shared by the wrapper modules.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};

/// Encodes an operating-system string as NUL-terminated UTF-16.
///
/// Win32 path APIs treat the first NUL as the end of the name. Rejecting an
/// embedded NUL prevents a safe wrapper from validating or reporting one path
/// while the kernel silently operates on a shorter one.
pub(crate) fn wide_null(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut encoded = value.encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Win32 string contains an embedded NUL",
        ));
    }
    encoded.push(0);
    Ok(encoded)
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use super::wide_null;

    #[test]
    fn win32_strings_reject_embedded_nul_without_truncation() {
        let value = OsString::from_wide(&[
            u16::from(b'C'),
            u16::from(b':'),
            u16::from(b'\\'),
            u16::from(b'a'),
            0,
            u16::from(b'b'),
        ]);
        let error = wide_null(&value).err();
        assert!(error.is_some_and(|value| value.kind() == std::io::ErrorKind::InvalidInput));
    }
}
