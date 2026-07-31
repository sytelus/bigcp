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
pub mod endpoint;
pub mod extents;
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
    ExtendedAttributes, clear_extended_attributes, clear_extended_attributes_checked,
    read_extended_attributes, read_extended_attributes_checked, write_extended_attributes,
    write_extended_attributes_checked,
};
pub use endpoint::{EndpointKind, classify_endpoint};
pub use extents::extent_count;
pub use file::{
    COPYABLE_ATTRIBUTES, DestinationFinal, DestinationTemp, FAT_COPYABLE_ATTRIBUTES, SourceFile,
    create_directory, is_cloud_placeholder, is_compressed, is_encrypted, is_readonly, is_sparse,
    publish_audit_temporary, set_basic_at, set_basic_at_checked, without_readonly,
};
pub use lock::DestinationLock;
pub use metadata::{
    BasicMetadata, DirectoryEntry, FileIdentity, ObjectKind, ObjectMetadata, enumerate_directory,
    metadata_at, open_metadata, open_root,
};
pub use path::{
    absolute_extended, comparison_key, display_path, final_path, is_same_or_descendant,
    is_same_or_descendant_with, ordinal_case_key,
};
pub use reparse::{
    ReparseCopyError, ReparseCopyResult, ReparseData, copy_reparse, read_reparse_data,
};
pub use security::{ProtectedDacl, read_protected_dacl_checked};
pub use sparse::AllocatedRange;
pub use streams::{DestinationStream, SourceStream, StreamInfo, list_streams};
pub use volume::{FileSystem, VolumeCapabilities, VolumeInfo, probe_volume};
