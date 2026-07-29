//! Source-read and destination-write choke points.
//!
//! Source handles are constructed read-only. Destination files are always
//! opaque sibling temporaries that self-delete on close until commit. The only
//! replacing write primitive in the workspace consumes DestinationTemp.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::mem::size_of;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, IntoRawHandle};
use std::path::{Path, PathBuf};

use uuid::Uuid;
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_COMPRESSED,
    FILE_ATTRIBUTE_ENCRYPTED, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_SYSTEM, FILE_BASIC_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, FileAllocationInfo, FileBasicInfo, FileDispositionInfo,
    FileRenameInfoEx, FlushFileBuffers, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    MoveFileExW, SetFileInformationByHandle,
};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
};

use crate::ea::{ExtendedAttributes, write_to_file};
use crate::metadata::{
    BasicMetadata, FileIdentity, ObjectKind, ObjectMetadata, metadata_from_file,
};
use crate::security::ProtectedDacl;
use crate::sparse::{AllocatedRange, mark_sparse, query_allocated_ranges};
use crate::streams::{DestinationStream, StreamInfo};
use crate::util::{bool_result, wide_null};

/// Attributes that are user-copyable under the PLAN.md section 4.2 contract.
pub const COPYABLE_ATTRIBUTES: u32 = FILE_ATTRIBUTE_READONLY
    | FILE_ATTRIBUTE_HIDDEN
    | FILE_ATTRIBUTE_SYSTEM
    | FILE_ATTRIBUTE_ARCHIVE
    | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;

/// Returns whether enumeration attributes describe a compressed source.
#[must_use]
pub const fn is_compressed(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_COMPRESSED != 0
}

/// Returns whether enumeration attributes describe a sparse source.
#[must_use]
pub const fn is_sparse(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0
}

/// Returns whether enumeration attributes describe an EFS-encrypted source.
#[must_use]
pub const fn is_encrypted(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_ENCRYPTED != 0
}

/// Returns whether attributes describe data that may hydrate on access.
#[must_use]
pub const fn is_cloud_placeholder(attributes: u32) -> bool {
    attributes
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            | FILE_ATTRIBUTE_RECALL_ON_OPEN)
        != 0
}

// FILE_RENAME_IGNORE_READONLY_ATTRIBUTE is currently exposed by windows-sys
// only in its kernel bindings. This is the documented user-mode flag value for
// FILE_RENAME_INFO_EX, named here rather than repeated at call sites.
const FILE_RENAME_FLAG_IGNORE_READONLY_ATTRIBUTE: u32 = 0x40;

/// Read-only source handle plus the metadata observed at open time.
pub struct SourceFile {
    file: File,
    opened_metadata: ObjectMetadata,
}

impl SourceFile {
    /// Opens a source with read access and maximum sharing compatibility.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options
            .access_mode(GENERIC_READ | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN | FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        let opened_metadata = metadata_from_file(&file)?;
        if opened_metadata.kind != ObjectKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ordinary source path changed to a reparse point or directory",
            ));
        }
        Ok(Self {
            file,
            opened_metadata,
        })
    }

    /// Returns the metadata captured from the opened handle.
    #[must_use]
    pub const fn opened_metadata(&self) -> &ObjectMetadata {
        &self.opened_metadata
    }

    /// Re-queries the same handle for the source-stability post-read check.
    pub fn current_metadata(&self) -> io::Result<ObjectMetadata> {
        metadata_from_file(&self.file)
    }

    /// Returns the allocated logical ranges of a sparse source.
    pub fn allocated_ranges(&self, logical_size: u64) -> io::Result<Vec<AllocatedRange>> {
        query_allocated_ranges(&self.file, logical_size)
    }
}

