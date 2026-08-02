//! Local and remote volume capability probing for pre-flight policy.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Wdk::Storage::FileSystem::{
    FILE_FS_ATTRIBUTE_INFORMATION, FileFsAttributeInformation, FileFsSizeInformation,
    FileFsVolumeInformation, NtQueryVolumeInformationFile,
};
use windows_sys::Wdk::System::SystemServices::{
    FILE_FS_SIZE_INFORMATION, FILE_FS_VOLUME_INFORMATION,
};
use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetVolumeInformationW,
    GetVolumePathNameW,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::{
    FILE_NAMED_STREAMS, FILE_PERSISTENT_ACLS, FILE_SUPPORTS_BLOCK_REFCOUNTING,
    FILE_SUPPORTS_ENCRYPTION, FILE_SUPPORTS_EXTENDED_ATTRIBUTES, FILE_SUPPORTS_POSIX_UNLINK_RENAME,
    FILE_SUPPORTS_REPARSE_POINTS, FILE_SUPPORTS_SPARSE_FILES,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_NO_ROOT_DIR, DRIVE_REMOTE};

use crate::endpoint::{EndpointKind, classify_endpoint};
use crate::metadata::open_root;
use crate::path::final_path;
use crate::util::{bool_result, wide_null};

/// Supported filesystem families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileSystem {
    /// Microsoft NTFS.
    Ntfs,
    /// Microsoft Resilient File System.
    Refs,
    /// The classic FAT family (FAT12, FAT16, or FAT32).
    Fat,
    /// Microsoft Extended FAT.
    ExFat,
    /// Linux filesystem exposed through WSL's UNC provider.
    Wsl,
    /// Capability-probed remote filesystem without known timestamp semantics.
    Remote,
}

impl FileSystem {
    /// Stable user-facing filesystem name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ntfs => "NTFS",
            Self::Refs => "ReFS",
            Self::Fat => "FAT/FAT32",
            Self::ExFat => "exFAT",
            Self::Wsl => "WSL/Linux",
            Self::Remote => "remote filesystem",
        }
    }

    /// Whether this filesystem requires representational degradation.
    #[must_use]
    pub const fn is_fat_family(self) -> bool {
        matches!(self, Self::Fat | Self::ExFat)
    }

    /// Maximum representable unnamed-stream size, when bounded below `u64`.
    #[must_use]
    pub const fn maximum_file_size(self) -> Option<u64> {
        match self {
            // FAT directory entries carry a 32-bit byte length. The all-ones
            // value is the largest byte count that can be represented.
            Self::Fat => Some(4_294_967_295),
            Self::Ntfs | Self::Refs | Self::ExFat | Self::Wsl | Self::Remote => None,
        }
    }
}

/// Capability flags queried from the mounted destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeCapabilities {
    /// Named alternate data stream support.
    pub named_streams: bool,
    /// Extended attribute support.
    pub extended_attributes: bool,
    /// Sparse-file support.
    pub sparse_files: bool,
    /// EFS encryption support.
    pub encryption: bool,
    /// Reparse-point support.
    pub reparse_points: bool,
    /// Block-refcounting support, used only for the same-volume ReFS hint.
    pub block_refcounting: bool,
    /// Persistent security descriptor / ACL support.
    pub persistent_acls: bool,
    /// POSIX-style unlink/rename support for replacement publication.
    pub posix_unlink_rename: bool,
}

impl VolumeCapabilities {
    /// Conservative capability set for providers whose Linux semantics cannot
    /// be represented by bigcp's Win32 metadata engine.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            named_streams: false,
            extended_attributes: false,
            sparse_files: false,
            encryption: false,
            reparse_points: false,
            block_refcounting: false,
            persistent_acls: false,
            posix_unlink_rename: false,
        }
    }
}

