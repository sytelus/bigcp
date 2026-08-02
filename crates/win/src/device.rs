//! Query-only storage topology and adapter capability profiling.
//!
//! Every device handle requests zero data access. USB bridges commonly reject
//! these IOCTLs, so each fact is optional and callers select a conservative
//! profile when confidence is low.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    BusTypeNvme, BusTypeSata, BusTypeUsb, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_EXTENT, IOCTL_STORAGE_QUERY_PROPERTY,
    PropertyStandardQuery, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_ADAPTER_DESCRIPTOR,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY, STORAGE_WRITE_CACHE_PROPERTY,
    StorageAccessAlignmentProperty, StorageAdapterProperty, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty, StorageDeviceWriteCacheProperty, VOLUME_DISK_EXTENTS,
    WriteCacheDisabled, WriteCacheEnabled,
};

use crate::EndpointKind;
use crate::util::bool_result;
use crate::volume::VolumeInfo;

/// Coarse bus class used by deterministic static profiles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceBus {
    /// PCIe NVMe.
    Nvme,
    /// SATA or ATA.
    Sata,
    /// USB mass storage.
    Usb,
    /// RAID, virtual, storage-spaces, or an unreported bus.
    Other,
}

/// Optional, query-only facts for one mounted volume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    /// How the filesystem is reached; remote endpoints never expose local
    /// storage topology IOCTLs.
    pub endpoint: EndpointKind,
    /// Physical disk numbers backing the volume.
    pub disk_numbers: Vec<u32>,
    /// Whether Windows reports seek penalty.
    pub incurs_seek_penalty: Option<bool>,
    /// Adapter bus family.
    pub bus: Option<DeviceBus>,
    /// Maximum transfer length reported by the adapter.
    pub maximum_transfer_length: Option<u32>,
    /// Logical sector bytes.
    pub logical_sector: Option<u32>,
    /// Physical sector bytes.
    pub physical_sector: Option<u32>,
    /// Device write-cache state (query-only). `Some(false)` on an external
    /// drive is the signature of Windows' Quick-removal policy, measured to
    /// cost ~3.4× on metadata-heavy small-file workloads (BENCHMARKS.md
    /// 2026-07-29) — surfaced to the user before a copy starts, never
    /// changed by bigcp.
    pub write_cache_enabled: Option<bool>,
    /// Whether the seek-penalty and bus-classification queries both succeeded.
    pub high_confidence: bool,
}

/// Profiles a volume without requesting read or write access to its device.
#[must_use]
pub fn profile_device(volume: &VolumeInfo) -> DeviceInfo {
    if volume.endpoint.is_remote() {
        return DeviceInfo {
            endpoint: volume.endpoint,
            disk_numbers: Vec::new(),
            incurs_seek_penalty: None,
            bus: None,
            maximum_transfer_length: None,
            logical_sector: Some(volume.bytes_per_sector),
            physical_sector: None,
            write_cache_enabled: None,
            high_confidence: false,
        };
    }
    let Ok(handle) = open_volume_device(volume) else {
        return DeviceInfo {
            endpoint: volume.endpoint,
            disk_numbers: Vec::new(),
            incurs_seek_penalty: None,
            bus: None,
            maximum_transfer_length: None,
            logical_sector: Some(volume.bytes_per_sector),
            physical_sector: None,
            write_cache_enabled: None,
            high_confidence: false,
        };
    };
    let disk_numbers = query_disk_numbers(&handle).unwrap_or_default();
    let seek =
        query_property::<DEVICE_SEEK_PENALTY_DESCRIPTOR>(&handle, StorageDeviceSeekPenaltyProperty)
            .map(|value| value.IncursSeekPenalty);
    let adapter = query_property::<STORAGE_ADAPTER_DESCRIPTOR>(&handle, StorageAdapterProperty);
    let alignment = query_property::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>(
        &handle,
        StorageAccessAlignmentProperty,
    );
    let bus = classify_bus(
        adapter.as_ref().map(|value| i32::from(value.BusType)),
        query_device_bus(&handle),
    );
    let maximum_transfer_length = adapter.map(|value| value.MaximumTransferLength);
    let logical_sector = alignment
        .as_ref()
        .map(|value| value.BytesPerLogicalSector)
        .or(Some(volume.bytes_per_sector));
    let physical_sector = alignment.map(|value| value.BytesPerPhysicalSector);
    DeviceInfo {
        endpoint: volume.endpoint,
        disk_numbers,
        incurs_seek_penalty: seek,
        bus,
        maximum_transfer_length,
        logical_sector,
        physical_sector,
        write_cache_enabled: query_write_cache(&handle),
        high_confidence: seek.is_some() && bus.is_some(),
    }
}