impl Read for SourceFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for SourceFile {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

/// A bigcp-owned temporary destination.
///
/// Dropping this value before commit removes only the opaque path it created.
/// No public constructor accepts an arbitrary existing path.
pub struct DestinationTemp {
    file: Option<File>,
    path: PathBuf,
    persistent: bool,
}

impl DestinationTemp {
    /// Creates a new opaque sibling and arms delete-on-close immediately.
    ///
    /// `encrypted` requests EFS at creation time — the only reliable moment:
    /// once the temp exists with an armed delete disposition, `EncryptFileW`'s
    /// internal path-based open is refused, so post-creation encryption can
    /// never succeed on a live temp.
    pub fn create(parent: &Path, run_id: &str, encrypted: bool) -> io::Result<Self> {
        for _ in 0..128 {
            let nonce = Uuid::new_v4().simple().to_string();
            let path = parent.join(format!(".bigcp-{run_id}-{}.part", &nonce[..12]));
            let mut options = OpenOptions::new();
            options
                // Keep std's creation validation in sync with the explicit
                // Win32 access mask below.
                .read(true)
                .write(true)
                .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE | FILE_WRITE_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .create_new(true);
            if encrypted {
                options.attributes(FILE_ATTRIBUTE_ENCRYPTED);
            }
            match options.open(&path) {
                Ok(file) => {
                    if let Err(error) = set_delete_on_close(&file, true) {
                        drop(file);
                        // Prefer a harmless opaque orphan over path-based
                        // cleanup after closing the identity-owning handle.
                        // Another actor could replace the name in that gap.
                        return Err(error);
                    }
                    return Ok(Self {
                        file: Some(file),
                        path,
                        persistent: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique bigcp temporary name",
        ))
    }

    /// Reopens a journal-owned sibling temporary for verified resume.
    ///
    /// Only bigcp's opaque ASCII naming shape is accepted, and `name` must be
    /// exactly one normal path component. The final component is opened
    /// without following a reparse point and must be an ordinary file. The
    /// caller still must verify its identity and contents before writing.
    pub fn resume(parent: &Path, name: &str) -> io::Result<Self> {
        // Enforce the exact opaque shape the creator produces. In particular
        // a `:` must never pass: `.bigcp-x:y.part` would open the alternate
        // stream `y.part` of a *different* file, and commit would then rename
        // that host file over the final destination name.
        let valid_shape = name
            .strip_prefix(".bigcp-")
            .and_then(|rest| rest.strip_suffix(".part"))
            .is_some_and(|body| {
                let mut parts = body.rsplitn(2, '-');
                let nonce = parts.next().unwrap_or_default();
                let run_id = parts.next().unwrap_or_default();
                !run_id.is_empty()
                    && run_id
                        .chars()
                        .all(|value| value.is_ascii_alphanumeric() || value == '-')
                    && (1..=32).contains(&nonce.len())
                    && nonce.chars().all(|value| value.is_ascii_hexdigit())
            });
        let relative = Path::new(name);
        if !valid_shape || relative.file_name().is_none_or(|value| value != name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal temporary name is not a safe bigcp sibling name",
            ));
        }
        let path = parent.join(relative);
        let mut options = OpenOptions::new();
        options
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE | FILE_WRITE_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(&path)?;
        if metadata_from_file(&file)?.kind != ObjectKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "journal temporary is not an ordinary file",
            ));
        }
        Ok(Self {
            file: Some(file),
            path,
            persistent: true,
        })
    }

    /// Returns the opaque path for journal and diagnostic records.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the stable filesystem identity of the opened temporary.
    pub fn identity(&self) -> io::Result<FileIdentity> {
        Ok(metadata_from_file(self.file_ref()?)?.identity)
    }

    /// Returns the current logical size of the owned temporary.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file_ref()?.metadata()?.len())
    }

    /// Returns whether the temporary currently has zero logical length.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Reports whether a CRC-valid journal is now responsible for cleanup.
    #[must_use]
    pub const fn is_persistent(&self) -> bool {
        self.persistent
    }

    /// Converts pending-delete ownership into journal-backed persistence.
    pub fn persist_for_resume(&mut self) -> io::Result<()> {
        if !self.persistent {
            set_delete_on_close(self.file_ref()?, false)?;
            self.persistent = true;
        }
        Ok(())
    }

    /// Discards a CRC-journal-proven temporary that failed prefix validation.
    pub fn discard(mut self) -> io::Result<()> {
        set_delete_on_close(self.file_ref()?, true)?;
        self.persistent = false;
        Ok(())
    }

    /// Sets the logical end of the temporary file.
    pub fn set_len(&self, length: u64) -> io::Result<()> {
        self.file_ref()?.set_len(length)
    }

    /// Reserves dense allocation without changing logical EOF.
    pub fn preallocate(&self, length: u64) -> io::Result<()> {
        let info = FILE_ALLOCATION_INFO {
            AllocationSize: i64::try_from(length)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file is too large"))?,
        };
        // SAFETY: info is the exact structure for FileAllocationInfo, and the
        // owned temp handle stays valid for the call.
        unsafe {
            bool_result(SetFileInformationByHandle(
                self.file_ref()?.as_raw_handle().cast(),
                FileAllocationInfo,
                (&raw const info).cast(),
                size_u32::<FILE_ALLOCATION_INFO>()?,
            ))
        }
    }

    /// Marks this opaque temporary sparse before establishing logical EOF.
    pub fn mark_sparse(&self) -> io::Result<()> {
        mark_sparse(self.file_ref()?)
    }

    /// Creates a named data stream attached to this owned temporary.
    pub fn create_stream(&self, stream: &StreamInfo) -> io::Result<DestinationStream> {
        // Windows refuses a new stream open while the base file has a pending
        // delete disposition. Clear it only for the open syscall and re-arm it
        // before returning the stream handle; the final name remains absent.
        let armed = !self.persistent;
        if armed {
            set_delete_on_close(self.file_ref()?, false)?;
        }
        let opened = DestinationStream::create_reparse(&self.path, stream, true);
        if armed && let Err(error) = set_delete_on_close(self.file_ref()?, true) {
            drop(opened);
            return Err(error);
        }
        opened
    }

    /// Reopens one named stream belonging to this journal-owned temporary.
    pub fn resume_stream(&self, stream: &StreamInfo) -> io::Result<DestinationStream> {
        DestinationStream::create_reparse(&self.path, stream, false)
    }

    /// Applies an explicitly protected DACL captured from a replace target.
    pub fn apply_protected_dacl(&self, dacl: &ProtectedDacl) -> io::Result<()> {
        dacl.apply_to(self.file_ref()?)
    }

    /// Writes source EAs while this file still has only its opaque temp name.
    pub fn write_extended_attributes(&self, attributes: &ExtendedAttributes) -> io::Result<()> {
        write_to_file(self.file_ref()?, attributes)
    }

    /// Reports the temporary's current basic attributes (EFS state check).
    pub fn basic_attributes(&self) -> io::Result<u32> {
        Ok(metadata_from_file(self.file_ref()?)?.basic.attributes)
    }

    /// Atomically publishes the completed file, sets metadata, optionally
    /// flushes it, and closes the handle before reporting success.
    pub fn commit(
        mut self,
        final_path: &Path,
        replace: bool,
        metadata: BasicMetadata,
        flush: bool,
    ) -> io::Result<()> {
        let file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("temporary handle already consumed"))?;
        // No pre-rename flush: `File` I/O is unbuffered at this layer (a
        // `flush()` here would be a misleading no-op), and durable-mode
        // flushing deliberately happens *after* rename+metadata so the final
        // state is what gets flushed (PLAN section 4.3 step 5).
        set_delete_on_close(&file, false)?;
        if let Err(error) = rename_by_handle(&file, final_path, replace) {
            if !self.persistent {
                let _ = set_delete_on_close(&file, true);
            }
            self.file = Some(file);
            return Err(error);
        }

        // From this instant the handle names the final file. A metadata error
        // must never trigger deletion of that user-visible name.
        self.path = final_path.to_path_buf();
        set_basic_by_handle(&file, metadata)?;
        if flush {
            // SAFETY: file owns a valid file handle for this call.
            unsafe {
                bool_result(FlushFileBuffers(file.as_raw_handle().cast()))?;
            }
        }
        close_file(file)
    }

    fn file_ref(&self) -> io::Result<&File> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("temporary handle already consumed"))
    }

    fn file_mut(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("temporary handle already consumed"))
    }
}

