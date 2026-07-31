//! Atomic publication shared by bounded state and report artifacts.
//!
//! Artifact writers create an exclusive, unpredictable sibling, synchronize
//! its contents, and replace the final path through the same handle that
//! created the sibling. Keeping this outside the destination-file engine
//! prevents audit/state hygiene from acquiring destination-tree mutation
//! authority while reusing its exact-handle ownership primitive.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use bigcp_win::DestinationTemp;

/// An exclusively created sibling awaiting atomic publication.
pub(crate) struct AtomicArtifact {
    temporary: Option<DestinationTemp>,
    final_path: PathBuf,
}

impl AtomicArtifact {
    /// Allocates an unpredictable empty sibling without replacing anything.
    pub(crate) fn create(path: &Path, temporary_kind: &str) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "artifact path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        Ok(Self {
            temporary: Some(DestinationTemp::create(parent, temporary_kind, false)?),
            final_path: path.to_path_buf(),
        })
    }

    /// Borrows the exclusive temporary for streaming serialization.
    pub(crate) fn writer(&mut self) -> io::Result<&mut DestinationTemp> {
        self.temporary
            .as_mut()
            .ok_or_else(|| io::Error::other("artifact temporary already published"))
    }

    /// Synchronizes the temporary and atomically replaces the final path.
    pub(crate) fn publish(mut self) -> io::Result<()> {
        self.temporary
            .take()
            .ok_or_else(|| io::Error::other("artifact temporary already published"))?
            .publish_artifact(&self.final_path)
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{AtomicArtifact, write_atomic_bytes};

    #[test]
    fn artifact_temporary_remains_handle_owned_until_drop() {
        let Ok(sandbox) = tempfile::tempdir() else {
            return;
        };
        let final_path = sandbox.path().join("report.json");
        let Ok(artifact) = AtomicArtifact::create(&final_path, "report") else {
            return;
        };
        let temporary_path = artifact
            .temporary
            .as_ref()
            .map(|temporary| temporary.path().to_path_buf());
        assert!(temporary_path.as_ref().is_some_and(|path| path.exists()));
        assert!(
            temporary_path
                .as_ref()
                .is_some_and(|path| fs::remove_file(path).is_err()),
            "another path-based actor must not be able to replace the owned temporary"
        );
        drop(artifact);
        assert!(temporary_path.as_ref().is_some_and(|path| !path.exists()));
    }

    #[test]
    fn atomic_bytes_replace_the_final_and_leave_no_temporary() {
        let Ok(sandbox) = tempfile::tempdir() else {
            return;
        };
        let final_path = sandbox.path().join("journal.jsonl");
        assert!(fs::write(&final_path, b"old").is_ok());
        assert!(write_atomic_bytes(&final_path, "journal", b"new").is_ok());
        assert_eq!(
            fs::read(&final_path).ok().as_deref(),
            Some(b"new".as_slice())
        );
        let entries = fs::read_dir(sandbox.path())
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
    }
}