/// Queries the device's write-cache state (query-only, never modified).
///
/// This must use the storage-property query, not
/// `IOCTL_DISK_GET_CACHE_INFORMATION`: that control code encodes
/// `FILE_READ_ACCESS` in its access bits, and the deliberately zero-access
/// volume handle can never satisfy it — the disk-cache IOCTL would fail with
/// access-denied on every volume and silently disable the Quick-removal
/// warning. `IOCTL_STORAGE_QUERY_PROPERTY` is `FILE_ANY_ACCESS` and reports
/// the same device-level write-cache state.
fn query_write_cache(handle: &File) -> Option<bool> {
    let cache =
        query_property::<STORAGE_WRITE_CACHE_PROPERTY>(handle, StorageDeviceWriteCacheProperty)?;
    // WRITE_CACHE_ENABLE is a tri-state; "unknown" must stay None so callers
    // never warn (or skip warning) based on a guess.
    match cache.WriteCacheEnabled {
        value if value == WriteCacheEnabled => Some(true),
        value if value == WriteCacheDisabled => Some(false),
        _ => None,
    }
}

/// Builds the `\\.\X:` query-only device path for a drive-letter volume root.
///
/// Production roots arrive in extended-length form (`\\?\C:\`) because every
/// path is canonicalized before probing, so a `\\?\` or `\\.\` prefix is
/// skipped before the drive-letter check. Without that skip, profiling
/// silently degraded to the low-confidence fallback on every real run.
///
/// The body must be *exactly* a drive root (`X:` or `X:\`). A volume mounted
/// in a folder legally reports a root like `C:\mount\vol\`; deriving `\\.\C:`
/// from it would profile the *host* drive's topology (disk numbers, seek
/// penalty, cache state) for the mounted volume — corrupting same-spindle
/// detection. Such roots degrade to the documented low-confidence fallback.
fn device_path_for_root(root: &Path) -> io::Result<OsString> {
    let units: Vec<u16> = root.as_os_str().encode_wide().collect();
    let body = match units.as_slice() {
        [b1, b2, q, b3, rest @ ..]
            if *b1 == u16::from(b'\\')
                && *b2 == u16::from(b'\\')
                && (*q == u16::from(b'?') || *q == u16::from(b'.'))
                && *b3 == u16::from(b'\\') =>
        {
            rest
        }
        rest => rest,
    };
    let drive_root_only = matches!(body.len(), 2 | 3)
        && body[1] == u16::from(b':')
        && (body.len() == 2 || body[2] == u16::from(b'\\'))
        && u8::try_from(body[0]).is_ok_and(|letter| letter.is_ascii_alphabetic());
    if !drive_root_only {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume mount is not a drive-letter root",
        ));
    }
    Ok(OsString::from_wide(&[
        u16::from(b'\\'),
        u16::from(b'\\'),
        u16::from(b'.'),
        u16::from(b'\\'),
        body[0],
        body[1],
    ]))
}

/// Combines the adapter-level and device-level bus answers.
///
/// NVMe drives behind Intel VMD/RAID controllers report an unspecific bus at
/// the ADAPTER level while the per-device descriptor still answers
/// `BusTypeNvme` — on such hardware the adapter-only classification silently
/// selected the moderate SATA-SSD row for a Gen4 NVMe pair (observed
/// 2026-08-02 on this project's own benchmark host). A specific answer from
/// either source wins, preferring the adapter; two unspecific answers stay
/// `Other`; no answer at all stays `None` so the conservative Unknown
/// profile applies exactly as before.
fn classify_bus(adapter: Option<i32>, device: Option<i32>) -> Option<DeviceBus> {
    let specific = |value: i32| match value {
        value if value == BusTypeNvme => Some(DeviceBus::Nvme),
        value if value == BusTypeSata => Some(DeviceBus::Sata),
        value if value == BusTypeUsb => Some(DeviceBus::Usb),
        _ => None,
    };
    match (adapter.and_then(specific), device.and_then(specific)) {
        (Some(bus), _) | (None, Some(bus)) => Some(bus),
        (None, None) if adapter.is_some() || device.is_some() => Some(DeviceBus::Other),
        (None, None) => None,
    }
}