/// Immutable facts used by pre-flight, tuning, and reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeInfo {
    /// Mounted volume root such as an extended C drive root.
    pub root: PathBuf,
    /// Whether this root is local, generic UNC, or WSL UNC.
    pub endpoint: EndpointKind,
    /// Filesystem family.
    pub filesystem: FileSystem,
    /// Volume serial number.
    pub serial: u32,
    /// Maximum component length in UTF-16 code units.
    pub maximum_component_length: u32,
    /// Logical sector size.
    pub bytes_per_sector: u32,
    /// Allocation unit size.
    pub cluster_size: u64,
    /// Bytes available to the current caller.
    pub free_bytes_available: u64,
    /// Total volume capacity.
    pub total_bytes: u64,
    /// Filesystem capabilities.
    pub capabilities: VolumeCapabilities,
}

/// Probes one existing local or UNC path.
///
/// Local roots retain the original volume-management API path. Remote roots
/// use handle-bound native volume queries because SMB deliberately does not
/// implement Win32 volume-management functions.
pub fn probe_volume(path: &Path) -> io::Result<VolumeInfo> {
    let input = wide_null(path.as_os_str())?;
    let mut volume_path = vec![0_u16; 32_768];
    // SAFETY: both UTF-16 buffers are valid for the documented lengths.
    unsafe {
        bool_result(GetVolumePathNameW(
            input.as_ptr(),
            volume_path.as_mut_ptr(),
            u32::try_from(volume_path.len())
                .map_err(|_| io::Error::other("volume path buffer exceeds u32"))?,
        ))?;
    }
    truncate_nul(&mut volume_path);
    let volume_root = PathBuf::from(OsString::from_wide(&volume_path));
    let endpoint = classify_endpoint(&volume_root);
    let volume_root_wide = wide_null(volume_root.as_os_str())?;

    // SAFETY: volume_root_wide is a live, nul-terminated string.
    let drive_type = unsafe { GetDriveTypeW(volume_root_wide.as_ptr()) };
    if drive_type == DRIVE_REMOTE || endpoint.is_remote() {
        return probe_remote_volume(volume_root, endpoint);
    }
    if drive_type == DRIVE_NO_ROOT_DIR {
        // GetDriveTypeW does not set the thread's last error; surfacing
        // `last_error()` here would report an unrelated stale code.
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "volume root not found",
        ));
    }

    let mut serial = 0_u32;
    let mut max_component = 0_u32;
    let mut flags = 0_u32;
    let mut filesystem = vec![0_u16; 64];
    // SAFETY: optional volume-name output is null; all other pointers target
    // initialized writable scalars or the declared filesystem buffer.
    unsafe {
        bool_result(GetVolumeInformationW(
            volume_root_wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &raw mut serial,
            &raw mut max_component,
            &raw mut flags,
            filesystem.as_mut_ptr(),
            u32::try_from(filesystem.len())
                .map_err(|_| io::Error::other("filesystem buffer exceeds u32"))?,
        ))?;
    }
    truncate_nul(&mut filesystem);
    let filesystem_name = String::from_utf16_lossy(&filesystem);
    let filesystem = known_filesystem(&filesystem_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "bigcp supports NTFS, ReFS, FAT/FAT32, and exFAT local volumes; found {filesystem_name}"
            ),
        )
    })?;

    let mut sectors_per_cluster = 0_u32;
    let mut bytes_per_sector = 0_u32;
    let mut free_clusters = 0_u32;
    let mut total_clusters = 0_u32;
    let mut free_for_caller = 0_u64;
    let mut total_bytes = 0_u64;
    let mut total_free = 0_u64;
    // SAFETY: output pointers are valid scalars and the root string stays live.
    unsafe {
        bool_result(GetDiskFreeSpaceW(
            volume_root_wide.as_ptr(),
            &raw mut sectors_per_cluster,
            &raw mut bytes_per_sector,
            &raw mut free_clusters,
            &raw mut total_clusters,
        ))?;
        bool_result(GetDiskFreeSpaceExW(
            volume_root_wide.as_ptr(),
            &raw mut free_for_caller,
            &raw mut total_bytes,
            &raw mut total_free,
        ))?;
    }

    Ok(VolumeInfo {
        root: volume_root,
        endpoint,
        filesystem,
        serial,
        maximum_component_length: max_component,
        bytes_per_sector,
        cluster_size: u64::from(sectors_per_cluster) * u64::from(bytes_per_sector),
        free_bytes_available: free_for_caller,
        total_bytes,
        capabilities: capabilities_from_flags(flags),
    })
}

