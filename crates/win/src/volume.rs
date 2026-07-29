//! Volume capability probing and the NTFS/ReFS/local-volume pre-flight gate.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetVolumeInformationW,
    GetVolumePathNameW,
};
use windows_sys::Win32::System::SystemServices::{
    FILE_NAMED_STREAMS, FILE_SUPPORTS_BLOCK_REFCOUNTING, FILE_SUPPORTS_ENCRYPTION,
    FILE_SUPPORTS_EXTENDED_ATTRIBUTES, FILE_SUPPORTS_REPARSE_POINTS, FILE_SUPPORTS_SPARSE_FILES,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_NO_ROOT_DIR, DRIVE_REMOTE};

use crate::util::{bool_result, wide_null};

/// Supported filesystem families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileSystem {
    /// Microsoft NTFS.
    Ntfs,
    /// Microsoft Resilient File System.
    Refs,
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
}

/// Immutable facts used by pre-flight, tuning, and reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeInfo {
    /// Mounted volume root such as an extended C drive root.
    pub root: PathBuf,
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

/// Probes one existing local path and rejects unsupported filesystems or shares.
pub fn probe_volume(path: &Path) -> io::Result<VolumeInfo> {
    let input = wide_null(path.as_os_str());
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
    let volume_root_wide = wide_null(volume_root.as_os_str());

    // SAFETY: volume_root_wide is a live, nul-terminated string.
    let drive_type = unsafe { GetDriveTypeW(volume_root_wide.as_ptr()) };
    if drive_type == DRIVE_REMOTE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bigcp works with local volumes only; mapped network drives are unsupported",
        ));
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
    let filesystem = if filesystem_name.eq_ignore_ascii_case("NTFS") {
        FileSystem::Ntfs
    } else if filesystem_name.eq_ignore_ascii_case("ReFS") {
        FileSystem::Refs
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "bigcp supports NTFS and ReFS volumes only; found {filesystem_name}. Reformat the volume or use robocopy for legacy media"
            ),
        ));
    };

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
        filesystem,
        serial,
        maximum_component_length: max_component,
        bytes_per_sector,
        cluster_size: u64::from(sectors_per_cluster) * u64::from(bytes_per_sector),
        free_bytes_available: free_for_caller,
        total_bytes,
        capabilities: VolumeCapabilities {
            named_streams: flags & FILE_NAMED_STREAMS != 0,
            extended_attributes: flags & FILE_SUPPORTS_EXTENDED_ATTRIBUTES != 0,
            sparse_files: flags & FILE_SUPPORTS_SPARSE_FILES != 0,
            encryption: flags & FILE_SUPPORTS_ENCRYPTION != 0,
            reparse_points: flags & FILE_SUPPORTS_REPARSE_POINTS != 0,
            block_refcounting: flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0,
        },
    })
}

fn truncate_nul(buffer: &mut Vec<u16>) {
    if let Some(index) = buffer.iter().position(|value| *value == 0) {
        buffer.truncate(index);
    }
}

#[cfg(test)]
mod tests {
    use super::{FileSystem, probe_volume};

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
            FileSystem::Ntfs | FileSystem::Refs
        ));
        assert!(info.cluster_size > 0);
    }
}
