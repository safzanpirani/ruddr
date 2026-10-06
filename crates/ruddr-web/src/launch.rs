//! Starting runs from the dashboard: new sessions and continuations of
//! finished threads. Each launch gets a private directory under
//! `CWD/.scratch/ruddr-tui` holding `prompt.md` and `launch.stderr.log`, and
//! runs `ruddr run --detach` so the controller outlives the server. Port of
//! tui/session-launch.ts and the argument builders in tui/core.ts.

use ruddr_core::state::RunState;
use ruddr_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const PROVIDERS: [&str; 6] = ["codex", "claude", "opencode", "pi", "omp", "droid"];
pub const STARTUP_WINDOW: Duration = Duration::from_millis(1500);

/// Inserts `--detach` after `run` unless the flags already hold it.
pub fn detached_run_arguments(args: Vec<String>) -> Vec<String> {
    if args.first().map(String::as_str) != Some("run") {
        return args;
    }
    let flags_end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    if args[1..flags_end].iter().any(|a| a == "--detach") {
        return args;
    }
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push("run".to_string());
    out.push("--detach".to_string());
    out.extend(args.into_iter().skip(1));
    out
}

/// A new run that resumes `session`'s thread in its working directory.
pub fn continuation_run_arguments(session: &RunState, prompt_file: &Path, state_dir: &Path, model: Option<&str>) -> Result<Vec<String>> {
    let (Some(thread), false) = (session.thread_id.as_deref().filter(|t| !t.is_empty()), session.cwd.is_empty()) else {
        return Err(Error::failed("continuation requires a thread and working directory"));
    };
    let provider = if session.provider.is_empty() { "codex" } else { &session.provider };
    let sandbox = if session.sandbox.is_empty() {
        "workspace-write"
    } else {
        &session.sandbox
    };
    let mut args = strings(&["run", "--provider", provider, "--cwd", &session.cwd, "--resume-thread", thread]);
    args.extend([
        "--prompt-file".into(),
        path_arg(prompt_file),
        "--state-dir".into(),
        path_arg(state_dir),
    ]);
    args.extend(strings(&["--sandbox", sandbox, "--approval-policy", "never", "--idle"]));
    let model = model
        .filter(|m| !m.is_empty())
        .or(Some(session.model.as_str()).filter(|m| !m.is_empty()));
    if let Some(model) = model {
        args.extend(strings(&["--model", model]));
    }
    if let Some(effort) = session.effort.as_deref().filter(|e| !e.is_empty()) {
        args.extend(strings(&["--effort", effort]));
    }
    Ok(args)
}

pub struct NewSession<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub cwd: &'a Path,
    pub resume_thread_id: Option<&'a str>,
}

