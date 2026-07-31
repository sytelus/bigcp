//! UTF-16 and Win32 error helpers shared by the wrapper modules.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};

use windows_sys::Win32::Foundation::{
    ERROR_CLOUD_FILE_ACCESS_DENIED, ERROR_CLOUD_FILE_ALREADY_CONNECTED,
    ERROR_CLOUD_FILE_AUTHENTICATION_FAILED, ERROR_CLOUD_FILE_CONNECTED_PROVIDER_ONLY,
    ERROR_CLOUD_FILE_DEHYDRATION_DISALLOWED, ERROR_CLOUD_FILE_IN_USE,
    ERROR_CLOUD_FILE_INCOMPATIBLE_HARDLINKS, ERROR_CLOUD_FILE_INSUFFICIENT_RESOURCES,
    ERROR_CLOUD_FILE_INVALID_REQUEST, ERROR_CLOUD_FILE_METADATA_CORRUPT,
    ERROR_CLOUD_FILE_METADATA_TOO_LARGE, ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE,
    ERROR_CLOUD_FILE_NOT_IN_SYNC, ERROR_CLOUD_FILE_NOT_SUPPORTED,
    ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT, ERROR_CLOUD_FILE_PINNED,
    ERROR_CLOUD_FILE_PROPERTY_BLOB_CHECKSUM_MISMATCH, ERROR_CLOUD_FILE_PROPERTY_BLOB_TOO_LARGE,
    ERROR_CLOUD_FILE_PROPERTY_CORRUPT, ERROR_CLOUD_FILE_PROPERTY_LOCK_CONFLICT,
    ERROR_CLOUD_FILE_PROPERTY_VERSION_NOT_SUPPORTED, ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING,
    ERROR_CLOUD_FILE_PROVIDER_TERMINATED, ERROR_CLOUD_FILE_READ_ONLY_VOLUME,
    ERROR_CLOUD_FILE_REQUEST_ABORTED, ERROR_CLOUD_FILE_REQUEST_CANCELED,
    ERROR_CLOUD_FILE_REQUEST_TIMEOUT, ERROR_CLOUD_FILE_SYNC_ROOT_METADATA_CORRUPT,
    ERROR_CLOUD_FILE_TOO_MANY_PROPERTY_BLOBS, ERROR_CLOUD_FILE_UNSUCCESSFUL,
    ERROR_CLOUD_FILE_US_MESSAGE_TIMEOUT, ERROR_CLOUD_FILE_VALIDATION_FAILED,
};

const CLOUD_FILE_ERRORS: &[u32] = &[
    ERROR_CLOUD_FILE_ACCESS_DENIED,
    ERROR_CLOUD_FILE_ALREADY_CONNECTED,
    ERROR_CLOUD_FILE_AUTHENTICATION_FAILED,
    ERROR_CLOUD_FILE_CONNECTED_PROVIDER_ONLY,
    ERROR_CLOUD_FILE_DEHYDRATION_DISALLOWED,
    ERROR_CLOUD_FILE_INCOMPATIBLE_HARDLINKS,
    ERROR_CLOUD_FILE_INSUFFICIENT_RESOURCES,
    ERROR_CLOUD_FILE_INVALID_REQUEST,
    ERROR_CLOUD_FILE_IN_USE,
    ERROR_CLOUD_FILE_METADATA_CORRUPT,
    ERROR_CLOUD_FILE_METADATA_TOO_LARGE,
    ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE,
    ERROR_CLOUD_FILE_NOT_IN_SYNC,
    ERROR_CLOUD_FILE_NOT_SUPPORTED,
    ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT,
    ERROR_CLOUD_FILE_PINNED,
    ERROR_CLOUD_FILE_PROPERTY_BLOB_CHECKSUM_MISMATCH,
    ERROR_CLOUD_FILE_PROPERTY_BLOB_TOO_LARGE,
    ERROR_CLOUD_FILE_PROPERTY_CORRUPT,
    ERROR_CLOUD_FILE_PROPERTY_LOCK_CONFLICT,
    ERROR_CLOUD_FILE_PROPERTY_VERSION_NOT_SUPPORTED,
    ERROR_CLOUD_FILE_PROVIDER_NOT_RUNNING,
    ERROR_CLOUD_FILE_PROVIDER_TERMINATED,
    ERROR_CLOUD_FILE_READ_ONLY_VOLUME,
    ERROR_CLOUD_FILE_REQUEST_ABORTED,
    ERROR_CLOUD_FILE_REQUEST_CANCELED,
    ERROR_CLOUD_FILE_REQUEST_TIMEOUT,
    ERROR_CLOUD_FILE_SYNC_ROOT_METADATA_CORRUPT,
    ERROR_CLOUD_FILE_TOO_MANY_PROPERTY_BLOBS,
    ERROR_CLOUD_FILE_UNSUCCESSFUL,
    ERROR_CLOUD_FILE_US_MESSAGE_TIMEOUT,
    ERROR_CLOUD_FILE_VALIDATION_FAILED,
];

/// Reports whether a raw Win32 error belongs to the Cloud Files error family.
///
/// These values are not contiguous, so callers must not classify them with a
/// numeric range or recognize only the most common provider-not-running code.
#[must_use]
pub fn is_cloud_file_error(code: i32) -> bool {
    CLOUD_FILE_ERRORS.contains(&code.cast_unsigned())
}

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

    use super::{CLOUD_FILE_ERRORS, is_cloud_file_error, wide_null};

    #[test]
    fn cloud_file_error_family_is_complete_and_does_not_use_a_range() {
        assert!(
            CLOUD_FILE_ERRORS
                .iter()
                .all(|code| is_cloud_file_error(code.cast_signed()))
        );
        assert!(!is_cloud_file_error(359));
        assert!(!is_cloud_file_error(-1));
    }

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
