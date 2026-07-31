//! Stable endpoint classification for local, generic UNC, and WSL UNC roots.
//!
//! Endpoint kind is deliberately separate from filesystem capability and
//! physical-device profiling. A remote share can advertise NTFS semantics
//! without exposing local storage IOCTLs, while WSL's UNC provider fronts a
//! Linux filesystem through a loopback 9P translation layer. Keeping those
//! axes distinct prevents remote fallbacks from entering local hot paths.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

const EXTENDED_UNC_PREFIX: &[u16] = &[
    b'\\' as u16,
    b'\\' as u16,
    b'?' as u16,
    b'\\' as u16,
    b'U' as u16,
    b'N' as u16,
    b'C' as u16,
    b'\\' as u16,
];
const UNC_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16];
const EXTENDED_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// How a Windows path reaches its backing filesystem.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// A drive-letter or volume-mounted local filesystem.
    #[default]
    Local,
    /// A generic UNC share or a mapped drive resolving to one.
    Unc,
    /// A `wsl.localhost`/legacy `wsl$` Linux-filesystem share.
    Wsl,
}

impl EndpointKind {
    /// Stable user-facing endpoint name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Unc => "UNC",
            Self::Wsl => "WSL UNC",
        }
    }

    /// Whether storage access crosses a redirector rather than a local volume.
    #[must_use]
    pub const fn is_remote(self) -> bool {
        matches!(self, Self::Unc | Self::Wsl)
    }

    /// Whether path components below the share root have Linux case semantics.
    #[must_use]
    pub const fn names_are_case_sensitive(self) -> bool {
        matches!(self, Self::Wsl)
    }
}

/// Classifies an absolute or extended-length Windows path without I/O.
#[must_use]
pub fn classify_endpoint(path: &Path) -> EndpointKind {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    let Some(body) = unc_body(&units) else {
        return EndpointKind::Local;
    };
    let server_end = body
        .iter()
        .position(|value| *value == u16::from(b'\\'))
        .unwrap_or(body.len());
    let server = &body[..server_end];
    if ascii_eq(server, "wsl.localhost") || ascii_eq(server, "wsl$") {
        EndpointKind::Wsl
    } else {
        EndpointKind::Unc
    }
}

fn unc_body(units: &[u16]) -> Option<&[u16]> {
    if starts_with_ascii_case_insensitive(units, EXTENDED_UNC_PREFIX) {
        Some(&units[EXTENDED_UNC_PREFIX.len()..])
    } else if units.starts_with(UNC_PREFIX) && !units.starts_with(EXTENDED_PREFIX) {
        Some(&units[UNC_PREFIX.len()..])
    } else {
        None
    }
}

fn ascii_eq(units: &[u16], expected: &str) -> bool {
    units.len() == expected.len()
        && units
            .iter()
            .zip(expected.bytes())
            .all(|(left, right)| ascii_upper(*left) == u16::from(right.to_ascii_uppercase()))
}

fn starts_with_ascii_case_insensitive(value: &[u16], prefix: &[u16]) -> bool {
    value.len() >= prefix.len()
        && value
            .iter()
            .zip(prefix)
            .take(prefix.len())
            .all(|(left, right)| ascii_upper(*left) == ascii_upper(*right))
}

fn ascii_upper(value: u16) -> u16 {
    if (u16::from(b'a')..=u16::from(b'z')).contains(&value) {
        value - u16::from(b'a' - b'A')
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{EndpointKind, classify_endpoint};
    use std::path::Path;

    #[test]
    fn endpoint_classification_distinguishes_local_unc_and_wsl_aliases() {
        assert_eq!(
            classify_endpoint(Path::new(r"\\?\C:\data")),
            EndpointKind::Local
        );
        assert_eq!(
            classify_endpoint(Path::new(r"\\server\share\data")),
            EndpointKind::Unc
        );
        assert_eq!(
            classify_endpoint(Path::new(r"\\?\UNC\server\share\data")),
            EndpointKind::Unc
        );
        assert_eq!(
            classify_endpoint(Path::new(r"\\wsl.localhost\Ubuntu\home")),
            EndpointKind::Wsl
        );
        assert_eq!(
            classify_endpoint(Path::new(r"\\?\UNC\WSL$\Ubuntu\home")),
            EndpointKind::Wsl
        );
    }
}
