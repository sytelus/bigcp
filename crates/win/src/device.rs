//! Query-only storage topology and adapter capability profiling.
//!
//! Every device handle requests zero data access. USB bridges commonly reject
//! these IOCTLs, so each fact is optional and callers select a conservative
//! profile when confidence is low.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::{
    BusTypeNvme, BusTypeSata, BusTypeUsb, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_EXTENT, IOCTL_STORAGE_QUERY_PROPERTY,
    PropertyStandardQuery, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_ADAPTER_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, StorageAccessAlignmentProperty, StorageAdapterProperty,
    StorageDeviceSeekPenaltyProperty,
};

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
    /// Whether the seek-penalty and bus-classification queries both succeeded.
    pub high_confidence: bool,
}

/// Profiles a volume without requesting read or write access to its device.
#[must_use]
pub fn profile_device(volume: &VolumeInfo) -> DeviceInfo {
    let Ok(handle) = open_volume_device(volume) else {
        return DeviceInfo {
            disk_numbers: Vec::new(),
            incurs_seek_penalty: None,
            bus: None,
            maximum_transfer_length: None,
            logical_sector: Some(volume.bytes_per_sector),
            physical_sector: None,
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
    let bus = adapter
        .as_ref()
        .map(|value| match i32::from(value.BusType) {
            value if value == BusTypeNvme => DeviceBus::Nvme,
            value if value == BusTypeSata => DeviceBus::Sata,
            value if value == BusTypeUsb => DeviceBus::Usb,
            _ => DeviceBus::Other,
        });
    let maximum_transfer_length = adapter.map(|value| value.MaximumTransferLength);
    let logical_sector = alignment
        .as_ref()
        .map(|value| value.BytesPerLogicalSector)
        .or(Some(volume.bytes_per_sector));
    let physical_sector = alignment.map(|value| value.BytesPerPhysicalSector);
    DeviceInfo {
        disk_numbers,
        incurs_seek_penalty: seek,
        bus,
        maximum_transfer_length,
        logical_sector,
        physical_sector,
        high_confidence: seek.is_some() && bus.is_some(),
    }
}

fn open_volume_device(volume: &VolumeInfo) -> io::Result<File> {
    let units: Vec<u16> = volume.root.as_os_str().encode_wide().collect();
    if units.len() < 2 || units[1] != u16::from(b':') {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume mount is not a drive-letter root",
        ));
    }
    let device = OsString::from_wide(&[
        u16::from(b'\\'),
        u16::from(b'\\'),
        u16::from(b'.'),
        u16::from(b'\\'),
        units[0],
        units[1],
    ]);
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
    if result == 0 || returned < u32::try_from(size_of::<T>()).ok()? {
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
    if returned < 8 {
        return None;
    }
    let count = u32::from_le_bytes(output.0[..4].try_into().ok()?) as usize;
    let extent_offset = 8_usize;
    let extent_size = size_of::<DISK_EXTENT>();
    if extent_offset.checked_add(count.checked_mul(extent_size)?)? > returned as usize {
        return None;
    }
    let mut disks = Vec::with_capacity(count);
    for index in 0..count {
        let offset = extent_offset + index * extent_size;
        // SAFETY: bounds were validated above. read_unaligned handles any
        // compiler padding assumptions conservatively.
        let extent = unsafe {
            std::ptr::read_unaligned(output.0.as_ptr().add(offset).cast::<DISK_EXTENT>())
        };
        if !disks.contains(&extent.DiskNumber) {
            disks.push(extent.DiskNumber);
        }
    }
    Some(disks)
}
