//! Safe, narrow wrappers around Win32 used by bigcp.
//!
//! This is the only crate in the workspace permitted to contain `unsafe`
//! code. Callers receive owned values and ordinary `io::Result` errors.

#![cfg_attr(not(windows), allow(dead_code))]
#![deny(missing_docs)]

#[cfg(not(windows))]
compile_error!("bigcp supports Windows only");

mod util;

pub mod device;
pub mod ea;
pub mod file;
pub mod lock;
pub mod metadata;
pub mod path;
pub mod reparse;
pub mod security;
pub mod sparse;
pub mod streams;
pub mod volume;

pub use device::{DeviceBus, DeviceInfo, profile_device};
pub use ea::{
    ExtendedAttributes, clear_extended_attributes, read_extended_attributes,
    write_extended_attributes,
};
pub use file::{
    COPYABLE_ATTRIBUTES, DestinationTemp, SourceFile, create_directory, is_cloud_placeholder,
    is_compressed, is_encrypted, is_sparse, publish_audit_temporary, set_basic_at,
};
pub use lock::DestinationLock;
pub use metadata::{
    BasicMetadata, DirectoryEntry, FileIdentity, ObjectKind, ObjectMetadata, enumerate_directory,
    metadata_at, open_metadata, open_root,
};
pub use path::{
    absolute_extended, display_path, final_path, is_same_or_descendant, ordinal_case_key,
};
pub use reparse::{ReparseCopyResult, ReparseData, copy_reparse, read_reparse_data};
pub use security::{ProtectedDacl, read_protected_dacl};
pub use sparse::AllocatedRange;
pub use streams::{DestinationStream, SourceStream, StreamInfo, list_streams};
pub use volume::{FileSystem, VolumeCapabilities, VolumeInfo, probe_volume};