impl Write for DestinationTemp {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file_mut()?.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file_mut()?.flush()
    }
}

impl Read for DestinationTemp {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file_mut()?.read(buffer)
    }
}

impl Seek for DestinationTemp {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.file_mut()?.seek(position)
    }
}

impl Drop for DestinationTemp {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            drop(file);
        }
        // Non-persistent temporaries are deleted by the disposition attached
        // to this exact handle. Never fall back to deleting by path after the
        // handle closes: the opaque name could have been replaced meanwhile.
    }
}

/// Creates one real directory without traversing an existing object.
pub fn create_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// Applies basic metadata to a file or directory after its final name exists.
pub fn set_basic_at(path: &Path, metadata: BasicMetadata) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    set_basic_by_handle(&file, metadata)
}

/// Applies basic metadata only when the opened object has the expected ID.
///
/// Identity validation and mutation use the same non-following handle, which
/// closes the stable path-replacement window around directory metadata repair.
pub fn set_basic_at_checked(
    path: &Path,
    expected: FileIdentity,
    metadata: BasicMetadata,
) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if metadata_from_file(&file)?.identity != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object identity changed before metadata update",
        ));
    }
    set_basic_by_handle(&file, metadata)
}

/// Atomically replaces one audit artifact with a sibling temporary.
///
/// This wrapper is intentionally path-based because JSON report publication
/// is outside the destination tree and `MoveFileExW` supplies atomic replace
/// semantics for an existing report. The caller must own `temporary`.
pub fn publish_audit_temporary(temporary: &Path, final_path: &Path) -> io::Result<()> {
    let temporary = wide_null(temporary.as_os_str());
    let final_path = wide_null(final_path.as_os_str());
    // SAFETY: both paths are nul-terminated and live for the synchronous call.
    unsafe {
        bool_result(MoveFileExW(
            temporary.as_ptr(),
            final_path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        ))
    }
}

