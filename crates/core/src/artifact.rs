//! Atomic publication shared by bounded state and report artifacts.
//!
//! Artifact writers create an exclusive, unpredictable sibling, synchronize
//! its contents, and replace the final path in one Win32 operation. Keeping
//! this outside the destination-file engine prevents audit/state hygiene from
//! acquiring destination-tree mutation authority.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use bigcp_win::publish_audit_temporary;
use uuid::Uuid;

/// An exclusively created sibling awaiting atomic publication.
pub(crate) struct AtomicArtifact {
    file: Option<File>,
    temporary: PathBuf,
    final_path: PathBuf,
}

impl AtomicArtifact {
    /// Allocates an unpredictable empty sibling without replacing anything.
    pub(crate) fn create(path: &Path, temporary_kind: &str) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "artifact path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        for _ in 0..128 {
            let temporary = parent.join(format!(
                ".bigcp-{temporary_kind}-{}.part",
                Uuid::new_v4().simple()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        temporary,
                        final_path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique artifact temporary name",
        ))
    }

    /// Borrows the exclusive temporary for streaming serialization.
    pub(crate) fn writer(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("artifact temporary already published"))
    }

    /// Synchronizes the temporary and atomically replaces the final path.
    pub(crate) fn publish(mut self) -> io::Result<()> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("artifact temporary already published"))?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        publish_audit_temporary(&self.temporary, &self.final_path)?;
        // The name moved successfully; Drop must not try to clean it up.
        self.temporary.clear();
        Ok(())
    }
}

impl Drop for AtomicArtifact {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.temporary.as_os_str().is_empty() {
            // The UUID name was opened with create_new and belongs to this
            // value. Cleanup is best-effort; the primary error is retained.
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

/// Writes bounded bytes through [`AtomicArtifact`].
pub(crate) fn write_atomic_bytes(
    path: &Path,
    temporary_kind: &str,
    contents: &[u8],
) -> io::Result<()> {
    let mut artifact = AtomicArtifact::create(path, temporary_kind)?;
    artifact.writer()?.write_all(contents)?;
    artifact.publish()
}
