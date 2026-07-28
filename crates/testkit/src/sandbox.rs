//! Structural containment guard required by every testkit operation.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use bigcp_win::{
    ObjectKind, absolute_extended, final_path, is_same_or_descendant, metadata_at, open_root,
};
use uuid::Uuid;

const MARKER: &str = ".bigcp-test-sandbox";
const FORBIDDEN_TEST_DRIVES: [char; 3] = ['F', 'G', 'H'];

/// Validated sandbox root that cannot represent F:, G:, or H:.
#[derive(Clone, Debug)]
pub struct SandboxRoot {
    root: PathBuf,
}

impl SandboxRoot {
    /// Opens an explicitly marked sandbox.
    pub fn open(path: &Path) -> Result<Self> {
        let root = absolute_extended(path).context("normalize sandbox root")?;
        validate_drive(&root)?;
        let root = resolve_without_alias(&root)?;
        let metadata = metadata_at(&root).context("open sandbox root")?;
        if metadata.kind != ObjectKind::Directory {
            bail!("sandbox root is not a real directory");
        }
        if !root.join(MARKER).is_file() {
            bail!("sandbox marker is missing; create an isolated sandbox with testkit init first");
        }
        validate_depth(&root)?;
        Ok(Self { root })
    }

    /// Creates one fresh sandbox under the system temporary directory.
    ///
    /// The method refuses to run if the system temporary directory resolves to
    /// F:, G:, or H:.
    pub fn create_system_temp(label: &str) -> Result<Self> {
        let base = validated_system_temp()?;
        let safe_label = label
            .chars()
            .filter(|value| value.is_ascii_alphanumeric() || *value == '-')
            .take(32)
            .collect::<String>();
        if safe_label.is_empty() {
            bail!("sandbox label must contain an ASCII letter or digit");
        }
        let root = base
            .join("bigcp-tests")
            .join(format!("{safe_label}-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&root).context("create system-temp sandbox")?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(MARKER))
            .context("create sandbox marker")?;
        Self::open(&root)
    }

    /// Returns the validated absolute root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Resolves a relative path and rejects traversal or reparse intermediates.
    pub fn child(&self, relative: &Path) -> Result<PathBuf> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            bail!("test path must be relative and may not contain parent traversal");
        }
        let mut current = self.root.clone();
        for component in relative.components() {
            if matches!(component, Component::CurDir) {
                continue;
            }
            current.push(component.as_os_str());
            match metadata_at(&current) {
                Ok(metadata) if metadata.kind == ObjectKind::Reparse => {
                    bail!(
                        "sandbox path crosses a reparse point: {}",
                        current.display()
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect sandbox child"),
            }
        }
        Ok(current)
    }

    /// Creates a new run directory, never reusing an existing path.
    pub fn create_run_dir(&self, label: &str) -> Result<PathBuf> {
        let relative = PathBuf::from(format!("{label}-{}", Uuid::new_v4().simple()));
        let path = self.child(&relative)?;
        fs::create_dir(&path).context("create sandbox run directory")?;
        Ok(path)
    }
}

fn validate_drive(path: &Path) -> Result<()> {
    let display = path.to_string_lossy();
    let bytes = display.as_bytes();
    let drive_index = if display.starts_with(r"\\?\") { 4 } else { 0 };
    if bytes.len() > drive_index + 1 && bytes[drive_index + 1] == b':' {
        let drive = (bytes[drive_index] as char).to_ascii_uppercase();
        if FORBIDDEN_TEST_DRIVES.contains(&drive) {
            bail!("tests are forbidden on external drive {drive}:");
        }
    }
    Ok(())
}

fn validate_depth(path: &Path) -> Result<()> {
    let normal_components = path
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    if normal_components < 3 {
        bail!("sandbox root is too broad");
    }
    Ok(())
}

fn resolve_without_alias(path: &Path) -> Result<PathBuf> {
    let pin = open_root(path).context("open sandbox path for final-path validation")?;
    let resolved = final_path(&pin).context("resolve sandbox final path")?;
    validate_drive(&resolved)?;
    let equivalent = is_same_or_descendant(path, &resolved).context("compare sandbox input")?
        && is_same_or_descendant(&resolved, path).context("compare sandbox final path")?;
    if !equivalent {
        bail!(
            "sandbox path may not traverse a junction, symlink, mount alias, or SUBST mapping (input {}, final {})",
            path.display(),
            resolved.display()
        );
    }
    Ok(resolved)
}

/// Returns the resolved system temporary directory after checking the drive
/// prohibition, before any test fixture is created.
pub fn validated_system_temp() -> Result<PathBuf> {
    let base = absolute_extended(&std::env::temp_dir()).context("normalize system temp")?;
    validate_drive(&base)?;
    resolve_without_alias(&base)
}

/// Creates a marker in an already-empty explicitly selected directory.
pub fn initialize_empty(path: &Path) -> Result<SandboxRoot> {
    let root = absolute_extended(path).context("normalize requested sandbox")?;
    validate_drive(&root)?;
    if !root.exists() {
        bail!("testkit init requires an existing empty directory");
    }
    let root = resolve_without_alias(&root)?;
    let mut entries = fs::read_dir(&root).context("inspect requested sandbox")?;
    if entries.next().transpose()?.is_some() {
        bail!("testkit init requires an empty directory");
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(MARKER))
        .context("create sandbox marker")?;
    SandboxRoot::open(&root)
}

/// Returns an ordinary I/O error for CLI adapters.
#[doc(hidden)]
pub fn io_other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

#[cfg(test)]
mod tests {
    use super::{SandboxRoot, initialize_empty, validated_system_temp};
    use std::fs;
    use std::os::windows::fs::symlink_file;
    use std::path::Path;

    #[test]
    fn forbidden_external_drives_are_rejected_before_access() {
        for drive in ["F:", "G:", "H:"] {
            let path = format!(r"{drive}\bigcp-must-not-touch");
            assert!(initialize_empty(Path::new(&path)).is_err());
        }
    }

    #[test]
    fn marked_system_temp_child_is_accepted() {
        let base = validated_system_temp();
        assert!(base.is_ok());
        let Some(base) = base.ok() else {
            return;
        };
        let outer = tempfile::tempdir_in(base);
        assert!(outer.is_ok());
        let Some(outer) = outer.ok() else {
            return;
        };
        let root = outer.path().join("sandbox");
        assert!(fs::create_dir(&root).is_ok());
        let sandbox = initialize_empty(&root);
        assert!(
            sandbox.is_ok(),
            "sandbox initialization failed: {sandbox:?}"
        );
        assert!(
            sandbox
                .as_ref()
                .is_ok_and(|value| value.path().ends_with("sandbox"))
        );
        let reopened = SandboxRoot::open(&root);
        assert!(reopened.is_ok());
    }

    #[test]
    fn dangling_symlink_children_are_rejected_when_links_are_available() {
        let Ok(sandbox) = SandboxRoot::create_system_temp("dangling-link") else {
            return;
        };
        let link = sandbox.path().join("dangling");
        if symlink_file(sandbox.path().join("missing-target"), &link).is_err() {
            return;
        }
        assert!(sandbox.child(Path::new("dangling/child")).is_err());
    }
}
