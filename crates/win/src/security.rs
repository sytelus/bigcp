//! Preservation of explicit destination DACL protection during replacement.

use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{LocalFree, WIN32_ERROR};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
};

use crate::metadata::{FileIdentity, metadata_from_file};
use crate::util::bool_result;

/// Opaque security descriptor owning a protected explicit DACL.
pub struct ProtectedDacl {
    descriptor: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
}

impl ProtectedDacl {
    /// Applies the preserved DACL and protected-control bit to a temp handle.
    pub(crate) fn apply_to(&self, file: &std::fs::File) -> io::Result<()> {
        // SAFETY: dacl points inside descriptor, which this value owns for the
        // entire call. Owner, group, and SACL are deliberately not changed.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.dacl,
                std::ptr::null(),
            )
        };
        win32_status(status)
    }
}

impl Drop for ProtectedDacl {
    fn drop(&mut self) {
        // SAFETY: descriptor is the LocalAlloc pointer returned by
        // GetSecurityInfo and is freed exactly once here.
        unsafe {
            let _ = LocalFree(self.descriptor.cast());
        }
    }
}

/// Reads a protected DACL only when the non-following handle still names the
/// expected destination object.
pub fn read_protected_dacl_checked(
    path: &Path,
    expected_identity: FileIdentity,
) -> io::Result<Option<ProtectedDacl>> {
    let file = open_for_dacl(path)?;
    if metadata_from_file(&file)?.identity != expected_identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "destination identity changed before DACL capture",
        ));
    }
    read_protected_dacl_from_file(&file)
}

fn open_for_dacl(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

fn read_protected_dacl_from_file(file: &std::fs::File) -> io::Result<Option<ProtectedDacl>> {
    let mut dacl = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: all requested output pointers are valid. Unrequested owner,
    // group, and SACL outputs are null.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    win32_status(status)?;
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor was returned successfully above and remains live.
    let control_result =
        unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) };
    if let Err(error) = bool_result(control_result) {
        // SAFETY: ownership has not yet moved into ProtectedDacl.
        unsafe {
            let _ = LocalFree(descriptor.cast());
        }
        return Err(error);
    }
    if control & SE_DACL_PROTECTED == 0 {
        // SAFETY: no wrapper will own the descriptor in this branch.
        unsafe {
            let _ = LocalFree(descriptor.cast());
        }
        return Ok(None);
    }
    Ok(Some(ProtectedDacl { descriptor, dacl }))
}

fn win32_status(status: WIN32_ERROR) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.cast_signed()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
        WRITE_DAC,
    };

    use super::{read_protected_dacl_checked, win32_status};
    use crate::file::DestinationTemp;
    use crate::metadata::metadata_at;

    /// Marks a test-owned file's DACL protected while keeping its ACEs.
    fn protect_dacl(file: &std::fs::File) -> io::Result<()> {
        let mut dacl = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: requested outputs are valid pointers; unrequested outputs
        // are null; the descriptor is freed exactly once below.
        let read_status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut dacl,
                std::ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        win32_status(read_status)?;
        // SAFETY: dacl points inside descriptor, which stays live for the call.
        let write_status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        // SAFETY: descriptor is the LocalAlloc pointer from GetSecurityInfo.
        unsafe {
            let _ = LocalFree(descriptor.cast());
        }
        win32_status(write_status)
    }

    #[test]
    fn captured_protected_dacl_applies_to_a_destination_temp_handle() {
        // Regression: DestinationTemp handles must carry WRITE_DAC.
        // SetSecurityInfo checks the handle's *granted* access, so without it
        // every replacement of a protected-DACL destination failed with
        // access-denied at the preserve_dacl step.
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let target = sandbox.path().join("protected.bin");
        assert!(std::fs::write(&target, b"payload").is_ok());
        let opened = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&target);
        assert!(opened.is_ok());
        let Some(opened) = opened.ok() else {
            return;
        };
        assert!(protect_dacl(&opened).is_ok());
        drop(opened);

        let identity = metadata_at(&target).map(|metadata| metadata.identity);
        assert!(identity.is_ok());
        let Some(identity) = identity.ok() else {
            return;
        };
        let captured = read_protected_dacl_checked(&target, identity)
            .ok()
            .flatten();
        assert!(captured.is_some(), "protected DACL was not captured");
        let Some(captured) = captured else {
            return;
        };

        let temp = DestinationTemp::create(sandbox.path(), "run1", false, false);
        assert!(temp.is_ok());
        let Some(temp) = temp.ok() else {
            return;
        };
        assert!(temp.apply_protected_dacl(&captured).is_ok());
    }

    #[test]
    fn checked_dacl_read_rejects_a_replaced_path() {
        let sandbox = tempfile::tempdir();
        assert!(sandbox.is_ok());
        let Some(sandbox) = sandbox.ok() else {
            return;
        };
        let expected_path = sandbox.path().join("expected.bin");
        let observed_path = sandbox.path().join("observed.bin");
        assert!(std::fs::write(&expected_path, b"expected").is_ok());
        assert!(std::fs::write(&observed_path, b"observed").is_ok());
        let expected = metadata_at(&expected_path);
        assert!(expected.is_ok());
        let Some(expected) = expected.ok() else {
            return;
        };

        let error = read_protected_dacl_checked(&observed_path, expected.identity).err();
        assert!(error.is_some_and(|value| value.kind() == io::ErrorKind::InvalidData));
        assert_eq!(
            std::fs::read(&observed_path).ok(),
            Some(b"observed".to_vec())
        );
    }
}
