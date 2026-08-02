//! Verbatim symlink, junction, and explicitly opted-in reparse transfer.
//!
//! Reparse-point directories are never traversed. The source buffer is read
//! from an OPEN_REPARSE_POINT handle and applied to an opaque sibling before
//! the same handle-based atomic rename used for files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateSymbolicLinkW, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
    SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE, SYMBOLIC_LINK_FLAG_DIRECTORY, WRITE_DAC,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_REPARSE_POINT, FSCTL_SET_REPARSE_POINT};
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

use crate::ea::{read_extended_attributes_checked, write_to_file};
use crate::file::{
    close_file, opaque_temp_candidate, rename_by_handle, set_basic_by_handle, set_delete_on_close,
    valid_temp_run_id,
};
use crate::metadata::{BasicMetadata, FileIdentity, ObjectMetadata, metadata_at};
use crate::security::ProtectedDacl;
use crate::streams::{DestinationStream, SourceStream, StreamInfo, list_streams};
use crate::util::{bool_result, wide_null};

/// Complete opaque reparse buffer plus classification facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReparseData {
    /// Reparse tag from the standard header.
    pub tag: u32,
    /// Exact bytes returned by FSCTL_GET_REPARSE_POINT.
    pub bytes: Vec<u8>,
    /// Whether the reparse object has directory shape.
    pub directory: bool,
}

/// Successful reparse transfer facts used for I/O accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReparseCopyResult {
    /// Reparse tag that was published.
    pub tag: u32,
    /// Named-stream bytes copied on the reparse object.
    pub named_stream_bytes: u64,
}

/// Typed failure from reparse transfer.
#[derive(Debug, thiserror::Error)]
pub enum ReparseCopyError {
    /// The reparse object changed after it was examined.
    #[error("source reparse point changed while it was being copied")]
    SourceChanged,
    /// A Win32 or filesystem operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads a reparse point without following it.
pub fn read_reparse_data(path: &Path) -> io::Result<ReparseData> {
    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_READ | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let file = options.open(path)?;
    let mut buffer = vec![0_u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut returned = 0_u32;
    // SAFETY: the source handle is live; the output buffer is writable for its
    // full declared size; the operation is synchronous so OVERLAPPED is null.
    unsafe {
        bool_result(DeviceIoControl(
            file.as_raw_handle().cast(),
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len())
                .map_err(|_| io::Error::other("reparse buffer size overflow"))?,
            &raw mut returned,
            std::ptr::null_mut(),
        ))?;
    }
    let returned = validated_reparse_length(&buffer, returned)?;
    buffer.truncate(returned);
    let tag = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let attributes = crate::metadata::metadata_from_file(&file)?.basic.attributes;
    Ok(ReparseData {
        tag,
        bytes: buffer,
        directory: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
    })
}

fn validated_reparse_length(buffer: &[u8], returned: u32) -> io::Result<usize> {
    let returned = usize::try_from(returned)
        .map_err(|_| io::Error::other("reparse byte count is too large"))?;
    if returned < 8 || returned > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse response length lies outside its output buffer",
        ));
    }
    let payload = usize::from(u16::from_le_bytes([buffer[4], buffer[5]]));
    if payload.checked_add(8) != Some(returned) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse response disagrees with its declared payload length",
        ));
    }
    Ok(returned)
}

