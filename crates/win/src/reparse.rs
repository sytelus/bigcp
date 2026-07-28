//! Verbatim symlink, junction, and explicitly opted-in reparse transfer.
//!
//! Reparse-point directories are never traversed. The source buffer is read
//! from an OPEN_REPARSE_POINT handle and applied to an opaque sibling before
//! the same handle-based atomic rename used for files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use uuid::Uuid;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateSymbolicLinkW, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
    SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE, SYMBOLIC_LINK_FLAG_DIRECTORY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_REPARSE_POINT, FSCTL_SET_REPARSE_POINT};
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

use crate::ea::{read_extended_attributes, write_to_file};
use crate::file::{close_file, rename_by_handle, set_basic_by_handle, set_delete_on_close};
use crate::metadata::BasicMetadata;
use crate::security::ProtectedDacl;
use crate::streams::{DestinationStream, SourceStream, StreamInfo, list_streams};
use crate::util::bool_result;

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
    if returned < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse buffer is shorter than its standard header",
        ));
    }
    buffer.truncate(returned as usize);
    let tag = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let attributes = crate::metadata::metadata_from_file(&file)?.basic.attributes;
    Ok(ReparseData {
        tag,
        bytes: buffer,
        directory: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
    })
}

/// Copies a reparse point through an opaque sibling and atomic publication.
///
/// Symlinks and mount points are always accepted. Other tags require raw=true
/// because their buffers may depend on a third-party filter driver.
pub fn copy_reparse(
    source: &Path,
    destination: &Path,
    run_id: &str,
    replace: bool,
    metadata: BasicMetadata,
    raw: bool,
    flush: bool,
    protected_dacl: Option<&ProtectedDacl>,
) -> io::Result<ReparseCopyResult> {
    let source_before = crate::metadata::metadata_at(source)?;
    let data = read_reparse_data(source)?;
    let streams = list_streams(source)?;
    let extended_attributes = read_extended_attributes(source)?;
    if data.tag != IO_REPARSE_TAG_SYMLINK && data.tag != IO_REPARSE_TAG_MOUNT_POINT && !raw {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "unsupported reparse tag 0x{:08x}; raw copying was not enabled",
                data.tag
            ),
        ));
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
    let named_stream_bytes = copy_named_streams(source, &temp, &streams)?;
    write_to_file(temp.file_ref()?, &extended_attributes)?;
    let source_after = crate::metadata::metadata_at(source)?;
    if source_before.identity != source_after.identity
        || source_before.size != source_after.size
        || source_before.basic.last_write_time != source_after.basic.last_write_time
        || source_before.basic.attributes != source_after.basic.attributes
        || source_before.reparse_tag != source_after.reparse_tag
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source reparse point changed while it was being copied",
        ));
    }
    temp.commit(destination, replace, metadata, flush, protected_dacl)?;
    Ok(ReparseCopyResult {
        tag: data.tag,
        named_stream_bytes,
    })
}

struct ReparseTemp {
    file: Option<File>,
    path: PathBuf,
    directory: bool,
    owned: bool,
}

impl ReparseTemp {
    fn create(parent: &Path, run_id: &str, directory: bool) -> io::Result<Self> {
        for _ in 0..128 {
            let nonce = Uuid::new_v4().simple().to_string();
            let path = parent.join(format!(".bigcp-{run_id}-{}.part", &nonce[..12]));
            let creation = if directory {
                fs::create_dir(&path)
            } else {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map(drop)
            };
            match creation {
                Ok(()) => {
                    let mut options = OpenOptions::new();
                    options
                        .access_mode(
                            GENERIC_READ
                                | GENERIC_WRITE
                                | DELETE
                                | FILE_READ_ATTRIBUTES
                                | FILE_WRITE_ATTRIBUTES,
                        )
                        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
                    let file = match options.open(&path) {
                        Ok(file) => file,
                        Err(error) => {
                            remove_owned(&path, directory);
                            return Err(error);
                        }
                    };
                    if let Err(error) = set_delete_on_close(&file, true) {
                        drop(file);
                        remove_owned(&path, directory);
                        return Err(error);
                    }
                    return Ok(Self {
                        file: Some(file),
                        path,
                        directory,
                        owned: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique reparse temporary name",
        ))
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
            let nonce = Uuid::new_v4().simple().to_string();
            let path = parent.join(format!(".bigcp-{run_id}-{}.part", &nonce[..12]));
            let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
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
                return Self::open_created(path, directory);
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

    fn open_created(path: PathBuf, directory: bool) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options
            .access_mode(
                GENERIC_READ
                    | GENERIC_WRITE
                    | DELETE
                    | FILE_READ_ATTRIBUTES
                    | FILE_WRITE_ATTRIBUTES,
            )
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) => {
                remove_owned(&path, directory);
                return Err(error);
            }
        };
        if let Err(error) = set_delete_on_close(&file, true) {
            drop(file);
            remove_owned(&path, directory);
            return Err(error);
        }
        Ok(Self {
            file: Some(file),
            path,
            directory,
            owned: true,
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
    ) -> io::Result<()> {
        let file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("reparse temporary handle already consumed"))?;
        if let Some(dacl) = protected_dacl {
            dacl.apply_to(&file)?;
        }
        set_delete_on_close(&file, false)?;
        if let Err(error) = rename_by_handle(&file, destination, replace) {
            let _ = set_delete_on_close(&file, true);
            self.file = Some(file);
            return Err(error);
        }
        self.owned = false;
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
) -> io::Result<u64> {
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut total = 0_u64;
    for stream in streams.iter().filter(|stream| !stream.is_unnamed()) {
        let mut input = SourceStream::open_reparse(source, stream)?;
        let mut output = temp.create_stream(stream)?;
        let mut copied = 0_u64;
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            copied = copied.saturating_add(count as u64);
        }
        if copied != stream.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "reparse-point named stream changed size while it was copied",
            ));
        }
        output.flush()?;
        total = total.saturating_add(copied);
    }
    Ok(total)
}

fn symlink_target(buffer: &[u8]) -> io::Result<Vec<u16>> {
    const PATH_BUFFER_OFFSET: usize = 20;
    if buffer.len() < PATH_BUFFER_OFFSET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link reparse buffer is truncated",
        ));
    }
    let substitute_offset = usize::from(read_u16(buffer, 8)?);
    let substitute_length = usize::from(read_u16(buffer, 10)?);
    let print_offset = usize::from(read_u16(buffer, 12)?);
    let print_length = usize::from(read_u16(buffer, 14)?);
    let (offset, length, substitute) = if print_length > 0 {
        (print_offset, print_length, false)
    } else {
        (substitute_offset, substitute_length, true)
    };
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
        if self.owned {
            remove_owned(&self.path, self.directory);
        }
    }
}

fn remove_owned(path: &Path, directory: bool) {
    if directory {
        let _ = fs::remove_dir(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::symlink_target;

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

    #[test]
    fn parser_prefers_the_user_facing_print_name() {
        let parsed = symlink_target(&buffer(r"\??\C:\target", r"C:\target"));
        assert_eq!(parsed.ok(), Some(r"C:\target".encode_utf16().collect()));
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
    fn parser_rejects_an_out_of_bounds_target() {
        let mut value = buffer("target", "");
        value[10..12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(symlink_target(&value).is_err());
    }
}