fn probe_remote_volume(root: PathBuf, endpoint: EndpointKind) -> io::Result<VolumeInfo> {
    let handle = open_root(&root)?;
    let endpoint = if endpoint == EndpointKind::Local {
        // GetVolumePathNameW returns a drive-letter root for a mapped drive.
        // Resolve that already-open remote handle once so mapped WSL drives
        // retain WSL case/capability policy; other mapped drives are UNC.
        let resolved = final_path(&handle).ok();
        effective_remote_endpoint(endpoint, resolved.as_deref())
    } else {
        endpoint
    };
    let attributes = query_remote_attributes(&handle)?;
    let serial = query_remote_serial(&handle)?;
    let size = query_remote_struct::<FILE_FS_SIZE_INFORMATION>(&handle, FileFsSizeInformation)?;
    let bytes_per_sector = size.BytesPerSector;
    let cluster_size = u64::from(size.SectorsPerAllocationUnit)
        .checked_mul(u64::from(bytes_per_sector))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "remote cluster size overflow")
        })?;
    if bytes_per_sector == 0 || cluster_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote filesystem reported a zero sector or cluster size",
        ));
    }
    let total_units = u64::try_from(size.TotalAllocationUnits).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "remote filesystem reported negative total allocation units",
        )
    })?;
    let available_units = u64::try_from(size.AvailableAllocationUnits).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "remote filesystem reported negative available allocation units",
        )
    })?;
    let total_bytes = total_units
        .checked_mul(cluster_size)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "remote size overflow"))?;
    let free_bytes_available = available_units
        .checked_mul(cluster_size)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "remote free-space overflow"))?;
    let filesystem = if endpoint == EndpointKind::Wsl {
        FileSystem::Wsl
    } else {
        known_filesystem(&attributes.filesystem_name).unwrap_or(FileSystem::Remote)
    };
    let flags = attributes.flags;
    let capabilities = if endpoint == EndpointKind::Wsl {
        // The 9P provider can expose Linux objects as Windows reparse data,
        // but bigcp cannot yet recreate Linux uid/gid/mode/xattrs or special
        // files through its Win32 semantic engine. Do not mistake provider
        // visibility for destination representability.
        VolumeCapabilities::none()
    } else {
        capabilities_from_flags(flags)
    };
    Ok(VolumeInfo {
        root,
        endpoint,
        filesystem,
        serial,
        maximum_component_length: u32::try_from(attributes.maximum_component_length).map_err(
            |_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote maximum component length was negative",
                )
            },
        )?,
        bytes_per_sector,
        cluster_size,
        free_bytes_available,
        total_bytes,
        capabilities,
    })
}

fn effective_remote_endpoint(probed: EndpointKind, resolved: Option<&Path>) -> EndpointKind {
    if probed.is_remote() {
        return probed;
    }
    resolved.map_or(EndpointKind::Unc, |path| match classify_endpoint(path) {
        EndpointKind::Local => EndpointKind::Unc,
        endpoint => endpoint,
    })
}

struct RemoteAttributes {
    flags: u32,
    maximum_component_length: i32,
    filesystem_name: String,
}