/// Copies a reparse point through an opaque sibling and atomic publication.
///
/// Symlinks and mount points are always accepted. Other tags require raw=true
/// because their buffers may depend on a third-party filter driver.
pub fn copy_reparse(
    source: &Path,
    destination: &Path,
    run_id: &str,
    expected_source: &ObjectMetadata,
    expected_destination: Option<&ObjectMetadata>,
    raw: bool,
    flush: bool,
    protected_dacl: Option<&ProtectedDacl>,
    destination_metadata: BasicMetadata,
    posix_unlink_rename: bool,
) -> Result<ReparseCopyResult, ReparseCopyError> {
    if !valid_temp_run_id(run_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "temporary run ID must contain only ASCII letters, digits, or hyphens",
        )
        .into());
    }
    let source_before = source_result(metadata_at(source))?;
    if !same_reparse_snapshot(&source_before, expected_source) {
        return Err(ReparseCopyError::SourceChanged);
    }
    let data = source_result(read_reparse_data(source))?;
    let streams = source_result(list_streams(source))?;
    let extended_attributes = source_identity_result(read_extended_attributes_checked(
        source,
        expected_source.identity,
    ))?;
    if data.tag != IO_REPARSE_TAG_SYMLINK && data.tag != IO_REPARSE_TAG_MOUNT_POINT && !raw {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "unsupported reparse tag 0x{:08x}; raw copying was not enabled",
                data.tag
            ),
        )
        .into());
    }
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "reparse destination has no parent",
        )
    })?;
    let mut temp = if data.tag == IO_REPARSE_TAG_SYMLINK {
        let target = symlink_target(&data.bytes)?;
        ReparseTemp::create_symbolic_link(parent, run_id, &target, data.directory)?
    } else {
        let temp = ReparseTemp::create(parent, run_id, data.directory)?;
        let mut returned = 0_u32;
        // SAFETY: both the live destination handle and immutable reparse buffer are
        // valid for the synchronous control call.
        unsafe {
            bool_result(DeviceIoControl(
                temp.file_ref()?.as_raw_handle().cast(),
                FSCTL_SET_REPARSE_POINT,
                data.bytes.as_ptr().cast(),
                u32::try_from(data.bytes.len())
                    .map_err(|_| io::Error::other("reparse buffer size overflow"))?,
                std::ptr::null_mut(),
                0,
                &raw mut returned,
                std::ptr::null_mut(),
            ))?;
        }
        temp
    };
    let named_stream_bytes = copy_named_streams(source, &temp, &streams, source_before.identity)?;
    write_to_file(temp.file_ref()?, &extended_attributes)?;
    let source_after = source_result(metadata_at(source))?;
    if !same_reparse_snapshot(&source_after, expected_source) {
        return Err(ReparseCopyError::SourceChanged);
    }
    revalidate_destination(destination, expected_destination)?;
    temp.commit(
        destination,
        expected_destination.is_some(),
        destination_metadata,
        flush,
        protected_dacl,
        posix_unlink_rename,
    )?;
    Ok(ReparseCopyResult {
        tag: data.tag,
        named_stream_bytes,
    })
}

fn same_reparse_snapshot(observed: &ObjectMetadata, expected: &ObjectMetadata) -> bool {
    observed.identity == expected.identity
        && observed.kind == expected.kind
        && observed.size == expected.size
        && observed.basic.last_write_time == expected.basic.last_write_time
        && observed.basic.attributes == expected.basic.attributes
        && observed.reparse_tag == expected.reparse_tag
}

fn source_result<T>(result: io::Result<T>) -> Result<T, ReparseCopyError> {
    match result {
        Ok(value) => Ok(value),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof
            ) =>
        {
            Err(ReparseCopyError::SourceChanged)
        }
        Err(error) => Err(ReparseCopyError::Io(error)),
    }
}

fn source_identity_result<T>(result: io::Result<T>) -> Result<T, ReparseCopyError> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            Err(ReparseCopyError::SourceChanged)
        }
        other => source_result(other),
    }
}

fn revalidate_destination(destination: &Path, expected: Option<&ObjectMetadata>) -> io::Result<()> {
    match (expected, metadata_at(destination)) {
        (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        (None, Ok(_)) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a destination object appeared after classification",
        )),
        (Some(expected), Ok(observed))
            if observed.identity == expected.identity
                && observed.kind == expected.kind
                && observed.size == expected.size
                && observed.basic.last_write_time == expected.basic.last_write_time
                && observed.basic.attributes == expected.basic.attributes
                && observed.reparse_tag == expected.reparse_tag =>
        {
            Ok(())
        }
        (Some(_), Ok(_)) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination changed after classification",
        )),
        (Some(_), Err(error)) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination disappeared after classification",
        )),
        (None | Some(_), Err(error)) => Err(error),
    }
}