pub(crate) fn set_delete_on_close(file: &File, enabled: bool) -> io::Result<()> {
    // The legacy information class is deliberate: it provides the exact
    // delete-on-close lifecycle bigcp needs on both NTFS and ReFS, while the
    // POSIX disposition flags are optional filesystem features and can return
    // ERROR_NOT_SUPPORTED even on otherwise-supported volumes.
    let info = FILE_DISPOSITION_INFO {
        DeleteFile: enabled,
    };
    // SAFETY: info is the exact structure required by this information class,
    // and file owns the handle for the duration of the call.
    unsafe {
        bool_result(SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size_u32::<FILE_DISPOSITION_INFO>()?,
        ))
    }
}

pub(crate) fn rename_by_handle(file: &File, final_path: &Path, replace: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let name: Vec<u16> = final_path.as_os_str().encode_wide().collect();
    let name_bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename path is too long"))?;
    // Allocate the complete Win32 structure plus the variable name and a
    // zeroed UTF-16 guard. Some filesystem drivers validate against the fixed
    // structure size (which includes FileName[1]) rather than its field
    // offset; using the full size also prevents a driver from observing bytes
    // beyond our initialized allocation.
    let total = size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .and_then(|value| value.checked_add(size_of::<u16>()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer overflow"))?;
    let mut buffer = vec![0_u64; total.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let mut flags = FILE_RENAME_FLAG_POSIX_SEMANTICS | FILE_RENAME_FLAG_IGNORE_READONLY_ATTRIBUTE;
    if replace {
        flags |= FILE_RENAME_FLAG_REPLACE_IF_EXISTS;
    }
    // SAFETY: buffer is large enough for the fixed fields plus every UTF-16
    // code unit. info is naturally aligned because the allocator aligns Vec.
    unsafe {
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = u32::try_from(name_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename path is too long"))?;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        bool_result(SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileRenameInfoEx,
            buffer.as_ptr().cast(),
            u32::try_from(total).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "rename buffer overflow")
            })?,
        ))
    }
}

pub(crate) fn set_basic_by_handle(file: &File, metadata: BasicMetadata) -> io::Result<()> {
    let attributes = metadata.attributes & COPYABLE_ATTRIBUTES;
    let info = FILE_BASIC_INFO {
        CreationTime: metadata.creation_time,
        LastAccessTime: metadata.last_access_time,
        LastWriteTime: metadata.last_write_time,
        ChangeTime: 0,
        FileAttributes: if attributes == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attributes
        },
    };
    // SAFETY: info has the exact layout required by FileBasicInfo and the
    // borrowed file keeps its handle alive.
    unsafe {
        bool_result(SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileBasicInfo,
            (&raw const info).cast(),
            size_u32::<FILE_BASIC_INFO>()?,
        ))
    }
}

pub(crate) fn close_file(file: File) -> io::Result<()> {
    let raw = file.into_raw_handle();
    // SAFETY: into_raw_handle transferred sole ownership to this function.
    unsafe { bool_result(CloseHandle(raw.cast())) }
}

fn size_u32<T>() -> io::Result<u32> {
    u32::try_from(size_of::<T>())
        .map_err(|_| io::Error::other("Win32 structure size does not fit u32"))
}

#[cfg(test)]
mod tests {
    use super::{DestinationTemp, SourceFile};
    use crate::metadata::BasicMetadata;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::windows::fs::symlink_file;

