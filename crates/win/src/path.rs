//! Native Windows path normalization and identity-safe comparison.
//!
//! All filesystem calls below the CLI boundary receive extended-length
//! absolute paths. Names stay as UTF-16, so unpaired surrogates are not lost.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Globalization::{LCMAP_UPPERCASE, LCMapStringEx, LOCALE_NAME_INVARIANT};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, GetFullPathNameW, VOLUME_NAME_DOS,
};

use crate::util::{last_error, os_from_wide, wide_null};

const EXTENDED_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
const UNC_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16];
const EXTENDED_UNC_PREFIX: &[u16] = &[
    b'\\' as u16,
    b'\\' as u16,
    b'?' as u16,
    b'\\' as u16,
    b'U' as u16,
    b'N' as u16,
    b'C' as u16,
    b'\\' as u16,
];

/// Resolves a path lexically and converts it to Win32 extended-length form.
///
/// This function deliberately does not follow reparse points. Root identity is
/// established separately by opening the path and calling final_path.
pub fn absolute_extended(path: &Path) -> io::Result<PathBuf> {
    let input = wide_null(path.as_os_str());
    // SAFETY: input is nul-terminated and remains alive for both calls. A null
    // output with length zero is the documented size query.
    let required = unsafe {
        GetFullPathNameW(
            input.as_ptr(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if required == 0 {
        return Err(last_error());
    }

    let mut buffer = vec![0_u16; required as usize];
    // SAFETY: buffer has the size returned by the preceding API call; all
    // pointers remain valid for the duration of the call.
    let written = unsafe {
        GetFullPathNameW(
            input.as_ptr(),
            required,
            buffer.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if written == 0 {
        return Err(last_error());
    }
    buffer.truncate(written as usize);

    if starts_with_ascii_case_insensitive(&buffer, EXTENDED_UNC_PREFIX) {
        buffer = canonicalize_wsl_server(buffer);
    } else if starts_with(&buffer, UNC_PREFIX) && !starts_with(&buffer, EXTENDED_PREFIX) {
        // Extended UNC syntax is `\\?\UNC\server\share`, not a plain
        // `\\?\` prefix followed by the ordinary UNC string.
        let mut extended = Vec::with_capacity(EXTENDED_UNC_PREFIX.len() + buffer.len() - 2);
        extended.extend_from_slice(EXTENDED_UNC_PREFIX);
        extended.extend_from_slice(&buffer[UNC_PREFIX.len()..]);
        buffer = canonicalize_wsl_server(extended);
    } else if !starts_with(&buffer, EXTENDED_PREFIX) {
        let mut extended = Vec::with_capacity(EXTENDED_PREFIX.len() + buffer.len());
        extended.extend_from_slice(EXTENDED_PREFIX);
        extended.extend_from_slice(&buffer);
        buffer = extended;
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

/// Returns the final DOS path for an already-open file or directory handle.
///
/// Reparse points in the path are resolved by Windows. The result is suitable
/// for root alias and nesting checks.
pub fn final_path(file: &File) -> io::Result<PathBuf> {
    let raw = file.as_raw_handle();
    let flags = FILE_NAME_NORMALIZED | VOLUME_NAME_DOS;
    // SAFETY: the borrowed file owns a valid handle for the entire call.
    let required = unsafe { GetFinalPathNameByHandleW(raw.cast(), std::ptr::null_mut(), 0, flags) };
    if required == 0 {
        return Err(last_error());
    }
    let mut buffer = vec![0_u16; required as usize + 1];
    // SAFETY: the buffer is writable and larger than the size returned above.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            raw.cast(),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len())
                .map_err(|_| io::Error::other("final path buffer exceeds u32"))?,
            flags,
        )
    };
    let written_usize = usize::try_from(written)
        .map_err(|_| io::Error::other("final path length exceeds address space"))?;
    if written == 0 || written_usize >= buffer.len() {
        return Err(last_error());
    }
    buffer.truncate(written_usize);
    buffer = canonicalize_wsl_server(buffer);
    Ok(PathBuf::from(os_from_wide(&buffer)))
}

/// Produces the Windows ordinal, case-insensitive join key for a name.
pub fn ordinal_case_key(name: &OsStr) -> io::Result<Vec<u16>> {
    let source: Vec<u16> = name.encode_wide().collect();
    if source.is_empty() {
        return Ok(Vec::new());
    }
    let source_len = i32::try_from(source.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name is too long"))?;
    // SAFETY: all pointers reference source, which remains live. A null
    // destination with zero length is the documented sizing operation.
    let required = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            source.as_ptr(),
            source_len,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if required == 0 {
        return Err(last_error());
    }
    let required_usize = usize::try_from(required)
        .map_err(|_| io::Error::other("case mapping size was negative"))?;
    let mut mapped = vec![0_u16; required_usize];
    // SAFETY: the output buffer has exactly the size requested by Windows.
    let written = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            source.as_ptr(),
            source_len,
            mapped.as_mut_ptr(),
            required,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if written == 0 {
        return Err(last_error());
    }
    mapped.truncate(
        usize::try_from(written)
            .map_err(|_| io::Error::other("case mapping result was negative"))?,
    );
    Ok(mapped)
}

/// Produces an exact or Windows-ordinal key for endpoint-aware matching.
pub fn comparison_key(name: &OsStr, case_sensitive: bool) -> io::Result<Vec<u16>> {
    if case_sensitive {
        Ok(name.encode_wide().collect())
    } else {
        ordinal_case_key(name)
    }
}

/// Returns true when candidate is root or is nested beneath root.
///
/// Both inputs must already be absolute final paths. Component comparison uses
/// the same Windows ordinal case folding as the destination join.
pub fn is_same_or_descendant(candidate: &Path, root: &Path) -> io::Result<bool> {
    is_same_or_descendant_with(candidate, root, false)
}

/// Endpoint-aware form of [`is_same_or_descendant`].
pub fn is_same_or_descendant_with(
    candidate: &Path,
    root: &Path,
    case_sensitive: bool,
) -> io::Result<bool> {
    let candidate_key = comparison_key(candidate.as_os_str(), case_sensitive)?;
    let mut root_key = comparison_key(root.as_os_str(), case_sensitive)?;
    while root_key
        .last()
        .is_some_and(|value| *value == u16::from(b'\\'))
    {
        root_key.pop();
    }
    if candidate_key == root_key {
        return Ok(true);
    }
    if candidate_key.len() <= root_key.len() || !candidate_key.starts_with(&root_key) {
        return Ok(false);
    }
    Ok(candidate_key[root_key.len()] == u16::from(b'\\'))
}

/// Converts an extended path into a friendlier display form without resolving it.
#[must_use]
pub fn display_path(path: &Path) -> OsString {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if starts_with_ascii_case_insensitive(&wide, EXTENDED_UNC_PREFIX) {
        let mut display = Vec::with_capacity(wide.len() - 6);
        display.extend_from_slice(UNC_PREFIX);
        display.extend_from_slice(&wide[EXTENDED_UNC_PREFIX.len()..]);
        return OsString::from_wide(&display);
    }
    if starts_with(&wide, EXTENDED_PREFIX) {
        return OsString::from_wide(&wide[EXTENDED_PREFIX.len()..]);
    }
    path.as_os_str().to_os_string()
}

fn starts_with(value: &[u16], prefix: &[u16]) -> bool {
    value.len() >= prefix.len() && &value[..prefix.len()] == prefix
}

fn starts_with_ascii_case_insensitive(value: &[u16], prefix: &[u16]) -> bool {
    value.len() >= prefix.len()
        && value
            .iter()
            .zip(prefix)
            .take(prefix.len())
            .all(|(left, right)| ascii_upper(*left) == ascii_upper(*right))
}

/// Gives WSL's legacy and current aliases one lock/state/root identity. The
/// modern spelling can auto-start a distribution on Windows 11 and avoids the
/// network-name ambiguity that caused Microsoft to introduce it.
fn canonicalize_wsl_server(value: Vec<u16>) -> Vec<u16> {
    if !starts_with_ascii_case_insensitive(&value, EXTENDED_UNC_PREFIX) {
        return value;
    }
    let body = &value[EXTENDED_UNC_PREFIX.len()..];
    let server_end = body
        .iter()
        .position(|unit| *unit == u16::from(b'\\'))
        .unwrap_or(body.len());
    let server = &body[..server_end];
    let legacy = server.len() == 4
        && server
            .iter()
            .zip("wsl$".bytes())
            .all(|(left, right)| ascii_upper(*left) == u16::from(right.to_ascii_uppercase()));
    let current = server.len() == "wsl.localhost".len()
        && server
            .iter()
            .zip("wsl.localhost".bytes())
            .all(|(left, right)| ascii_upper(*left) == u16::from(right.to_ascii_uppercase()));
    if !legacy && !current {
        return value;
    }
    let canonical: Vec<u16> = "wsl.localhost".encode_utf16().collect();
    let mut normalized = Vec::with_capacity(
        EXTENDED_UNC_PREFIX.len() + canonical.len() + body.len().saturating_sub(server_end),
    );
    normalized.extend_from_slice(EXTENDED_UNC_PREFIX);
    normalized.extend_from_slice(&canonical);
    normalized.extend_from_slice(&body[server_end..]);
    normalized
}

fn ascii_upper(value: u16) -> u16 {
    if (u16::from(b'a')..=u16::from(b'z')).contains(&value) {
        value - u16::from(b'a' - b'A')
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{
        absolute_extended, comparison_key, display_path, final_path, is_same_or_descendant,
        is_same_or_descendant_with, ordinal_case_key,
    };
    use crate::{EndpointKind, classify_endpoint, open_root};
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn ordinal_key_is_case_insensitive() {
        let lower = ordinal_case_key(OsStr::new("Résumé.txt"));
        let upper = ordinal_case_key(OsStr::new("RÉSUMÉ.TXT"));
        assert!(lower.is_ok());
        assert_eq!(lower.ok(), upper.ok());
    }

    #[test]
    fn descendant_test_observes_component_boundary() {
        assert!(
            is_same_or_descendant(Path::new(r"\\?\C:\data\child"), Path::new(r"\\?\C:\data"))
                .is_ok_and(|value| value)
        );
        assert!(
            is_same_or_descendant(Path::new(r"\\?\C:\database"), Path::new(r"\\?\C:\data"))
                .is_ok_and(|value| !value)
        );
    }

    #[test]
    fn display_strips_extended_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\data")),
            OsStr::new(r"C:\data")
        );
    }

    #[test]
    fn unc_paths_use_extended_syntax_and_wsl_aliases_canonicalize() {
        assert_eq!(
            absolute_extended(Path::new(r"\\server\share\folder")).ok(),
            Some(Path::new(r"\\?\UNC\server\share\folder").to_path_buf())
        );
        assert_eq!(
            absolute_extended(Path::new(r"\\WSL$\Ubuntu\home")).ok(),
            Some(Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home").to_path_buf())
        );
        assert_eq!(
            absolute_extended(Path::new(r"\\WSL.LOCALHOST\Ubuntu\home")).ok(),
            Some(Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home").to_path_buf())
        );
    }

    #[test]
    fn endpoint_aware_comparison_preserves_wsl_case() {
        let upper = comparison_key(OsStr::new("File"), true);
        let lower = comparison_key(OsStr::new("file"), true);
        assert_ne!(upper.ok(), lower.ok());
        assert!(
            is_same_or_descendant_with(
                Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\File"),
                Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\file"),
                true,
            )
            .is_ok_and(|value| !value)
        );
    }

    #[test]
    fn final_path_preserves_a_local_temp_endpoint() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let pin = open_root(root.path())?;
        let resolved = final_path(&pin)?;
        assert_eq!(
            classify_endpoint(&resolved),
            EndpointKind::Local,
            "unexpected final path: {}",
            resolved.display()
        );
        Ok(())
    }
}