struct ReparseTemp {
    file: Option<File>,
    path: PathBuf,
}

impl ReparseTemp {
    fn create(parent: &Path, run_id: &str, directory: bool) -> io::Result<Self> {
        for _ in 0..128 {
            let path = opaque_temp_candidate(parent, run_id)?;
            let created = if directory {
                fs::create_dir(&path).and_then(|()| Self::open_created(path.clone()))
            } else {
                Self::create_file(path)
            };
            match created {
                Ok(temp) => return Ok(temp),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique reparse temporary name",
        ))
    }

    fn create_file(path: PathBuf) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            // WRITE_DAC: commit applies a preserved protected DACL through
            // this handle, and SetSecurityInfo checks granted handle access.
            .access_mode(
                GENERIC_READ
                    | GENERIC_WRITE
                    | DELETE
                    | FILE_READ_ATTRIBUTES
                    | FILE_WRITE_ATTRIBUTES
                    | WRITE_DAC,
            )
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .create_new(true);
        let file = options.open(&path)?;
        Self::arm_created(file, path)
    }

    fn create_symbolic_link(
        parent: &Path,
        run_id: &str,
        target: &[u16],
        directory: bool,
    ) -> io::Result<Self> {
        if target.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "symbolic-link target contains an embedded NUL",
            ));
        }
        let mut target_wide = target.to_vec();
        target_wide.push(0);
        for _ in 0..128 {
            let path = opaque_temp_candidate(parent, run_id)?;
            let path_wide = wide_null(path.as_os_str())?;
            let flags = SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE
                | if directory {
                    SYMBOLIC_LINK_FLAG_DIRECTORY
                } else {
                    0
                };
            // SAFETY: both UTF-16 buffers are NUL-terminated and remain live for
            // the duration of the synchronous call.
            let created =
                unsafe { CreateSymbolicLinkW(path_wide.as_ptr(), target_wide.as_ptr(), flags) };
            if created {
                return Self::open_created(path);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique symbolic-link temporary name",
        ))
    }

    fn open_created(path: PathBuf) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options
            // WRITE_DAC: see create_file — commit can apply a protected DACL.
            .access_mode(
                GENERIC_READ
                    | GENERIC_WRITE
                    | DELETE
                    | FILE_READ_ATTRIBUTES
                    | FILE_WRITE_ATTRIBUTES
                    | WRITE_DAC,
            )
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        // See `create`: never delete by path after losing the handle that
        // proves which object occupies this opaque name.
        let file = options.open(&path)?;
        Self::arm_created(file, path)
    }

    fn arm_created(file: File, path: PathBuf) -> io::Result<Self> {
        if let Err(error) = set_delete_on_close(&file, true) {
            drop(file);
            return Err(error);
        }
        Ok(Self {
            file: Some(file),
            path,
        })
    }

    fn file_ref(&self) -> io::Result<&File> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("reparse temporary handle already consumed"))
    }

    fn create_stream(&self, stream: &StreamInfo) -> io::Result<DestinationStream> {
        set_delete_on_close(self.file_ref()?, false)?;
        let opened = DestinationStream::create_reparse(&self.path, stream, true);
        if let Err(error) = set_delete_on_close(self.file_ref()?, true) {
            drop(opened);
            return Err(error);
        }
        opened
    }

    fn commit(
        &mut self,
        destination: &Path,
        replace: bool,
        metadata: BasicMetadata,
        flush: bool,
        protected_dacl: Option<&ProtectedDacl>,
        posix_unlink_rename: bool,
    ) -> io::Result<()> {
        let file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("reparse temporary handle already consumed"))?;
        if let Some(dacl) = protected_dacl {
            dacl.apply_to(&file)?;
        }
        set_delete_on_close(&file, false)?;
        if let Err(error) = rename_by_handle(&file, destination, replace, posix_unlink_rename) {
            let _ = set_delete_on_close(&file, true);
            self.file = Some(file);
            return Err(error);
        }
        self.path = destination.to_path_buf();
        set_basic_by_handle(&file, metadata)?;
        if flush {
            file.sync_all()?;
        }
        close_file(file)
    }
}