    #[test]
    fn uncommitted_temp_is_deleted_by_its_handle() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let temp = DestinationTemp::create(sandbox.path(), "delete-on-close", false);
        assert!(temp.is_ok(), "temporary creation failed: {:?}", temp.err());
        let Some(temp) = temp.ok() else {
            return;
        };
        let path = temp.path().to_path_buf();
        assert!(path.exists());
        drop(temp);
        assert!(!path.exists());
    }

    #[test]
    fn resume_rejects_a_directory_with_an_opaque_temp_name() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let name = ".bigcp-stale-000000000000.part";
        assert!(fs::create_dir(sandbox.path().join(name)).is_ok());
        let result = DestinationTemp::resume(sandbox.path(), name);
        assert!(result.is_err());
        assert!(sandbox.path().join(name).is_dir());
    }

    #[test]
    fn resumed_temp_identity_detects_name_reuse_without_harming_replacement() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let temp = DestinationTemp::create(sandbox.path(), "identity-reuse", false);
        assert!(temp.is_ok());
        let Some(mut temp) = temp.ok() else {
            return;
        };
        let original_identity = temp.identity();
        let path = temp.path().to_path_buf();
        let name = path.file_name().and_then(|value| value.to_str());
        assert!(original_identity.is_ok());
        assert!(name.is_some());
        assert!(temp.persist_for_resume().is_ok());
        drop(temp);
        assert!(fs::remove_file(&path).is_ok());
        assert!(fs::write(&path, b"unrelated replacement").is_ok());

        let resumed = name.and_then(|name| DestinationTemp::resume(sandbox.path(), name).ok());
        assert!(resumed.is_some());
        assert!(
            resumed
                .as_ref()
                .is_some_and(|candidate| { candidate.identity().ok() != original_identity.ok() })
        );
        drop(resumed);
        assert_eq!(fs::read(path).ok(), Some(b"unrelated replacement".to_vec()));
    }

    #[test]
    fn ordinary_source_open_never_follows_a_reparse_point() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let target = sandbox.path().join("target.txt");
        let link = sandbox.path().join("link.txt");
        assert!(fs::write(&target, b"must not be read through the link").is_ok());
        if symlink_file(&target, &link).is_err() {
            return;
        }
        assert!(SourceFile::open(&link).is_err());
        assert_eq!(
            fs::read(target).ok(),
            Some(b"must not be read through the link".to_vec())
        );
    }

    #[test]
    fn nonreplacing_commit_preserves_a_dangling_link_collision() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let final_path = sandbox.path().join("final.txt");
        if symlink_file(sandbox.path().join("missing-target"), &final_path).is_err() {
            return;
        }
        let temp = DestinationTemp::create(sandbox.path(), "new-collision", false);
        assert!(temp.is_ok());
        let Some(mut temp) = temp.ok() else {
            return;
        };
        assert!(temp.write_all(b"new data").is_ok());
        let result = temp.commit(
            &final_path,
            false,
            BasicMetadata {
                creation_time: 0,
                last_access_time: 0,
                last_write_time: 0,
                attributes: 0,
            },
            false,
        );
        assert!(result.is_err());
        assert!(fs::symlink_metadata(&final_path).is_ok_and(|value| value.is_symlink()));
        assert_eq!(
            fs::read_link(final_path).ok(),
            Some(sandbox.path().join("missing-target"))
        );
    }

    #[test]
    fn temp_is_not_visible_under_final_name_until_commit() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let final_path = sandbox.path().join("final.bin");
        let temp = DestinationTemp::create(sandbox.path(), "12345678", false);
        assert!(temp.is_ok(), "temporary creation failed: {:?}", temp.err());
        let Some(mut temp) = temp.ok() else {
            return;
        };
        assert!(temp.write_all(b"complete").is_ok());
        assert!(!final_path.exists());
        let metadata = BasicMetadata {
            creation_time: 0,
            last_access_time: 0,
            last_write_time: 0,
            attributes: 0,
        };
        assert!(temp.commit(&final_path, false, metadata, false).is_ok());
        assert_eq!(fs::read(final_path).ok(), Some(b"complete".to_vec()));
    }

    #[test]
    fn source_open_is_readable_and_stable() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let source_path = sandbox.path().join("source.bin");
        assert!(fs::write(&source_path, b"source").is_ok());
        let source = SourceFile::open(&source_path);
        assert!(source.is_ok());
        let Some(mut source) = source.ok() else {
            return;
        };
        let mut bytes = Vec::new();
        assert!(source.read_to_end(&mut bytes).is_ok());
        assert_eq!(bytes, b"source");
        assert_eq!(source.opened_metadata().size, 6);
    }
}
