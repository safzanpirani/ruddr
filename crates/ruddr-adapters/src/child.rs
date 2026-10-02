//! A provider CLI child process with piped stdin and stdout. Stderr is
//! inherited, so provider diagnostics reach the runner's
//! `provider.stderr.log`.
//!
//! Writes go through one writer thread per child, in order. A caller that
//! needs to know a write landed waits for it with a deadline, so a provider
//! that stops reading its stdin never blocks an adapter thread forever.

use crate::protocol::lock;
use std::io::{self, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct Job {
    line: Vec<u8>,
    ack: Option<mpsc::SyncSender<io::Result<()>>>,
}

pub struct ChildProcess {
    child: Mutex<Child>,
    writer: Mutex<Option<mpsc::Sender<Job>>>,
    /// Signalled directly on Unix; Windows kills through the child handle.
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: u32,
}

impl ChildProcess {
    /// Spawns the command with piped stdin/stdout and inherited stderr.
    pub fn spawn(mut command: Command) -> io::Result<(Arc<ChildProcess>, ChildStdout)> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| io::Error::other("child stdin is not piped"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("child stdout is not piped"))?;
        let (sender, receiver) = mpsc::channel::<Job>();
        thread::spawn(move || {
            for job in receiver {
                let written = stdin.write_all(&job.line).and_then(|()| stdin.flush());
                let failed = written.is_err();
                if let Some(ack) = job.ack {
                    let _ = ack.send(written);
                }
                if failed {
                    break;
                }
            }
            // Dropping stdin here closes the pipe once every queued line is out.
        });
        let pid = child.id();
        Ok((
            Arc::new(ChildProcess {
                child: Mutex::new(child),
                writer: Mutex::new(Some(sender)),
                pid,
            }),
            stdout,
        ))
    }

    /// Queues a line and waits up to `timeout` for it to reach the pipe.
    pub fn write(&self, mut line: Vec<u8>, timeout: Duration) -> Result<(), String> {
        line.push(b'\n');
        let (ack, done) = mpsc::sync_channel(1);
        self.send(Job { line, ack: Some(ack) })?;
        match done.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("write failed: {error}")),
            Err(RecvTimeoutError::Timeout) => Err(format!("write timed out after {}ms", timeout.as_millis())),
            Err(RecvTimeoutError::Disconnected) => Err("process is not running".into()),
        }
    }

    /// Queues a line without waiting for it.
    pub fn enqueue(&self, mut line: Vec<u8>) -> Result<(), String> {
        line.push(b'\n');
        self.send(Job { line, ack: None })
    }

    fn send(&self, job: Job) -> Result<(), String> {
        match lock(&self.writer).as_ref() {
            Some(sender) => sender.send(job).map_err(|_| "process is not running".to_string()),
            None => Err("process stdin is closed".into()),
        }
    }

    /// Closes stdin after every queued line is written.
    pub fn close_stdin(&self) {
        lock(&self.writer).take();
    }

    /// Asks the process to stop: SIGTERM on Unix, TerminateProcess elsewhere.
    pub fn terminate(&self) {
        let mut child = lock(&self.child);
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            // SAFETY: kill(2) with a pid this process spawned and has not reaped.
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill();
        }
    }

    /// Kills the process outright.
    pub fn kill(&self) {
        let mut child = lock(&self.child);
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
        }
    }

    /// The exit status once the process has exited.
    pub fn try_status(&self) -> Option<std::process::ExitStatus> {
        lock(&self.child).try_wait().ok().flatten()
    }

    /// Waits up to `timeout` for the process to exit; true when it has.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.try_status().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Closes stdin, waits `grace` for a clean exit, then terminates and
    /// waits another second. This is how the TypeScript clients closed.
    pub fn shut_down(&self, grace: Duration) {
        self.close_stdin();
        if !self.wait_timeout(grace) {
            self.terminate();
            self.wait_timeout(Duration::from_secs(1));
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // Reap the child if it already exited so it does not linger as a zombie.
        let _ = lock(&self.child).try_wait();
    }
}

/// Describes an exit status the way the Claude Agent SDK words it.
pub fn describe_exit(status: std::process::ExitStatus) -> Option<String> {
    if let Some(code) = status.code() {
        return (code != 0).then(|| format!("exited with code {code}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(format!("terminated by signal {}", signal_name(signal)));
        }
    }
    None
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGHUP => "SIGHUP".into(),
        libc::SIGPIPE => "SIGPIPE".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGABRT => "SIGABRT".into(),
        other => format!("{other}"),
    }
}

/// Finds an executable on PATH, trying each name in order.
pub fn find_on_path(names: &[&str]) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for name in names {
        for dir in std::env::split_paths(&path) {
            for candidate in executable_candidates(&dir.join(name)) {
                if is_executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn executable_candidates(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    vec![base.to_path_buf()]
}

#[cfg(not(unix))]
fn executable_candidates(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
    let mut candidates = vec![base.to_path_buf()];
    for ext in exts.split(';').filter(|ext| !ext.is_empty()) {
        let mut name = base.as_os_str().to_os_string();
        name.push(ext.to_ascii_lowercase());
        candidates.push(name.into());
    }
    candidates
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path).map(|meta| meta.is_file()).unwrap_or(false)
}

/// The host name Droid records as the session's machine ID.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is valid for its full length, and gethostname
        // NUL-terminates within it on success.
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result == 0 {
            let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
        String::from("localhost")
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
    }
}