fn copy_named_streams(
    source: &Path,
    temp: &ReparseTemp,
    streams: &[StreamInfo],
    expected_source_identity: FileIdentity,
) -> Result<u64, ReparseCopyError> {
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut total = 0_u64;
    for stream in streams.iter().filter(|stream| !stream.is_unnamed()) {
        let mut input = source_result(SourceStream::open_reparse(source, stream))?;
        if source_result(input.identity())? != expected_source_identity {
            return Err(ReparseCopyError::SourceChanged);
        }
        let mut output = temp.create_stream(stream)?;
        let mut copied = 0_u64;
        loop {
            let count = source_result(input.read(&mut buffer))?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            copied = copied.saturating_add(count as u64);
        }
        if copied != stream.size {
            return Err(ReparseCopyError::SourceChanged);
        }
        output.flush()?;
        total = total.saturating_add(copied);
    }
    Ok(total)
}

fn symlink_target(buffer: &[u8]) -> io::Result<Vec<u16>> {
    const PATH_BUFFER_OFFSET: usize = 20;
    const SYMLINK_FLAG_RELATIVE: u32 = 0x1;
    if buffer.len() < PATH_BUFFER_OFFSET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link reparse buffer is truncated",
        ));
    }
    // The SubstituteName is what the filesystem dereferences; the PrintName is
    // cosmetic and can be stale or deliberately different. Fidelity requires
    // recreating from the substitute name plus the RELATIVE flag — never from
    // the print name.
    let substitute_offset = usize::from(read_u16(buffer, 8)?);
    let substitute_length = usize::from(read_u16(buffer, 10)?);
    let flags = u32::from(read_u16(buffer, 16)?) | (u32::from(read_u16(buffer, 18)?) << 16);
    let relative = flags & SYMLINK_FLAG_RELATIVE != 0;
    let (offset, length, substitute) = (substitute_offset, substitute_length, !relative);
    if offset % 2 != 0 || length % 2 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link target is not aligned UTF-16",
        ));
    }
    let start = PATH_BUFFER_OFFSET.checked_add(offset).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link target offset overflow",
        )
    })?;
    let end = start.checked_add(length).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link target length overflow",
        )
    })?;
    let bytes = buffer.get(start..end).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link target lies outside its reparse buffer",
        )
    })?;
    let mut target = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    if substitute {
        // Absolute links carry an NT-namespace substitute (`\??\…`); strip it
        // to the user-mode form CreateSymbolicLinkW expects. A volume-GUID
        // target has no drive-letter user form — stripping would leave a path
        // CreateSymbolicLinkW misreads as *relative*, silently corrupting the
        // link, so it is refused instead (documented limitation).
        let volume_prefix: Vec<u16> = r"\??\Volume{".encode_utf16().collect();
        if target.starts_with(&volume_prefix) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "symbolic link targets a volume GUID path; recreate it manually at the destination",
            ));
        }
        target = user_path_from_substitute(target);
    }
    if target.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link target is empty",
        ));
    }
    Ok(target)
}

