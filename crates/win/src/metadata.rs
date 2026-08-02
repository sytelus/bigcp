//! Handle-based metadata and non-following directory enumeration.
//!
//! Every object is opened with OPEN_REPARSE_POINT before classification. This
//! keeps traversal from accidentally crossing a junction or symbolic link.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo, FileBasicInfo,
    FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdExtdDirectoryInfo, FileIdInfo,
    FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
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
///
/// Deliberately **not** `PartialEq`: `ea_size` is populated asymmetrically
/// (authoritative from directory enumeration, always zero from handle
/// queries), so whole-struct equality would misreport identical snapshots as
/// different. Compare the specific fields a decision needs.
#[derive(Clone, Debug)]
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
        if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bool_result(GetFileInformationByHandleEx(
                raw,
                FileAttributeTagInfo,
                (&raw mut tag).cast(),
                size_u32::<FILE_ATTRIBUTE_TAG_INFO>()?,
            ))?;
        }
    }
    let identity = identity_from_file(file)?;

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
        identity,
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
    let directory_identity = identity_from_file(&directory)?;

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
            if entries.is_empty() && unsupported_information_class(&error) {
                return enumerate_directory_legacy(
                    &directory,
                    path,
                    directory_identity.volume_serial,
                );
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
            if !offset.is_multiple_of(size_of::<u64>()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned a misaligned record",
                ));
            }
            // SAFETY: offset and eight-byte alignment were checked above.
            // The fixed prefix is copied by value — a `&FILE_ID_EXTD_DIR_INFO`
            // is never formed, because a reference's provenance would not
            // cover the variable-length name that follows the struct.
            let record = unsafe {
                buffer
                    .as_ptr()
                    .add(offset / size_of::<u64>())
                    .cast::<FILE_ID_EXTD_DIR_INFO>()
                    .read()
            };
            let name_field_offset = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
            let (name_units, next_offset) = directory_record_layout(
                offset,
                byte_len,
                size_of::<FILE_ID_EXTD_DIR_INFO>(),
                name_field_offset,
                record.FileNameLength,
                record.NextEntryOffset,
                "directory enumeration",
            )?;
            // SAFETY: the name bounds and two-byte alignment were checked by
            // directory_record_layout; the pointer derives from the backing
            // buffer, so its provenance spans the whole allocation.
            let name_slice = unsafe {
                std::slice::from_raw_parts(
                    buffer
                        .as_ptr()
                        .cast::<u16>()
                        .add((offset + name_field_offset) / size_of::<u16>()),
                    name_units,
                )
            };
            let name = OsString::from_wide(name_slice);
            if validate_child_name(&name)? {
                let kind = classify_kind(record.FileAttributes, record.ReparsePointTag);
                entries.push(DirectoryEntry {
                    path: path.join(&name),
                    name,
                    metadata: ObjectMetadata {
                        identity: FileIdentity {
                            volume_serial: directory_identity.volume_serial,
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
            let Some(next_offset) = next_offset else {
                break;
            };
            offset = next_offset;
        }
    }
    Ok(entries)
}

/// Enumerates through the older 64-bit-ID information class used by FAT
/// drivers that reject `FileIdExtdDirectoryInfo`. This remains a single
/// directory query stream: FAT support does not regress to one handle-open
/// syscall per child.
fn enumerate_directory_legacy(
    directory: &File,
    path: &Path,
    volume_serial: u64,
) -> io::Result<Vec<DirectoryEntry>> {
    let word_count = ENUMERATION_BUFFER_BYTES.div_ceil(size_of::<u64>());
    let mut buffer = vec![0_u64; word_count];
    let mut entries = Vec::new();
    let mut restart = true;
    loop {
        // SAFETY: buffer is writable and eight-byte aligned; directory stays
        // open throughout the enumeration sequence.
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle().cast(),
                if restart {
                    FileIdBothDirectoryRestartInfo
                } else {
                    FileIdBothDirectoryInfo
                },
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len() * size_of::<u64>())
                    .map_err(|_| io::Error::other("enumeration buffer is too large"))?,
            )
        };
        restart = false;
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
                .checked_add(size_of::<FILE_ID_BOTH_DIR_INFO>())
                .is_none_or(|end| end > byte_len)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy directory enumeration returned a truncated record",
                ));
            }
            if !offset.is_multiple_of(size_of::<u64>()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy directory enumeration returned a misaligned record",
                ));
            }
            // SAFETY: fixed record size and alignment were checked above. The
            // fixed prefix is copied by value — no reference is formed whose
            // provenance would exclude the trailing variable-length name.
            let record = unsafe {
                buffer
                    .as_ptr()
                    .add(offset / size_of::<u64>())
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
                    .read()
            };
            let name_field_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            let (name_units, next_offset) = directory_record_layout(
                offset,
                byte_len,
                size_of::<FILE_ID_BOTH_DIR_INFO>(),
                name_field_offset,
                record.FileNameLength,
                record.NextEntryOffset,
                "legacy directory enumeration",
            )?;
            // SAFETY: the name bounds and two-byte alignment were checked by
            // directory_record_layout; the pointer derives from the backing
            // buffer, so its provenance spans the whole allocation.
            let name_slice = unsafe {
                std::slice::from_raw_parts(
                    buffer
                        .as_ptr()
                        .cast::<u16>()
                        .add((offset + name_field_offset) / size_of::<u16>()),
                    name_units,
                )
            };
            let name = OsString::from_wide(name_slice);
            if validate_child_name(&name)? {
                // FAT/exFAT do not support reparse points. Preserving the
                // attribute classification still fails safely if a third-
                // party driver returns one through this legacy class.
                let kind = classify_kind(record.FileAttributes, 0);
                let mut file_id = [0_u8; 16];
                file_id[..8].copy_from_slice(&record.FileId.to_le_bytes());
                entries.push(DirectoryEntry {
                    path: path.join(&name),
                    name,
                    metadata: ObjectMetadata {
                        identity: FileIdentity {
                            volume_serial,
                            file_id,
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
                        reparse_tag: (kind == ObjectKind::Reparse).then_some(0),
                    },
                });
            }
            let Some(next_offset) = next_offset else {
                break;
            };
            offset = next_offset;
        }
    }
    Ok(entries)
}

