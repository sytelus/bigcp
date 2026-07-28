//! Verbatim symlink, junction, and explicitly opted-in reparse transfer.
//!
//! Reparse-point directories are never traversed. The source buffer is read
//! from an OPEN_REPARSE_POINT handle and applied to an opaque sibling before
//! the same handle-based atomic rename used for files.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use uuid::Uuid;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
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
) -> io::Result<u32> {
    let source_before = crate::metadata::metadata_at(source)?;
    let data = read_reparse_data(source)?;
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
    let mut temp = ReparseTemp::create(parent, run_id, data.directory)?;
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
    write_to_file(temp.file_ref()?, &extended_attributes)?;
    let source_after = crate::metadata::metadata_at(source)?;
    if source_before.identity != source_after.identity
        || source_before.basic.last_write_time != source_after.basic.last_write_time
        || source_before.reparse_tag != source_after.reparse_tag
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source reparse point changed while it was being copied",
        ));
    }
    temp.commit(destination, replace, metadata, flush, protected_dacl)?;
    Ok(data.tag)
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

    fn file_ref(&self) -> io::Result<&File> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("reparse temporary handle already consumed"))
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