/// Queries the variable-length device descriptor for its bus type.
///
/// `STORAGE_DEVICE_DESCRIPTOR` trails identification strings, so the query
/// uses a bounded raw buffer and reads only the fixed-offset `BusType` field
/// after validating the returned length covers it.
fn query_device_bus(handle: &File) -> Option<i32> {
    #[repr(C, align(8))]
    struct Aligned([u8; 1024]);

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut output = Aligned([0; 1024]);
    let mut returned = 0_u32;
    // SAFETY: query and output are valid for their passed lengths; the
    // control code is a read-only query and the call is synchronous.
    let succeeded = unsafe {
        DeviceIoControl(
            handle.as_raw_handle().cast(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&raw const query).cast(),
            u32::try_from(size_of::<STORAGE_PROPERTY_QUERY>()).ok()?,
            output.0.as_mut_ptr().cast(),
            u32::try_from(output.0.len()).ok()?,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    let bus_offset = offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType);
    let needed = bus_offset.checked_add(size_of::<i32>())?;
    if succeeded == 0 || (returned as usize) < needed {
        return None;
    }
    // SAFETY: the returned length covers the fixed BusType offset within the
    // zero-initialized bounded buffer; read_unaligned tolerates any packing.
    Some(unsafe { std::ptr::read_unaligned(output.0.as_ptr().add(bus_offset).cast::<i32>()) })
}

fn open_volume_device(volume: &VolumeInfo) -> io::Result<File> {
    let device = device_path_for_root(&volume.root)?;
    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    options.open(device)
}

fn query_property<T: Default>(handle: &File, property: i32) -> Option<T> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: property,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut output = T::default();
    let mut returned = 0_u32;
    // SAFETY: query and output are properly aligned concrete structures, the
    // handle stays live, and the call is synchronous.
    let result = unsafe {
        DeviceIoControl(
            handle.as_raw_handle().cast(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&raw const query).cast(),
            u32::try_from(size_of::<STORAGE_PROPERTY_QUERY>()).ok()?,
            (&raw mut output).cast(),
            u32::try_from(size_of::<T>()).ok()?,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if result == 0 || returned != u32::try_from(size_of::<T>()).ok()? {
        None
    } else {
        Some(output)
    }
}

fn query_disk_numbers(handle: &File) -> Option<Vec<u32>> {
    #[repr(C, align(8))]
    struct Aligned([u8; 4096]);

    let mut output = Aligned([0; 4096]);
    let mut returned = 0_u32;
    // SAFETY: output is aligned for VOLUME_DISK_EXTENTS and writable. The
    // operation is synchronous and no input buffer is required.
    unsafe {
        bool_result(DeviceIoControl(
            handle.as_raw_handle().cast(),
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            std::ptr::null(),
            0,
            output.0.as_mut_ptr().cast(),
            u32::try_from(output.0.len()).ok()?,
            &raw mut returned,
            std::ptr::null_mut(),
        ))
        .ok()?;
    }
    disk_numbers_from_bytes(&output.0, returned)
}

fn disk_numbers_from_bytes(output: &[u8], returned: u32) -> Option<Vec<u32>> {
    let returned = usize::try_from(returned).ok()?;
    // Use the binding's actual ABI layout rather than assuming the compiler's
    // padding between NumberOfDiskExtents and the first i64-aligned extent.
    let extent_offset = offset_of!(VOLUME_DISK_EXTENTS, Extents);
    if returned < extent_offset || returned > output.len() {
        return None;
    }
    let count = u32::from_le_bytes(output[..4].try_into().ok()?) as usize;
    let extent_size = size_of::<DISK_EXTENT>();
    if extent_offset.checked_add(count.checked_mul(extent_size)?)? > returned {
        return None;
    }
    let mut disks = Vec::with_capacity(count);
    for index in 0..count {
        let offset = extent_offset + index * extent_size;
        // SAFETY: bounds were validated above. read_unaligned handles any
        // compiler padding assumptions conservatively.
        let extent =
            unsafe { std::ptr::read_unaligned(output.as_ptr().add(offset).cast::<DISK_EXTENT>()) };
        if !disks.contains(&extent.DiskNumber) {
            disks.push(extent.DiskNumber);
        }
    }
    Some(disks)
}

#[cfg(test)]
mod tests {
    use super::{device_path_for_root, disk_numbers_from_bytes};
    use std::ffi::OsString;
    use std::mem::{offset_of, size_of};
    use std::path::Path;

    use windows_sys::Win32::System::Ioctl::{DISK_EXTENT, VOLUME_DISK_EXTENTS};

    #[test]
    fn extended_length_root_yields_drive_device_path() {
        // Regression: production roots are always extended-length; the old
        // check rejected them and silently disabled all device profiling.
        let device = device_path_for_root(Path::new(r"\\?\C:\"));
        assert_eq!(device.ok(), Some(OsString::from(r"\\.\C:")));
    }

    #[test]
    fn bare_drive_root_yields_drive_device_path() {
        let device = device_path_for_root(Path::new(r"C:\"));
        assert_eq!(device.ok(), Some(OsString::from(r"\\.\C:")));
    }

    #[test]
    fn non_letter_roots_are_rejected() {
        assert!(device_path_for_root(Path::new(r"\\?\Volume{0000}\a")).is_err());
        assert!(device_path_for_root(Path::new(r"relative")).is_err());
    }

    #[test]
    fn bus_classification_prefers_any_specific_answer_over_unspecific_ones() {
        use windows_sys::Win32::Storage::FileSystem::{BusTypeNvme, BusTypeSata, BusTypeUsb};

        use super::{DeviceBus, classify_bus};

        // Specific adapter answers win exactly as before.
        assert_eq!(classify_bus(Some(BusTypeNvme), None), Some(DeviceBus::Nvme));
        assert_eq!(
            classify_bus(Some(BusTypeSata), Some(BusTypeNvme)),
            Some(DeviceBus::Sata)
        );
        // The VMD case: unspecific adapter, specific device descriptor.
        assert_eq!(
            classify_bus(Some(99), Some(BusTypeNvme)),
            Some(DeviceBus::Nvme)
        );
        assert_eq!(classify_bus(None, Some(BusTypeUsb)), Some(DeviceBus::Usb));
        // Two unspecific answers remain Other; silence remains None so the
        // conservative Unknown profile still applies.
        assert_eq!(classify_bus(Some(99), Some(77)), Some(DeviceBus::Other));
        assert_eq!(classify_bus(Some(99), None), Some(DeviceBus::Other));
        assert_eq!(classify_bus(None, None), None);
    }

    #[test]
    fn mounted_folder_roots_never_borrow_the_host_drive_device() {
        // A volume mounted in a folder reports a root under the host drive;
        // deriving \\.\C: from it would profile the wrong physical disk and
        // corrupt same-spindle detection. It must degrade to the
        // low-confidence fallback instead.
        assert!(device_path_for_root(Path::new(r"\\?\C:\mount\vol\")).is_err());
        assert!(device_path_for_root(Path::new(r"C:\mount\vol")).is_err());
        assert!(device_path_for_root(Path::new(r"\\?\1:\")).is_err());
    }

    #[test]
    fn disk_extent_parser_rejects_impossible_returned_lengths() {
        let extent_offset = offset_of!(VOLUME_DISK_EXTENTS, Extents);
        let buffer = [0_u8; 32];
        assert!(disk_numbers_from_bytes(&buffer, 33).is_none());
        assert!(
            disk_numbers_from_bytes(
                &buffer,
                u32::try_from(extent_offset.saturating_sub(1)).unwrap_or_default()
            )
            .is_none()
        );
        assert_eq!(
            disk_numbers_from_bytes(&buffer, u32::try_from(extent_offset).unwrap_or_default()),
            Some(Vec::new())
        );
    }

    #[test]
    fn disk_extent_parser_uses_the_bound_structure_layout_and_deduplicates() {
        let extent_offset = offset_of!(VOLUME_DISK_EXTENTS, Extents);
        let extent_size = size_of::<DISK_EXTENT>();
        let mut buffer = vec![0_u8; extent_offset + 3 * extent_size];
        buffer[..4].copy_from_slice(&3_u32.to_le_bytes());
        buffer[extent_offset..extent_offset + 4].copy_from_slice(&7_u32.to_le_bytes());
        buffer[extent_offset + extent_size..extent_offset + extent_size + 4]
            .copy_from_slice(&7_u32.to_le_bytes());
        buffer[extent_offset + 2 * extent_size..extent_offset + 2 * extent_size + 4]
            .copy_from_slice(&9_u32.to_le_bytes());

        assert_eq!(
            disk_numbers_from_bytes(&buffer, u32::try_from(buffer.len()).unwrap_or_default()),
            Some(vec![7, 9])
        );
    }
}
