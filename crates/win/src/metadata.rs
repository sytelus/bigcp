//! Handle-based metadata and non-following directory enumeration.
//!
//! Every object is opened with OPEN_REPARSE_POINT before classification. This
//! keeps traversal from accidentally crossing a junction or symbolic link.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo,
    FileBasicInfo, FileIdExtdDirectoryInfo, FileIdInfo, FileStandardInfo,
    GetFileInformationByHandleEx,
};
use windows_sys::Win32::System::SystemServices::{IO_REPARSE_TAG_CLOUD, IO_REPARSE_TAG_CLOUD_MASK};

use crate::util::{bool_result, last_error};

const ENUMERATION_BUFFER_BYTES: usize = 256 * 1024;
const ERROR_NO_MORE_FILES: i32 = 18;

/// The metadata subset copied by bigcp and used by the equality classifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BasicMetadata {
    /// Creation time in 100 ns Windows FILETIME ticks.
    pub creation_time: i64,
    /// Last-access time in 100 ns Windows FILETIME ticks.
    pub last_access_time: i64,
    /// Last-write time in 100 ns Windows FILETIME ticks.
    pub last_write_time: i64,
    /// Raw Windows file attributes.
    pub attributes: u32,
}

/// Stable identity for a file on one mounted volume.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity {
    /// Volume serial reported by FileIdInfo.
    pub volume_serial: u64,
    /// 128-bit filesystem file identifier.
    pub file_id: [u8; 16],
}

/// Classification that never follows a reparse point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    /// Ordinary data file.
    File,
    /// Real directory that may be traversed.
    Directory,
    /// Reparse point of any tag; callers decide which tags are supported.
    Reparse,
}

/// Metadata collected from one opened object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectMetadata {
    /// Stable object identity.
    pub identity: FileIdentity,
    /// Object type without following reparse points.
    pub kind: ObjectKind,
    /// Logical size of the unnamed stream.
    pub size: u64,
    /// Allocated bytes reported by FileStandardInfo.
    pub allocation_size: u64,
    /// Extended-attribute bytes reported by directory enumeration.
    ///
    /// A direct single-object query has no equivalent Win32 information
    /// class, so it reports zero. Equality classification uses directory
    /// snapshots, where this value is authoritative.
    pub ea_size: u32,
    /// Times and attributes used by copy semantics.
    pub basic: BasicMetadata,
    /// Reparse tag when kind is Reparse.
    pub reparse_tag: Option<u32>,
}

/// A directory child and its non-following metadata snapshot.
#[derive(Clone, Debug)]
pub struct DirectoryEntry {
    /// Child name only, preserving arbitrary UTF-16.
    pub name: OsString,
    /// Absolute extended-length path to the child.
    pub path: PathBuf,
    /// Metadata observed during enumeration.
    pub metadata: ObjectMetadata,
}