/// Validates record-local lengths before a variable-length name is observed.
/// A provider-supplied name must never reach into the next record, even when
/// the complete enumeration buffer is large enough to make such a slice safe.
fn directory_record_layout(
    offset: usize,
    buffer_len: usize,
    fixed_size: usize,
    name_field_offset: usize,
    name_bytes: u32,
    next_entry_offset: u32,
    context: &str,
) -> io::Result<(usize, Option<usize>)> {
    let name_bytes = usize::try_from(name_bytes)
        .map_err(|_| io::Error::other("file name length does not fit address space"))?;
    if name_bytes % size_of::<u16>() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{context} returned an odd UTF-16 byte length"),
        ));
    }

    let next_offset = if next_entry_offset == 0 {
        None
    } else {
        let next = usize::try_from(next_entry_offset)
            .map_err(|_| io::Error::other("directory record offset is too large"))?;
        let absolute = offset.checked_add(next);
        if next < fixed_size
            || !next.is_multiple_of(size_of::<u64>())
            || absolute.is_none_or(|value| value >= buffer_len)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{context} returned an invalid record offset"),
            ));
        }
        absolute
    };
    let record_end = next_offset.unwrap_or(buffer_len);
    let name_offset = offset
        .checked_add(name_field_offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file name offset overflow"))?;
    if name_offset
        .checked_add(name_bytes)
        .is_none_or(|end| end > record_end)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{context} returned a file name outside its record"),
        ));
    }
    Ok((name_bytes / size_of::<u16>(), next_offset))
}

