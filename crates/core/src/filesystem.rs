//! Destination filesystem policy without weakening the strict NTFS/ReFS path.
//!
//! Windows exposes one common API surface, but filesystems do not represent
//! the same metadata. This module is the only place where the core projects
//! source semantics onto a destination filesystem. NTFS/ReFS comparisons stay
//! exact; FAT-family comparisons accept only their documented timestamp
//! quantization and mask only attributes those directory entries can store.

use bigcp_win::{
    BasicMetadata, COPYABLE_ATTRIBUTES, FAT_COPYABLE_ATTRIBUTES, FileSystem, VolumeCapabilities,
};

const FILETIME_TICKS_PER_MILLISECOND: u64 = 10_000;
const FILETIME_TICKS_PER_SECOND: u64 = 10_000_000;

/// Copy and verification policy derived once from the destination volume.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FilesystemPolicy {
    filesystem: FileSystem,
    capabilities: VolumeCapabilities,
}

impl FilesystemPolicy {
    /// Creates a policy from probed, immutable destination facts.
    #[must_use]
    pub const fn new(filesystem: FileSystem, capabilities: VolumeCapabilities) -> Self {
        Self {
            filesystem,
            capabilities,
        }
    }

    /// Destination filesystem family.
    #[must_use]
    pub const fn filesystem(self) -> FileSystem {
        self.filesystem
    }

    /// Whether copied metadata must be projected or omitted.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        self.filesystem.is_fat_family()
    }

    /// Maximum logical file size accepted by this destination.
    #[must_use]
    pub const fn maximum_file_size(self) -> Option<u64> {
        self.filesystem.maximum_file_size()
    }

    /// Whether named data streams can be represented.
    #[must_use]
    pub const fn supports_streams(self) -> bool {
        self.capabilities.named_streams
    }

    /// Whether extended attributes can be represented.
    #[must_use]
    pub const fn supports_eas(self) -> bool {
        self.capabilities.extended_attributes
    }

    /// Whether sparse allocation can be represented.
    #[must_use]
    pub const fn supports_sparse(self) -> bool {
        self.capabilities.sparse_files
    }

    /// Whether EFS encryption can be represented.
    #[must_use]
    pub const fn supports_encryption(self) -> bool {
        self.capabilities.encryption
    }

    /// Whether reparse points can be represented.
    #[must_use]
    pub const fn supports_reparse_points(self) -> bool {
        self.capabilities.reparse_points
    }

    /// Whether protected destination DACLs can exist and need preservation.
    #[must_use]
    pub const fn supports_persistent_acls(self) -> bool {
        self.capabilities.persistent_acls
    }

    /// Whether handle renames may use POSIX replacement semantics.
    #[must_use]
    pub const fn supports_posix_unlink_rename(self) -> bool {
        self.capabilities.posix_unlink_rename
    }

    /// Whether the direct small-file path needs a final metadata restamp.
    #[must_use]
    pub const fn requires_post_write_stamp(self) -> bool {
        self.filesystem.is_fat_family()
    }

    /// Projects basic metadata to fields the destination can represent.
    #[must_use]
    pub const fn project_basic(self, mut metadata: BasicMetadata) -> BasicMetadata {
        metadata.attributes &= self.attribute_mask();
        metadata
    }

    /// Whether last-write values denote the same representable destination time.
    #[must_use]
    pub fn last_write_equal(self, source: i64, destination: i64) -> bool {
        timestamp_equal(source, destination, self.last_write_quantum())
    }

    /// Whether creation values denote the same representable destination time.
    #[must_use]
    pub fn creation_equal(self, source: i64, destination: i64) -> bool {
        timestamp_equal(source, destination, self.creation_quantum())
    }

    /// Informational last-access comparison at destination granularity.
    #[must_use]
    pub fn last_access_equal(self, source: i64, destination: i64) -> bool {
        timestamp_equal(source, destination, self.last_access_quantum())
    }

    /// Whether copyable source/destination attributes match after projection.
    #[must_use]
    pub const fn attributes_equal(self, source: u32, destination: u32) -> bool {
        source & self.attribute_mask() == destination & self.attribute_mask()
    }

    const fn attribute_mask(self) -> u32 {
        if self.filesystem.is_fat_family() {
            FAT_COPYABLE_ATTRIBUTES
        } else {
            COPYABLE_ATTRIBUTES
        }
    }

    const fn creation_quantum(self) -> Option<u64> {
        match self.filesystem {
            FileSystem::Ntfs | FileSystem::Refs => None,
            FileSystem::Fat | FileSystem::ExFat => Some(10 * FILETIME_TICKS_PER_MILLISECOND),
        }
    }

    const fn last_write_quantum(self) -> Option<u64> {
        match self.filesystem {
            FileSystem::Ntfs | FileSystem::Refs => None,
            FileSystem::Fat => Some(2 * FILETIME_TICKS_PER_SECOND),
            FileSystem::ExFat => Some(10 * FILETIME_TICKS_PER_MILLISECOND),
        }
    }

    const fn last_access_quantum(self) -> Option<u64> {
        match self.filesystem {
            FileSystem::Ntfs | FileSystem::Refs => None,
            FileSystem::Fat => Some(86_400 * FILETIME_TICKS_PER_SECOND),
            // exFAT has no 10 ms increment field for last access.
            FileSystem::ExFat => Some(2 * FILETIME_TICKS_PER_SECOND),
        }
    }
}

