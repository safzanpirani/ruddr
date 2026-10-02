//! Process liveness. Spawning, detaching, and tree termination belong to the
//! runner crate; this is the read-only check every command needs.

/// Whether `pid` names a live process. A process owned by another user counts
/// as alive.
pub fn alive(pid: i64) -> bool {
    if pid <= 0 || pid > u32::MAX as i64 {
        return false;
    }
    imp::alive(pid as u32)
}

#[cfg(unix)]
mod imp {
    pub fn alive(pid: u32) -> bool {
        if pid > i32::MAX as u32 {
            return false;
        }
        // SAFETY: signal 0 only checks existence and permission.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

    pub fn alive(pid: u32) -> bool {
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let result = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            result == WAIT_TIMEOUT
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn current_process_is_alive() {
        assert!(super::alive(std::process::id() as i64));
        assert!(!super::alive(0));
        assert!(!super::alive(-5));
    }
}
