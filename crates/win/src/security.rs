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
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, READ_CONTROL,
};

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

/// Reads a destination DACL only when it carries explicit protection.
pub fn read_protected_dacl(path: &Path) -> io::Result<Option<ProtectedDacl>> {
    let mut options = OpenOptions::new();
    options
        .access_mode(READ_CONTROL)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
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