fn query_remote_attributes(handle: &File) -> io::Result<RemoteAttributes> {
    // Filesystem names are normally tiny ("NTFS", "9P", "lxfs"). A fixed
    // page keeps this query allocation bounded while leaving ample room for a
    // third-party redirector's identifier.
    let mut words = vec![0_u64; 4096 / size_of::<u64>()];
    let returned = query_remote_bytes(handle, FileFsAttributeInformation, &mut words)?;
    let name_offset = std::mem::offset_of!(FILE_FS_ATTRIBUTE_INFORMATION, FileSystemName);
    validate_query_length(returned, words.len() * size_of::<u64>(), name_offset)?;
    let info = words.as_ptr().cast::<FILE_FS_ATTRIBUTE_INFORMATION>();
    // SAFETY: the buffer is eight-byte aligned and at least one page. The
    // native query succeeded before these fixed fields are read.
    let (flags, maximum_component_length, name_bytes) = unsafe {
        (
            (*info).FileSystemAttributes,
            (*info).MaximumComponentNameLength,
            usize::try_from((*info).FileSystemNameLength)
                .map_err(|_| io::Error::other("remote filesystem name is too long"))?,
        )
    };
    if !name_bytes.is_multiple_of(size_of::<u16>())
        || name_offset
            .checked_add(name_bytes)
            .is_none_or(|end| end > returned)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote filesystem returned an invalid name length",
        ));
    }
    // SAFETY: the variable name bounds and UTF-16 alignment were validated.
    // The pointer derives from the backing buffer, not from a reference to
    // the one-element FileSystemName array, so its provenance spans the
    // entire variable-length name.
    let name = unsafe {
        std::slice::from_raw_parts(
            words
                .as_ptr()
                .cast::<u16>()
                .add(name_offset / size_of::<u16>()),
            name_bytes / size_of::<u16>(),
        )
    };
    Ok(RemoteAttributes {
        flags,
        maximum_component_length,
        filesystem_name: String::from_utf16_lossy(name),
    })
}

fn query_remote_serial(handle: &File) -> io::Result<u32> {
    // FILE_FS_VOLUME_INFORMATION ends in a variable-length volume label. A
    // fixed `size_of` query works only for an empty/one-code-unit label and
    // would reject ordinary SMB shares. One bounded page covers Windows label
    // limits while preserving eight-byte alignment for the fixed header.
    let mut words = vec![0_u64; 4096 / size_of::<u64>()];
    let returned = query_remote_bytes(handle, FileFsVolumeInformation, &mut words)?;
    validate_query_length(
        returned,
        words.len() * size_of::<u64>(),
        std::mem::offset_of!(FILE_FS_VOLUME_INFORMATION, VolumeLabel),
    )?;
    let info = words.as_ptr().cast::<FILE_FS_VOLUME_INFORMATION>();
    // SAFETY: the aligned page is larger than the fixed portion of the native
    // structure, and the query succeeded before this field is read.
    Ok(unsafe { (*info).VolumeSerialNumber })
}

fn query_remote_struct<T: Default>(handle: &File, class: i32) -> io::Result<T> {
    let mut value = T::default();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: value is an initialized concrete structure of the length passed,
    // the root handle remains live, and the operation is a synchronous query.
    let result = unsafe {
        NtQueryVolumeInformationFile(
            handle.as_raw_handle().cast(),
            &raw mut status,
            (&raw mut value).cast(),
            u32::try_from(size_of::<T>())
                .map_err(|_| io::Error::other("remote query structure is too large"))?,
            class,
        )
    };
    nt_result(result)?;
    validate_query_length(status.Information, size_of::<T>(), size_of::<T>())?;
    Ok(value)
}

fn query_remote_bytes(handle: &File, class: i32, words: &mut [u64]) -> io::Result<usize> {
    let mut status = IO_STATUS_BLOCK::default();
    let length = words
        .len()
        .checked_mul(size_of::<u64>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| io::Error::other("remote query buffer is too large"))?;
    // SAFETY: words is writable and naturally aligned, the handle remains
    // live, and the native call completes synchronously.
    let result = unsafe {
        NtQueryVolumeInformationFile(
            handle.as_raw_handle().cast(),
            &raw mut status,
            words.as_mut_ptr().cast(),
            length,
            class,
        )
    };
    nt_result(result)?;
    validate_query_length(status.Information, length as usize, 0)?;
    Ok(status.Information)
}

fn validate_query_length(returned: usize, capacity: usize, minimum: usize) -> io::Result<()> {
    if returned < minimum || returned > capacity {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote filesystem returned an invalid query byte count",
        ))
    } else {
        Ok(())
    }
}

