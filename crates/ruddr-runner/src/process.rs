//! Platform process setup: the app-server child in its own process group,
//! detached controllers, and process-tree termination.

use std::process::Command;

/// Puts the app-server child in its own process group on Unix, so the whole
/// tree can be signalled and a terminal's Ctrl-C reaches only the controller.
pub fn configure_child(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// Whether a failed detached start is worth retrying without leaving the
/// launching job: a Windows job that forbids breakaway rejects the flag.
pub const DETACH_SUPPORTS_BREAKAWAY: bool = cfg!(windows);

#[cfg(windows)]
pub mod windows_flags {
    pub const DETACHED_PROCESS: u32 = 0x0000_0008;
    pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
}

/// Starts a background controller so it survives the launching terminal or
/// SSH connection: a new session on Unix. On Windows the child leaves the
/// console, and with `breakaway` also the launching job, because Windows
/// OpenSSH kills every process in a session's job when the connection closes.
pub fn configure_detached(command: &mut Command, breakaway: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = breakaway;
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut flags = windows_flags::DETACHED_PROCESS | windows_flags::CREATE_NEW_PROCESS_GROUP;
        if breakaway {
            flags |= windows_flags::CREATE_BREAKAWAY_FROM_JOB;
        }
        command.creation_flags(flags);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (command, breakaway);
}

/// Stops this process's own standard handles from leaking into children it
/// did not hand them to. Windows children inherit every inheritable handle,
/// and the launcher's stdout and stderr are inheritable pipes when its caller
/// captures them: a detached controller holding them would keep the caller
/// reading until the run ends. Children given `Stdio::inherit` still get
/// them, because the standard library passes a duplicate.
pub fn keep_std_handles_private() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle and SetHandleInformation only read and flag
            // this process's own handle table entries.
            unsafe {
                let handle = GetStdHandle(which);
                if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
}

/// Signals the child's whole process tree. `force` sends SIGKILL instead of
/// SIGTERM on Unix; Windows always force-kills the tree. `reaped` says the
/// child itself was already waited for, so its PID must not be signalled
/// directly (it may belong to another process by now). The process group
/// outlives its leader while any member runs, so group signals stay safe.
pub fn terminate_process_tree(pid: u32, force: bool, reaped: bool) {
    if pid == 0 {
        return;
    }
    imp::terminate(pid, force, reaped);
}

#[cfg(unix)]
mod imp {
    pub fn terminate(pid: u32, force: bool, reaped: bool) {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return };
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: plain kill(2) calls on the child's process group and PID.
        unsafe {
            if libc::kill(-pid, signal) == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) && !reaped {
                libc::kill(pid, signal);
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    pub fn terminate(pid: u32, _force: bool, reaped: bool) {
        if reaped {
            // TODO(review): Own the provider tree with a Windows Job Object so descendants remain terminable after the parent exits.
            return;
        }
        let killed = std::process::Command::new("taskkill.exe")
            .args(super::taskkill_args(pid))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(super::windows_flags::CREATE_NO_WINDOW)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if killed {
            return;
        }
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn terminate(_pid: u32, _force: bool, _reaped: bool) {}
}

/// `taskkill` arguments that end a whole Windows process tree.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn taskkill_args(pid: u32) -> Vec<String> {
    vec!["/PID".into(), pid.to_string(), "/T".into(), "/F".into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taskkill_ends_the_tree() {
        assert_eq!(taskkill_args(1234), ["/PID", "1234", "/T", "/F"]);
    }

    #[cfg(unix)]
    #[test]
    fn terminate_kills_the_group_and_reaped_children_stay_safe() {
        let mut command = Command::new("sleep");
        command.arg("30");
        configure_child(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        terminate_process_tree(pid, false, false);
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(!ruddr_core::process::alive(pid as i64));
        // A second call after the reap only signals the (empty) group.
        terminate_process_tree(pid, true, true);
    }
}