/// Opens an object for metadata without following a final reparse point.
pub fn open_metadata(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

/// Opens and pins a root without FILE_SHARE_DELETE for the run lifetime.
///
/// The final component is followed so final_path returns the resolved object;
/// all descendant traversal still uses open_metadata and never follows an
/// unexpected child reparse point.
pub fn open_root(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    options.open(path)
}

/// Reads bigcp's complete classification snapshot for one path.
pub fn metadata_at(path: &Path) -> io::Result<ObjectMetadata> {
    let file = open_metadata(path)?;
    metadata_from_file(&file)
}

/// Reads metadata from an already-open handle.
pub fn metadata_from_file(file: &File) -> io::Result<ObjectMetadata> {
    let raw = file.as_raw_handle().cast();
    let mut basic = FILE_BASIC_INFO::default();
    let mut standard = FILE_STANDARD_INFO::default();
    let mut identity = FILE_ID_INFO::default();
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();

    // SAFETY: every output points to a properly aligned initialized structure,
    // and the borrowed file keeps its valid handle alive across all calls.
    unsafe {
        bool_result(GetFileInformationByHandleEx(
            raw,
            FileBasicInfo,
            (&raw mut basic).cast(),
            size_u32::<FILE_BASIC_INFO>()?,
        ))?;
        bool_result(GetFileInformationByHandleEx(
            raw,
            FileStandardInfo,
            (&raw mut standard).cast(),
            size_u32::<FILE_STANDARD_INFO>()?,
        ))?;
        bool_result(GetFileInformationByHandleEx(
            raw,
            FileIdInfo,
            (&raw mut identity).cast(),
            size_u32::<FILE_ID_INFO>()?,
        ))?;
        bool_result(GetFileInformationByHandleEx(
            raw,
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            size_u32::<FILE_ATTRIBUTE_TAG_INFO>()?,
        ))?;
    }

    let is_cloud = tag.ReparseTag & !IO_REPARSE_TAG_CLOUD_MASK
        == IO_REPARSE_TAG_CLOUD & !IO_REPARSE_TAG_CLOUD_MASK;
    let kind = if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !is_cloud {
        ObjectKind::Reparse
    } else if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        ObjectKind::Directory
    } else {
        ObjectKind::File
    };

    Ok(ObjectMetadata {
        identity: FileIdentity {
            volume_serial: identity.VolumeSerialNumber,
            file_id: identity.FileId.Identifier,
        },
        kind,
        size: nonnegative_u64(standard.EndOfFile, "negative file size")?,
        allocation_size: nonnegative_u64(standard.AllocationSize, "negative allocation size")?,
        ea_size: 0,
        basic: BasicMetadata {
            creation_time: basic.CreationTime,
            last_access_time: basic.LastAccessTime,
            last_write_time: basic.LastWriteTime,
            attributes: basic.FileAttributes,
        },
        reparse_tag: (kind == ObjectKind::Reparse).then_some(tag.ReparseTag),
    })
}