fn nt_result(status: i32) -> io::Result<()> {
    if status >= 0 {
        Ok(())
    } else {
        // SAFETY: this pure ntdll conversion accepts any NTSTATUS value.
        let code = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(
            i32::try_from(code).unwrap_or(i32::MAX),
        ))
    }
}

fn known_filesystem(name: &str) -> Option<FileSystem> {
    if name.eq_ignore_ascii_case("NTFS") {
        Some(FileSystem::Ntfs)
    } else if name.eq_ignore_ascii_case("ReFS") {
        Some(FileSystem::Refs)
    } else if name.eq_ignore_ascii_case("FAT") || name.eq_ignore_ascii_case("FAT32") {
        Some(FileSystem::Fat)
    } else if name.eq_ignore_ascii_case("exFAT") {
        Some(FileSystem::ExFat)
    } else {
        None
    }
}

fn capabilities_from_flags(flags: u32) -> VolumeCapabilities {
    VolumeCapabilities {
        named_streams: flags & FILE_NAMED_STREAMS != 0,
        extended_attributes: flags & FILE_SUPPORTS_EXTENDED_ATTRIBUTES != 0,
        sparse_files: flags & FILE_SUPPORTS_SPARSE_FILES != 0,
        encryption: flags & FILE_SUPPORTS_ENCRYPTION != 0,
        reparse_points: flags & FILE_SUPPORTS_REPARSE_POINTS != 0,
        block_refcounting: flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0,
        persistent_acls: flags & FILE_PERSISTENT_ACLS != 0,
        posix_unlink_rename: flags & FILE_SUPPORTS_POSIX_UNLINK_RENAME != 0,
    }
}

fn truncate_nul(buffer: &mut Vec<u16>) {
    if let Some(index) = buffer.iter().position(|value| *value == 0) {
        buffer.truncate(index);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EndpointKind, FileSystem, effective_remote_endpoint, probe_volume, validate_query_length,
    };
    use std::path::Path;

    #[test]
    fn system_temp_is_on_a_supported_local_volume() {
        let path = std::env::temp_dir();
        let result = probe_volume(&path);
        assert!(result.is_ok());
        let Some(info) = result.ok() else {
            return;
        };
        assert!(matches!(
            info.filesystem,
            FileSystem::Ntfs | FileSystem::Refs | FileSystem::Fat | FileSystem::ExFat
        ));
        assert_eq!(info.endpoint, EndpointKind::Local);
        assert!(info.cluster_size > 0);
    }

    #[test]
    fn fat_size_limit_is_explicit_and_exfat_is_not_artificially_capped() {
        assert_eq!(
            FileSystem::Fat.maximum_file_size(),
            Some(u64::from(u32::MAX))
        );
        assert_eq!(FileSystem::ExFat.maximum_file_size(), None);
        assert!(FileSystem::Fat.is_fat_family());
        assert!(FileSystem::ExFat.is_fat_family());
        assert!(!FileSystem::Ntfs.is_fat_family());
    }

    #[test]
    fn mapped_remote_drives_resolve_to_unc_or_wsl_endpoint_policy() {
        assert_eq!(
            effective_remote_endpoint(
                EndpointKind::Local,
                Some(Path::new(r"\\?\UNC\server\share"))
            ),
            EndpointKind::Unc
        );
        assert_eq!(
            effective_remote_endpoint(
                EndpointKind::Local,
                Some(Path::new(r"\\?\UNC\wsl.localhost\Ubuntu"))
            ),
            EndpointKind::Wsl
        );
        assert_eq!(
            effective_remote_endpoint(EndpointKind::Local, None),
            EndpointKind::Unc
        );
    }

    #[test]
    fn remote_query_lengths_stay_within_initialized_output() {
        assert!(validate_query_length(16, 64, 8).is_ok());
        assert!(validate_query_length(7, 64, 8).is_err());
        assert!(validate_query_length(65, 64, 8).is_err());
    }
}