fn read_u16(buffer: &[u8], offset: usize) -> io::Result<u16> {
    let bytes = buffer
        .get(offset..offset + 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "reparse field is truncated"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn user_path_from_substitute(mut target: Vec<u16>) -> Vec<u16> {
    let nt_prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    let unc_prefix: Vec<u16> = r"\??\UNC\".encode_utf16().collect();
    if target.starts_with(&unc_prefix) {
        let mut user = vec![u16::from(b'\\'), u16::from(b'\\')];
        user.extend_from_slice(&target[unc_prefix.len()..]);
        user
    } else if target.starts_with(&nt_prefix) {
        target.drain(..nt_prefix.len());
        target
    } else {
        target
    }
}

impl Drop for ReparseTemp {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            drop(file);
        }
        // The delete disposition is bound to the exact created object. A
        // path-based fallback after close could remove a replacement name.
    }
}

#[cfg(test)]
mod tests {
    use super::{symlink_target, validated_reparse_length};

    #[test]
    fn reparse_response_length_must_match_the_header_and_buffer() {
        let mut value = vec![0_u8; 12];
        value[4..6].copy_from_slice(&4_u16.to_le_bytes());
        assert_eq!(validated_reparse_length(&value, 12).ok(), Some(12));
        assert!(validated_reparse_length(&value, 11).is_err());
        assert!(validated_reparse_length(&value, 13).is_err());
    }

    fn buffer(substitute: &str, print: &str) -> Vec<u8> {
        let substitute: Vec<u16> = substitute.encode_utf16().collect();
        let print: Vec<u16> = print.encode_utf16().collect();
        let Some(substitute_bytes) = substitute
            .len()
            .checked_mul(2)
            .and_then(|value| u16::try_from(value).ok())
        else {
            return Vec::new();
        };
        let Some(print_bytes) = print
            .len()
            .checked_mul(2)
            .and_then(|value| u16::try_from(value).ok())
        else {
            return Vec::new();
        };
        let mut value = vec![0_u8; 20];
        value[10..12].copy_from_slice(&substitute_bytes.to_le_bytes());
        value[12..14].copy_from_slice(&substitute_bytes.to_le_bytes());
        value[14..16].copy_from_slice(&print_bytes.to_le_bytes());
        for code_unit in substitute.into_iter().chain(print) {
            value.extend_from_slice(&code_unit.to_le_bytes());
        }
        value
    }

    fn relative_buffer(substitute: &str, print: &str) -> Vec<u8> {
        let mut value = buffer(substitute, print);
        value[16..20].copy_from_slice(&1_u32.to_le_bytes());
        value
    }

    #[test]
    fn parser_uses_the_authoritative_substitute_name_not_the_print_name() {
        // A stale or hostile print name must never redirect the recreated
        // link: the substitute name is what the filesystem dereferences.
        let parsed = symlink_target(&buffer(r"\??\C:\real", r"C:\fake"));
        assert_eq!(parsed.ok(), Some(r"C:\real".encode_utf16().collect()));
    }

    #[test]
    fn parser_preserves_a_relative_target_verbatim() {
        let parsed = symlink_target(&relative_buffer(r"sibling\file", r"sibling\file"));
        assert_eq!(parsed.ok(), Some(r"sibling\file".encode_utf16().collect()));
    }

    #[test]
    fn parser_converts_an_nt_unc_substitute_name() {
        let parsed = symlink_target(&buffer(r"\??\UNC\server\share\file", ""));
        assert_eq!(
            parsed.ok(),
            Some(r"\\server\share\file".encode_utf16().collect())
        );
    }

    #[test]
    fn parser_refuses_a_volume_guid_substitute() {
        // Stripping `\??\` from a volume-GUID target would leave a path that
        // CreateSymbolicLinkW misreads as relative — a corrupted link.
        let parsed = symlink_target(&buffer(r"\??\Volume{0000-0000}\dir\file", ""));
        assert!(parsed.is_err());
    }

    #[test]
    fn parser_rejects_an_out_of_bounds_target() {
        let mut value = buffer("target", "");
        value[10..12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(symlink_target(&value).is_err());
    }
}