/// Accept only one literal child component. This is defense in depth around
/// filesystem/redirector output: a malformed name must not turn `join` into a
/// path outside the directory handle that produced it.
fn validate_child_name(name: &OsStr) -> io::Result<bool> {
    if name == "." || name == ".." {
        return Ok(false);
    }
    if name.encode_wide().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory enumeration returned a name containing NUL",
        ));
    }
    // No real local filesystem returns `:` inside a child name, but a hostile
    // or buggy redirector could. `dir\victim.txt:payload` parses as a single
    // normal component, and joining it would address an alternate data stream
    // of a *sibling* at the destination — data landing in the wrong file.
    if name.encode_wide().any(|unit| unit == u16::from(b':')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory enumeration returned a name containing a stream separator",
        ));
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == name => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory enumeration returned a non-child name",
        )),
    }
}

fn identity_from_file(file: &File) -> io::Result<FileIdentity> {
    let raw = file.as_raw_handle().cast();
    let mut identity = FILE_ID_INFO::default();
    // SAFETY: output is aligned and initialized; the file owns a valid handle.
    let extended = unsafe {
        bool_result(GetFileInformationByHandleEx(
            raw,
            FileIdInfo,
            (&raw mut identity).cast(),
            size_u32::<FILE_ID_INFO>()?,
        ))
    };
    match extended {
        Ok(()) => Ok(FileIdentity {
            volume_serial: identity.VolumeSerialNumber,
            file_id: identity.FileId.Identifier,
        }),
        Err(error) if unsupported_information_class(&error) => {
            let mut legacy = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: output is aligned and initialized; the handle stays live.
            unsafe {
                bool_result(GetFileInformationByHandle(raw, &raw mut legacy))?;
            }
            let id = (u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow);
            let mut file_id = [0_u8; 16];
            file_id[..8].copy_from_slice(&id.to_le_bytes());
            Ok(FileIdentity {
                volume_serial: u64::from(legacy.dwVolumeSerialNumber),
                file_id,
            })
        }
        Err(error) => Err(error),
    }
}

fn unsupported_information_class(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_INVALID_FUNCTION.cast_signed()
                || code == ERROR_NOT_SUPPORTED.cast_signed()
                || code == ERROR_INVALID_PARAMETER.cast_signed()
    )
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
#[cfg(test)]
mod tests {
    use super::{
        ObjectKind, directory_record_layout, enumerate_directory, metadata_at,
        unsupported_information_class, validate_child_name,
    };
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStringExt;

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

    #[test]
    fn legacy_identity_fallback_is_limited_to_unsupported_information_classes() {
        for code in [1, 50, 87] {
            assert!(unsupported_information_class(
                &io::Error::from_raw_os_error(code)
            ));
        }
        assert!(!unsupported_information_class(
            &io::Error::from_raw_os_error(5)
        ));
    }

    #[test]
    fn provider_names_and_record_lengths_cannot_escape_their_record() {
        assert_eq!(
            validate_child_name(OsStr::new("ordinary.txt")).ok(),
            Some(true)
        );
        assert_eq!(validate_child_name(OsStr::new(".")).ok(), Some(false));
        // `victim.txt:payload` is a single Normal component to the path
        // parser, but joining it would address a sibling's alternate data
        // stream at the destination — it must be rejected outright.
        for invalid in [
            r"\escape",
            r"..\escape",
            r"C:escape",
            "two/parts",
            "victim.txt:payload",
            ":bare-stream",
        ] {
            assert!(validate_child_name(OsStr::new(invalid)).is_err());
        }
        assert!(validate_child_name(&OsString::from_wide(&[u16::from(b'a'), 0])).is_err());

        assert!(directory_record_layout(0, 256, 64, 60, 2, 64, "test").is_ok());
        assert!(directory_record_layout(0, 256, 64, 60, 6, 64, "test").is_err());
        assert!(directory_record_layout(0, 256, 64, 60, 2, 65, "test").is_err());
    }
}
