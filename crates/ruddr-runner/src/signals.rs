//! Cancellation for a foreground controller. SIGINT and SIGTERM on Unix, and
//! Ctrl-C, Ctrl-Break, and console close on Windows, cancel a token; the
//! controller then interrupts the turn, ends the provider tree, and persists
//! `interrupted`. Tests cancel the token directly.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A cancellation flag that threads can wait on.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        let (flag, condvar) = &*self.inner;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        condvar.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        *self.inner.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits up to `timeout` and reports whether the token is cancelled.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let (flag, condvar) = &*self.inner;
        let deadline = Instant::now() + timeout;
        let mut cancelled = flag.lock().unwrap_or_else(|e| e.into_inner());
        while !*cancelled {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            cancelled = condvar.wait_timeout(cancelled, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
        *cancelled
    }
}

/// Routes interrupt and termination signals to `token` for the rest of the
/// process. Later signals are absorbed, so a second Ctrl-C does not cut the
/// cleanup short.
pub fn install(token: &CancelToken) -> std::io::Result<()> {
    imp::install(token.clone())
}

#[cfg(unix)]
mod imp {
    use super::CancelToken;
    use std::io;
    use std::sync::atomic::{AtomicI32, Ordering};

    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_signal(_: libc::c_int) {
        let fd = WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = 1u8;
            // SAFETY: write(2) is async-signal-safe; the pipe is nonblocking.
            unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
        }
    }

    pub fn install(token: CancelToken) -> io::Result<()> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe(2) fills two descriptors; both get close-on-exec so
        // provider children never inherit them.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) == -1 {
                return Err(io::Error::last_os_error());
            }
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            let flags = libc::fcntl(fds[1], libc::F_GETFL);
            libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        WRITE_FD.store(fds[1], Ordering::Relaxed);
        let read_fd = fds[0];
        std::thread::Builder::new().name("ruddr-signals".into()).spawn(move || {
            let mut byte = 0u8;
            loop {
                // SAFETY: reads one byte into a stack buffer.
                let n = unsafe { libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) };
                if n == 1 {
                    token.cancel();
                } else if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return;
                }
            }
        })?;
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: installs a handler that only writes to the pipe.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::CancelToken;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler,
    };

    static TOKEN: OnceLock<CancelToken> = OnceLock::new();

    // Returns a Win32 BOOL, which is an i32.
    unsafe extern "system" fn handler(event: u32) -> i32 {
        let Some(token) = TOKEN.get() else { return 0 };
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                token.cancel();
                1
            }
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                token.cancel();
                // Windows ends the process when this returns; give the
                // controller time to stop the provider and persist state.
                std::thread::sleep(std::time::Duration::from_secs(10));
                1
            }
            _ => 0,
        }
    }

    pub fn install(token: CancelToken) -> std::io::Result<()> {
        let _ = TOKEN.set(token);
        // SAFETY: registers a handler that only touches the static token.
        if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn install(_token: super::CancelToken) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_wakes_waiters() {
        let token = CancelToken::new();
        assert!(!token.wait_timeout(Duration::from_millis(5)));
        let clone = token.clone();
        let waiter = std::thread::spawn(move || clone.wait_timeout(Duration::from_secs(5)));
        token.cancel();
        assert!(waiter.join().unwrap());
        assert!(token.is_cancelled());
    }
}
