//! Machine-wide exact-destination run lock.
//!
//! The mutex carries an explicit world-accessible DACL so separate interactive
//! sessions observe the same exclusion object. Ownership remains on the
//! acquiring thread until this guard is dropped.

use std::io;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex};

use crate::util::{bool_result, wide_null};

/// Owned machine-wide lock for one exact destination root.
pub struct DestinationLock {
    handle: HANDLE,
}

impl DestinationLock {
    /// Acquires a named mutex or returns WouldBlock if another run owns it.
    pub fn acquire(name_suffix: &str) -> io::Result<Self> {
        let mutex_name = format!("Global\\bigcp-{name_suffix}");
        let mutex_name = wide_null(mutex_name.as_ref());
        let sddl = wide_null("D:(A;;GA;;;WD)".as_ref());
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: sddl is a valid nul-terminated SDDL string and descriptor is
        // a valid out pointer. The returned LocalAlloc allocation is freed
        // after CreateMutexW has consumed the security descriptor.
        unsafe {
            bool_result(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            ))?;
        }

        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .map_err(|_| io::Error::other("SECURITY_ATTRIBUTES size overflow"))?,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: pointers refer to initialized values for the duration of the
        // call. The mutex name is nul-terminated.
        let handle = unsafe { CreateMutexW(&raw const attributes, 1, mutex_name.as_ptr()) };
        // Capture the status before LocalFree changes the thread's last error.
        // SAFETY: GetLastError has no preconditions.
        let create_status = unsafe { GetLastError() };
        // SAFETY: descriptor came from the SDDL conversion API and must be
        // released with LocalFree after CreateMutexW returns.
        unsafe {
            let _ = LocalFree(descriptor.cast());
        }

        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(create_status.cast_signed()));
        }
        if create_status == ERROR_ALREADY_EXISTS {
            // SAFETY: handle is a valid mutex handle returned above. This
            // thread did not acquire ownership of an existing mutex.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another bigcp run already owns this exact destination root",
            ));
        }
        Ok(Self { handle })
    }
}

impl Drop for DestinationLock {
    fn drop(&mut self) {
        // SAFETY: this guard was constructed only after acquiring initial
        // ownership. Both operations accept this live mutex handle.
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DestinationLock;

    #[test]
    fn exact_destination_mutex_excludes_a_second_thread() {
        let suffix = format!("test-{}", uuid::Uuid::new_v4().simple());
        let first = DestinationLock::acquire(&suffix);
        assert!(first.is_ok());
        let second_suffix = suffix.clone();
        let second =
            std::thread::spawn(move || DestinationLock::acquire(&second_suffix).err()).join();
        assert!(second.is_ok());
        assert!(
            second
                .ok()
                .flatten()
                .is_some_and(|error| { error.kind() == std::io::ErrorKind::WouldBlock })
        );
        drop(first);
        assert!(DestinationLock::acquire(&suffix).is_ok());
    }
}
