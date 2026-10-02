//! `run --detach` and `--prompt-file -`. The detached controller runs in its
//! own session (or, on Windows, outside the console and the launching job),
//! writes its early stderr to `launch.stderr.log`, and the launcher returns
//! once the run reports a live or finished state.

use ruddr_core::state::{self, Status};
use ruddr_core::{Error, Result};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const STDIN_PROMPT_FILE: &str = "prompt.md";
pub const LAUNCH_STDERR_FILE: &str = "launch.stderr.log";
pub const STARTUP_WINDOW: Duration = Duration::from_secs(15);

/// Stores a prompt read from stdin as a private `prompt.md` in the state
/// directory, so the controller reads it like any other prompt file.
pub fn write_stdin_prompt(state_dir: &Path, mut stdin: impl Read) -> Result<PathBuf> {
    let state_dir = ruddr_core::paths::absolute(state_dir);
    ruddr_core::fsutil::create_private_dir(&state_dir)?;
    if let Ok(existing) = state::read_state(&state_dir) {
        return Err(Error::failed(format!(
            "state directory already contains a Ruddr run with status {}; use a new --state-dir",
            existing.status
        )));
    }
    let mut raw = Vec::new();
    stdin
        .read_to_end(&mut raw)
        .map_err(|e| Error::failed(format!("read prompt from stdin: {e}")))?;
    if String::from_utf8_lossy(&raw).trim().is_empty() {
        return Err(Error::failed("prompt from stdin is empty"));
    }
    let path = state_dir.join(STDIN_PROMPT_FILE);
    let mut file = match ruddr_core::fsutil::create_private_file_new(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::failed(format!(
                "state directory already contains {STDIN_PROMPT_FILE}; use a new --state-dir"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    file.write_all(&raw)?;
    Ok(path)
}

/// What the launcher reports about a detached run.
#[derive(Debug, Clone, PartialEq)]
pub struct Startup {
    pub state_dir: String,
    pub pid: i64,
    pub status: Status,
}

/// Launches `command` (the program and its arguments) as a detached
/// controller and waits up to `window` for it to report a state. A
/// controller still starting when the window closes keeps running:
/// retrying could create a second run.
pub fn start_detached_run(state_dir: &Path, command: &[OsString], window: Duration) -> Result<Startup> {
    let state_dir = ruddr_core::paths::absolute(state_dir);
    ruddr_core::fsutil::create_private_dir(&state_dir)?;
    // Append: the TUI creates this log before launching `run --detach` and
    // captures the launcher's own stderr in it.
    let stderr_path = state_dir.join(LAUNCH_STDERR_FILE);
    let stderr = ruddr_core::fsutil::open_private_append(&stderr_path)?;
    let start = |breakaway: bool| -> std::io::Result<std::process::Child> {
        let mut child = std::process::Command::new(&command[0]);
        child
            .args(&command[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(stderr.try_clone()?));
        crate::process::configure_detached(&mut child, breakaway);
        child.spawn()
    };
    crate::process::keep_std_handles_private();
    let mut started = start(crate::process::DETACH_SUPPORTS_BREAKAWAY);
    if started.is_err() && crate::process::DETACH_SUPPORTS_BREAKAWAY {
        // The launching job forbids breakaway. The run then survives a closed
        // console but not the end of an SSH session.
        started = start(false);
    }
    drop(stderr);
    let mut child = started.map_err(|e| Error::failed(format!("start detached run: {e}")))?;
    let exited = Arc::new(AtomicBool::new(false));
    let flag = exited.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        flag.store(true, Ordering::SeqCst);
    });
    wait_for_startup(
        &state_dir,
        &stderr_path,
        &exited,
        Instant::now() + window,
        Duration::from_millis(25),
    )
}

/// Polls `state.json` until the run is live or finished, the launched
/// process exits, or `deadline` passes.
pub fn wait_for_startup(state_dir: &Path, stderr_path: &Path, exited: &AtomicBool, deadline: Instant, tick: Duration) -> Result<Startup> {
    let startup = |s: &state::RunState| Startup {
        state_dir: s.state_dir.clone(),
        pid: s.pid,
        status: s.status,
    };
    loop {
        let child_exited = exited.load(Ordering::SeqCst);
        let read = state::read_state(state_dir);
        if let Ok(current) = &read {
            match current.status {
                Status::Active | Status::Idle | Status::Completed => return Ok(startup(current)),
                Status::Failed | Status::Interrupted => {
                    return Err(startup_error(state_dir, stderr_path, current.error.as_deref().unwrap_or_default()));
                }
                _ => {}
            }
        }
        if child_exited {
            // The controller may have persisted state just before exiting.
            if let Ok(last) = state::read_state(state_dir)
                && last.status == Status::Completed
            {
                return Ok(startup(&last));
            }
            return Err(startup_error(state_dir, stderr_path, ""));
        }
        if Instant::now() >= deadline {
            return Ok(match read {
                Ok(current) => startup(&current),
                Err(_) => Startup {
                    state_dir: state_dir.to_string_lossy().into_owned(),
                    pid: 0,
                    status: Status::Starting,
                },
            });
        }
        std::thread::sleep(tick);
    }
}

fn startup_error(state_dir: &Path, stderr_path: &Path, state_error: &str) -> Error {
    let raw = std::fs::read(stderr_path).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    let mut diagnostic = text.trim();
    if diagnostic.len() > 4096 {
        let mut start = diagnostic.len() - 4096;
        while !diagnostic.is_char_boundary(start) {
            start += 1;
        }
        diagnostic = &diagnostic[start..];
    }
    let diagnostic = [diagnostic, state_error, "run exited during startup"]
        .into_iter()
        .find(|d| !d.is_empty())
        .unwrap_or_default();
    Error::failed(format!("{diagnostic} (state dir {})", state_dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn stdin_prompts_are_private_and_never_reused() {
        let dir = TempDir::new("stdin");
        let state_dir = dir.join("run");
        let path = write_stdin_prompt(&state_dir, "do the thing\n".as_bytes()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "do the thing\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert!(write_stdin_prompt(&state_dir, "again".as_bytes()).is_err());
        assert!(write_stdin_prompt(&dir.join("empty"), "  \n".as_bytes()).is_err());
    }

    #[test]
    fn slow_starts_keep_running() {
        let dir = TempDir::new("slow");
        let startup = wait_for_startup(
            &dir,
            &dir.join(LAUNCH_STDERR_FILE),
            &AtomicBool::new(false),
            Instant::now(),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(startup.status, Status::Starting);
        assert_eq!(startup.pid, 0);
    }

    #[test]
    fn startup_errors_prefer_stderr() {
        let dir = TempDir::new("crash");
        let stderr = dir.join(LAUNCH_STDERR_FILE);
        let error = wait_for_startup(&dir, &stderr, &AtomicBool::new(true), Instant::now(), Duration::from_millis(1)).unwrap_err();
        assert!(error.message.starts_with("run exited during startup (state dir"), "{error}");
        std::fs::write(&stderr, "provider binary not found\n").unwrap();
        let error = wait_for_startup(&dir, &stderr, &AtomicBool::new(true), Instant::now(), Duration::from_millis(1)).unwrap_err();
        assert!(error.message.starts_with("provider binary not found (state dir"), "{error}");
    }
}