/// Enumerates one directory and snapshots every child without following links.
///
/// One `FileIdExtdDirectoryInfo` pass supplies identity, EA size, allocation,
/// timestamps, attributes, and reparse tag. It avoids a handle-open syscall per
/// child while retaining arbitrary UTF-16 names.
pub fn enumerate_directory(path: &Path) -> io::Result<Vec<DirectoryEntry>> {
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(path)?;
    let mut directory_identity = FILE_ID_INFO::default();
    // SAFETY: the output is aligned and initialized, and directory remains
    // open for the call.
    unsafe {
        bool_result(GetFileInformationByHandleEx(
            directory.as_raw_handle().cast(),
            FileIdInfo,
            (&raw mut directory_identity).cast(),
            size_u32::<FILE_ID_INFO>()?,
        ))?;
    }

    // u64 backing gives the Win32 records their required eight-byte alignment.
    let word_count = ENUMERATION_BUFFER_BYTES.div_ceil(size_of::<u64>());
    let mut buffer = vec![0_u64; word_count];
    let mut entries = Vec::new();
    loop {
        // SAFETY: buffer is writable, correctly aligned, and its reported byte
        // length matches the allocation. The directory handle remains valid.
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle().cast(),
                FileIdExtdDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len() * size_of::<u64>())
                    .map_err(|_| io::Error::other("enumeration buffer is too large"))?,
            )
        };
        if succeeded == 0 {
            let error = last_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                break;
            }
            return Err(error);
        }

        let byte_len = buffer.len() * size_of::<u64>();
        let mut offset = 0_usize;
        loop {
            if offset
                .checked_add(size_of::<FILE_ID_EXTD_DIR_INFO>())
                .is_none_or(|end| end > byte_len)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned a truncated record",
                ));
            }
            // SAFETY: offset was bounds checked and records are naturally
            // aligned by the kernel. We copy fixed fields before reusing the
            // enumeration buffer.
            if !offset.is_multiple_of(size_of::<u64>()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned a misaligned record",
                ));
            }
            let record = unsafe {
                &*buffer
                    .as_ptr()
                    .add(offset / size_of::<u64>())
                    .cast::<FILE_ID_EXTD_DIR_INFO>()
            };
            let name_bytes = usize::try_from(record.FileNameLength)
                .map_err(|_| io::Error::other("file name length does not fit address space"))?;
            if name_bytes % size_of::<u16>() != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned an odd UTF-16 byte length",
                ));
            }
            let name_offset = offset + std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
            if name_offset
                .checked_add(name_bytes)
                .is_none_or(|end| end > byte_len)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned a truncated file name",
                ));
            }
            // SAFETY: the name bounds and two-byte alignment were checked.
            let name_slice = unsafe {
                std::slice::from_raw_parts(record.FileName.as_ptr(), name_bytes / size_of::<u16>())
            };
            let name = OsString::from_wide(name_slice);
            if name != "." && name != ".." {
                let kind = classify_kind(record.FileAttributes, record.ReparsePointTag);
                entries.push(DirectoryEntry {
                    path: path.join(&name),
                    name,
                    metadata: ObjectMetadata {
                        identity: FileIdentity {
                            volume_serial: directory_identity.VolumeSerialNumber,
                            file_id: record.FileId.Identifier,
                        },
                        kind,
                        size: nonnegative_u64(record.EndOfFile, "negative file size")?,
                        allocation_size: nonnegative_u64(
                            record.AllocationSize,
                            "negative allocation size",
                        )?,
                        ea_size: record.EaSize,
                        basic: BasicMetadata {
                            creation_time: record.CreationTime,
                            last_access_time: record.LastAccessTime,
                            last_write_time: record.LastWriteTime,
                            attributes: record.FileAttributes,
                        },
                        reparse_tag: (kind == ObjectKind::Reparse)
                            .then_some(record.ReparsePointTag),
                    },
                });
            }
            if record.NextEntryOffset == 0 {
                break;
            }
            let next = usize::try_from(record.NextEntryOffset)
                .map_err(|_| io::Error::other("directory record offset is too large"))?;
            if next < size_of::<FILE_ID_EXTD_DIR_INFO>()
                || offset
                    .checked_add(next)
                    .is_none_or(|value| value >= byte_len)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned an invalid record offset",
                ));
            }
            offset += next;
        }
    }
    Ok(entries)
}

fn classify_kind(attributes: u32, reparse_tag: u32) -> ObjectKind {
    let is_cloud = reparse_tag & !IO_REPARSE_TAG_CLOUD_MASK
        == IO_REPARSE_TAG_CLOUD & !IO_REPARSE_TAG_CLOUD_MASK;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !is_cloud {
        ObjectKind::Reparse
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        ObjectKind::Directory
    } else {
        ObjectKind::File
    }
}

fn size_u32<T>() -> io::Result<u32> {
    u32::try_from(size_of::<T>())
        .map_err(|_| io::Error::other("Win32 structure size does not fit u32"))
}

fn nonnegative_u64(value: i64, message: &'static str) -> io::Result<u64> {
    u64::try_from(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, message))
}

/// Re-exports the last Win32 error for focused wrapper tests.
#[doc(hidden)]
#[must_use]
pub fn metadata_last_error_for_test() -> io::Error {
    last_error()
}

#[cfg(test)]
mod tests {
    use super::{ObjectKind, enumerate_directory, metadata_at};
    use std::fs;

    #[test]
    fn enumerates_only_inside_system_temp_sandbox() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let child = sandbox.path().join("child.bin");
        assert!(fs::write(&child, b"safe").is_ok());

        let entries = enumerate_directory(sandbox.path());
        assert!(entries.is_ok());
        let Some(entries) = entries.ok() else {
            return;
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metadata.kind, ObjectKind::File);
        assert_eq!(entries[0].metadata.size, 4);

        let metadata = metadata_at(&child);
        assert!(metadata.is_ok());
    }
}