pub fn new_session_run_arguments(options: &NewSession, prompt_file: &Path, state_dir: &Path) -> Vec<String> {
    let mut args = strings(&["run", "--provider", options.provider]);
    args.extend(["--cwd".into(), path_arg(options.cwd), "--prompt-file".into(), path_arg(prompt_file)]);
    args.extend(["--state-dir".into(), path_arg(state_dir)]);
    args.extend(strings(&["--sandbox", "workspace-write", "--approval-policy", "never", "--idle"]));
    if let Some(thread) = options.resume_thread_id.filter(|t| !t.is_empty()) {
        args.extend(strings(&["--resume-thread", thread]));
    }
    if let Some(model) = options.model.filter(|m| !m.is_empty()) {
        args.extend(strings(&["--model", model]));
    }
    if let Some(effort) = options.effort.filter(|e| !e.is_empty()) {
        args.extend(strings(&["--effort", effort]));
    }
    args
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Creates the launch bundle and starts `ruddr run --detach`. Blocks for at
/// most `window` while the controller starts. A run still starting after the
/// window counts as launched: retrying it could create a second live session.
pub fn launch_session(
    ruddr: &Path,
    cwd: &Path,
    message: &str,
    arguments_for_files: impl FnOnce(&Path, &Path) -> Result<Vec<String>>,
    on_spawn: impl FnOnce(&Path),
    window: Duration,
) -> Result<PathBuf> {
    let base = ruddr_core::paths::launch_runs_dir(cwd);
    ruddr_core::paths::ensure_ignored_runs_dir(&base).map_err(|e| Error::failed(format!("create {}: {e}", base.display())))?;
    let state_dir = create_launch_dir(&base)?;
    let prompt_file = state_dir.join("prompt.md");
    {
        let mut prompt = ruddr_core::fsutil::create_private_file_new(&prompt_file)?;
        std::io::Write::write_all(&mut prompt, format!("{message}\n").as_bytes())?;
    }
    let stderr_path = state_dir.join("launch.stderr.log");
    let stderr = ruddr_core::fsutil::create_private_file_new(&stderr_path)?;
    let args = detached_run_arguments(arguments_for_files(&prompt_file, &state_dir)?);
    let mut child = Command::new(ruddr)
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|e| Error::failed(format!("start {}: {e}", ruddr.display())))?;
    on_spawn(&state_dir);
    let deadline = Instant::now() + window;
    let mut exit: Option<Option<i32>> = None;
    loop {
        if exit.is_none()
            && let Ok(Some(status)) = child.try_wait()
        {
            exit = Some(status.code());
        }
        // The controller creates state after spawn; a missing file is expected.
        let status = std::fs::read(state_dir.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|state| state.get("status").and_then(|s| s.as_str()).map(str::to_string));
        let status = status.as_deref();
        let failed_exit = matches!(exit, Some(code) if code != Some(0)) && status != Some("completed");
        if matches!(status, Some("failed" | "interrupted")) || failed_exit {
            let diagnostic = ruddr_core::fsutil::read_tail(&stderr_path, 4096)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !diagnostic.is_empty() {
                return Err(Error::failed(diagnostic));
            }
            let code = match exit {
                Some(Some(code)) => format!(" ({code})"),
                Some(None) => " (signal)".into(),
                None => String::new(),
            };
            return Err(Error::failed(format!(
                "Session exited during startup{code}; see {}",
                state_dir.display()
            )));
        }
        // `run --detach` exits 0 once the controller runs on its own.
        if matches!(status, Some("active" | "idle" | "completed")) || exit == Some(Some(0)) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if exit.is_none() {
        // Reap the launcher whenever it exits.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    Ok(state_dir)
}

fn create_launch_dir(base: &Path) -> Result<PathBuf> {
    for _ in 0..16 {
        let dir = ruddr_core::paths::new_run_dir_name(base);
        match ruddr_core::fsutil::create_private_dir_new(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::failed(format!("create {}: {e}", dir.display()))),
        }
    }
    Err(Error::failed(format!("could not create a launch directory in {}", base.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_detach_once() {
        let args = detached_run_arguments(strings(&["run", "--provider", "codex"]));
        assert_eq!(args, strings(&["run", "--detach", "--provider", "codex"]));
        assert_eq!(detached_run_arguments(args.clone()), args);
        assert_eq!(detached_run_arguments(strings(&["steer"])), strings(&["steer"]));
        assert_eq!(
            detached_run_arguments(strings(&["run", "--", "--detach"])),
            strings(&["run", "--detach", "--", "--detach"])
        );
    }

    #[test]
    fn builds_new_session_arguments() {
        let options = NewSession {
            provider: "claude",
            model: Some("m"),
            effort: Some(""),
            cwd: Path::new("/w"),
            resume_thread_id: Some("t"),
        };
        let args = new_session_run_arguments(&options, Path::new("/w/p.md"), Path::new("/w/s"));
        assert_eq!(
            args,
            strings(&[
                "run",
                "--provider",
                "claude",
                "--cwd",
                "/w",
                "--prompt-file",
                "/w/p.md",
                "--state-dir",
                "/w/s",
                "--sandbox",
                "workspace-write",
                "--approval-policy",
                "never",
                "--idle",
                "--resume-thread",
                "t",
                "--model",
                "m"
            ])
        );
    }
}
