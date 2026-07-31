//! Alternate data stream discovery and read-only source stream handles.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_HANDLE_EOF, ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, GetLastError,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FindClose, FindFirstStreamW,
    FindNextStreamW, FindStreamInfoStandard, WIN32_FIND_STREAM_DATA,
};

use crate::metadata::{FileIdentity, metadata_from_file};
use crate::util::{last_error, wide_null};

/// One data stream reported by Windows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamInfo {
    /// Exact stream suffix such as ::$DATA or :zone.identifier:$DATA.
    pub name: OsString,
    /// Logical stream bytes.
    pub size: u64,
}

impl StreamInfo {
    /// Constructs the ordinary unnamed data stream without querying ADS.
    ///
    /// FAT-family volumes do not support named streams, so callers can avoid
    /// an unsupported discovery syscall while retaining the shared engine's
    /// stream-oriented accounting.
    #[must_use]
    pub fn unnamed(size: u64) -> Self {
        Self {
            name: OsString::from("::$DATA"),
            size,
        }
    }

    /// Returns true for the ordinary unnamed data stream.
    #[must_use]
    pub fn is_unnamed(&self) -> bool {
        self.name == OsStr::new("::$DATA")
    }

    /// Returns true for an ordinary unnamed or alternate `$DATA` stream.
    ///
    /// Filesystem implementation streams such as a directory's
    /// `::$INDEX_ALLOCATION` are not user ADS and must never be copied.
    #[must_use]
    pub fn is_data(&self) -> bool {
        const SUFFIX: &[u16] = &[
            b':' as u16,
            b'$' as u16,
            b'D' as u16,
            b'A' as u16,
            b'T' as u16,
            b'A' as u16,
        ];
        let name = self.name.encode_wide().collect::<Vec<_>>();
        name.ends_with(SUFFIX)
    }
}

/// Read-only named source stream.
pub struct SourceStream {
    file: File,
}

/// Write-only named stream belonging to a DestinationTemp.
pub struct DestinationStream {
    file: File,
}

impl DestinationStream {
    /// Opens or creates one destination named stream.
    ///
    /// `truncate` is true for a fresh transfer and false only for a
    /// journal-owned resume candidate. The base object's final component is
    /// never followed if it changes to a reparse point.
    pub fn create(base: &Path, stream: &StreamInfo, truncate: bool) -> io::Result<Self> {
        Self::create_with_flags(
            base,
            stream,
            truncate,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
        )
    }

    /// Opens or creates a named stream on a reparse object without following it.
    pub fn create_reparse(base: &Path, stream: &StreamInfo, truncate: bool) -> io::Result<Self> {
        Self::create(base, stream, truncate)
    }

    /// Opens or creates a stream only after pinning the expected base object.
    ///
    /// The short-lived base handle omits delete sharing, so its verified path
    /// cannot be replaced between the identity check and stream creation.
    pub fn create_checked(
        base: &Path,
        expected: FileIdentity,
        stream: &StreamInfo,
        truncate: bool,
    ) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        let pin = options.open(base)?;
        if metadata_from_file(&pin)?.identity != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "base object identity changed before stream creation",
            ));
        }
        let stream = Self::create(base, stream, truncate)?;
        drop(pin);
        Ok(stream)
    }

    fn create_with_flags(
        base: &Path,
        stream: &StreamInfo,
        truncate: bool,
        flags: u32,
    ) -> io::Result<Self> {
        if stream.is_unnamed() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unnamed data must use the primary temporary handle",
            ));
        }
        let path = stream_path(base, &stream.name);
        let mut options = OpenOptions::new();
        options
            // `access_mode` controls CreateFileW, while these flags also
            // satisfy std's create/truncate validation.
            .read(true)
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .create(true)
            .truncate(truncate)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(flags);
        Ok(Self {
            file: options.open(path)?,
        })
    }

    /// Returns the current logical stream length.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Returns whether this stream currently has zero logical length.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Returns the stable identity of the base object that owns this stream.
    pub fn identity(&self) -> io::Result<FileIdentity> {
        Ok(metadata_from_file(&self.file)?.identity)
    }

    /// Truncates uncheckpointed tail bytes before continuation.
    pub fn set_len(&self, length: u64) -> io::Result<()> {
        self.file.set_len(length)
    }
}

impl Write for DestinationStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Read for DestinationStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for DestinationStream {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

impl SourceStream {
    /// Opens one stream suffix using read-only, final-component-safe access.
    ///
    /// This deliberately applies `FILE_FLAG_OPEN_REPARSE_POINT` even for an
    /// enumerated ordinary file. If the path is replaced between enumeration
    /// and this open, Windows must not redirect the read through a new link.
    pub fn open(base: &Path, stream: &StreamInfo) -> io::Result<Self> {
        Self::open_with_flags(
            base,
            stream,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
        )
    }

    /// Opens a named stream on a reparse object without following it.
    pub fn open_reparse(base: &Path, stream: &StreamInfo) -> io::Result<Self> {
        Self::open(base, stream)
    }

    fn open_with_flags(base: &Path, stream: &StreamInfo, flags: u32) -> io::Result<Self> {
        let path = stream_path(base, &stream.name);
        let mut options = OpenOptions::new();
        options
            .access_mode(GENERIC_READ)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(flags);
        Ok(Self {
            file: options.open(path)?,
        })
    }