fn timestamp_equal(left: i64, right: i64, quantum: Option<u64>) -> bool {
    match quantum {
        None => left == right,
        // Setting a FILETIME on a coarse filesystem can round anywhere in
        // one representable interval. A strict '< quantum' comparison accepts
        // that round-trip without treating the next timestamp bucket as equal.
        Some(quantum) => left.abs_diff(right) < quantum,
    }
}

#[cfg(test)]
mod tests {
    use super::FilesystemPolicy;
    use bigcp_win::{FileSystem, VolumeCapabilities};

    fn capabilities() -> VolumeCapabilities {
        VolumeCapabilities {
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

    #[test]
    fn strict_filesystems_still_compare_every_filetime_tick() {
        let policy = FilesystemPolicy::new(FileSystem::Ntfs, capabilities());
        assert!(policy.last_write_equal(100, 100));
        assert!(!policy.last_write_equal(100, 101));
    }

    #[test]
    fn fat_and_exfat_accept_only_their_timestamp_quantization() {
        let fat = FilesystemPolicy::new(FileSystem::Fat, capabilities());
        assert!(fat.last_write_equal(0, 19_999_999));
        assert!(!fat.last_write_equal(0, 20_000_000));

        let exfat = FilesystemPolicy::new(FileSystem::ExFat, capabilities());
        assert!(exfat.creation_equal(0, 99_999));
        assert!(!exfat.creation_equal(0, 100_000));
    }

    #[test]
    fn fat_file_size_limit_is_not_applied_to_exfat() {
        let fat = FilesystemPolicy::new(FileSystem::Fat, capabilities());
        let exfat = FilesystemPolicy::new(FileSystem::ExFat, capabilities());
        assert_eq!(fat.maximum_file_size(), Some(u64::from(u32::MAX)));
        assert_eq!(exfat.maximum_file_size(), None);
    }

    #[test]
    fn fat_projects_attributes_without_changing_strict_policy() {
        const READONLY: u32 = 0x0000_0001;
        const NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
        let fat = FilesystemPolicy::new(FileSystem::Fat, capabilities());
        let ntfs = FilesystemPolicy::new(FileSystem::Ntfs, capabilities());
        let metadata = bigcp_win::BasicMetadata {
            creation_time: 1,
            last_access_time: 2,
            last_write_time: 3,
            attributes: READONLY | NOT_CONTENT_INDEXED,
        };

        assert_eq!(fat.project_basic(metadata).attributes, READONLY);
        assert_eq!(
            ntfs.project_basic(metadata).attributes,
            READONLY | NOT_CONTENT_INDEXED
        );
    }
}