    /// Returns the stable identity of the base object that owns this stream.
    pub fn identity(&self) -> io::Result<FileIdentity> {
        Ok(metadata_from_file(&self.file)?.identity)
    }
}

impl Read for SourceStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for SourceStream {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

/// Lists user data streams, including the unnamed stream for ordinary files.
///
/// Filesystem-internal non-data streams are deliberately excluded.
pub fn list_streams(path: &Path) -> io::Result<Vec<StreamInfo>> {
    let path = wide_null(path.as_os_str());
    let mut data = WIN32_FIND_STREAM_DATA::default();
    // SAFETY: path is nul-terminated and data is a valid writable structure.
    let handle = unsafe {
        FindFirstStreamW(
            path.as_ptr(),
            FindStreamInfoStandard,
            (&raw mut data).cast(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = last_error();
        if matches!(
            error.raw_os_error(),
            Some(code) if code == ERROR_HANDLE_EOF.cast_signed()
                || code == ERROR_NO_MORE_FILES.cast_signed()
        ) {
            return Ok(Vec::new());
        }
        return Err(error);
    }
    let guard = FindGuard(handle);
    let mut result = Vec::new();
    loop {
        let stream = stream_from_data(&data)?;
        if stream.is_data() {
            result.push(stream);
        }
        data = WIN32_FIND_STREAM_DATA::default();
        // SAFETY: guard owns a live enumeration handle and data is writable.
        let next = unsafe { FindNextStreamW(guard.0, (&raw mut data).cast()) };
        if next == 0 {
            // SAFETY: GetLastError has no preconditions and must immediately
            // follow the failed enumeration call.
            let code = unsafe { GetLastError() };
            if code == ERROR_HANDLE_EOF || code == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(io::Error::from_raw_os_error(code.cast_signed()));
        }
    }
    Ok(result)
}

/// Builds a base-path-plus-stream path without Unicode conversion.
pub(crate) fn stream_path(base: &Path, stream_name: &OsStr) -> PathBuf {
    if stream_name == OsStr::new("::$DATA") {
        return base.to_path_buf();
    }
    let mut wide: Vec<u16> = base.as_os_str().encode_wide().collect();
    wide.extend(stream_name.encode_wide());
    PathBuf::from(OsString::from_wide(&wide))
}

fn stream_from_data(data: &WIN32_FIND_STREAM_DATA) -> io::Result<StreamInfo> {
    let end = data
        .cStreamName
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(data.cStreamName.len());
    let size = u64::try_from(data.StreamSize)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative stream size"))?;
    Ok(StreamInfo {
        name: OsString::from_wide(&data.cStreamName[..end]),
        size,
    })
}

struct FindGuard(windows_sys::Win32::Foundation::HANDLE);

impl Drop for FindGuard {
    fn drop(&mut self) {
        // SAFETY: this guard is created only for a valid FindFirstStream handle.
        unsafe {
            let _ = FindClose(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DestinationStream, SourceStream, StreamInfo};
    use crate::metadata::metadata_at;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::windows::fs::symlink_file;

    #[test]
    fn filesystem_implementation_streams_are_not_user_data() {
        let unnamed = StreamInfo {
            name: OsString::from("::$DATA"),
            size: 0,
        };
        let alternate = StreamInfo {
            name: OsString::from(":zone.identifier:$DATA"),
            size: 1,
        };
        let directory_index = StreamInfo {
            name: OsString::from("::$INDEX_ALLOCATION"),
            size: 0,
        };
        assert!(unnamed.is_data());
        assert!(alternate.is_data());
        assert!(!directory_index.is_data());
    }

    #[test]
    fn source_stream_identity_is_the_base_file_and_links_are_not_followed() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let target = sandbox.path().join("target.txt");
        let link = sandbox.path().join("link.txt");
        assert!(fs::write(&target, b"base").is_ok());
        let stream = StreamInfo {
            name: OsString::from(":identity:$DATA"),
            size: 4,
        };
        let output = DestinationStream::create(&target, &stream, true);
        assert!(output.is_ok());
        let Some(mut output) = output.ok() else {
            return;
        };
        assert!(output.write_all(b"safe").is_ok());
        drop(output);

        let source = SourceStream::open(&target, &stream);
        assert!(source.is_ok());
        let Some(source) = source.ok() else {
            return;
        };
        assert_eq!(
            source.identity().ok(),
            metadata_at(&target).ok().map(|m| m.identity)
        );
        let identity = source.identity();
        assert!(identity.is_ok());
        let Some(mut wrong_identity) = identity.ok() else {
            return;
        };
        wrong_identity.file_id[0] ^= 0xff;
        assert!(DestinationStream::create_checked(&target, wrong_identity, &stream, true).is_err());
        let reopened = SourceStream::open(&target, &stream);
        assert!(reopened.is_ok());
        let Some(mut reopened) = reopened.ok() else {
            return;
        };
        let mut stream_bytes = Vec::new();
        assert!(reopened.read_to_end(&mut stream_bytes).is_ok());
        assert_eq!(stream_bytes, b"safe");

        if symlink_file(&target, &link).is_ok() {
            assert!(SourceStream::open(&link, &stream).is_err());
        }
    }
}
