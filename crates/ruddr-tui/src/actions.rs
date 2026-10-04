//! Work that leaves the UI thread: control requests to a live controller,
//! detached launches of new and continued runs, git diffs, and helper
//! processes (model catalog, deja, update). Each runs on its own thread and
//! reports back over the UI channel.

use crate::core::{PromptRoute, revalidate_route};
use ruddr_core::control::{self, Command as ControlCommand, Request};
use ruddr_core::state::Status;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STEER_TIMEOUT: Duration = Duration::from_secs(30);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(35);

/// Sends a typed prompt along the route it was typed for. The route is
/// checked again against fresh state right before sending, and a steer
/// carries the turn the user saw. A mismatch returns an error; it never
/// becomes a different kind of request.
pub fn send_prompt(
    state_dir: &Path,
    route: PromptRoute,
    observed_turn: Option<&str>,
    text: &str,
    images: &[String],
) -> Result<String, String> {
    let fresh = ruddr_core::state::read_state(state_dir).map_err(|e| e.message)?.displayed();
    revalidate_route(&fresh, route, observed_turn)?;
    let request = match route {
        PromptRoute::Steer => Request {
            command: ControlCommand::Steer,
            images: images.to_vec(),
            text: Some(text.into()),
            expected_turn_id: fresh.turn_id.clone(),
        },
        PromptRoute::Prompt => Request {
            command: ControlCommand::Prompt,
            images: images.to_vec(),
            text: Some(text.into()),
            expected_turn_id: None,
        },
        PromptRoute::Continue => return Err("a continuation starts a new run, not a control request".into()),
    };
    let timeout = if route == PromptRoute::Steer {
        STEER_TIMEOUT
    } else {
        PROMPT_TIMEOUT
    };
    control::call(state_dir, &request, timeout).map_err(|e| e.message)?;
    Ok(match route {
        PromptRoute::Steer => "Steer delivered".into(),
        _ => "Prompt sent".into(),
    })
}

/// Ends an idle session or interrupts the active turn the user saw.
pub fn stop(state_dir: &Path, observed: Status, observed_turn: Option<&str>) -> Result<String, String> {
    let fresh = ruddr_core::state::read_state(state_dir).map_err(|e| e.message)?.displayed();
    if fresh.status != observed {
        return Err(format!("The session is now {}; nothing was stopped", fresh.status));
    }
    let request = match observed {
        Status::Idle => Request {
            command: ControlCommand::Stop,
            images: vec![],
            text: None,
            expected_turn_id: None,
        },
        Status::Active => {
            if fresh.turn_id.as_deref() != observed_turn {
                return Err("The turn changed; nothing was interrupted".into());
            }
            Request {
                command: ControlCommand::Interrupt,
                images: vec![],
                text: None,
                expected_turn_id: fresh.turn_id.clone(),
            }
        }
        other => return Err(format!("A {other} session cannot be stopped")),
    };
    let timeout = if observed == Status::Idle {
        STOP_TIMEOUT
    } else {
        INTERRUPT_TIMEOUT
    };
    control::call(state_dir, &request, timeout).map_err(|e| e.message)?;
    Ok(if observed == Status::Idle {
        "Shutdown requested".into()
    } else {
        "Interrupt requested".into()
    })
}

/// The `ruddr` binary to launch runs with: this executable.
pub fn ruddr_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ruddr"))
}

/// Creates a private run directory under `CWD/.scratch/ruddr-tui`, writes
/// `prompt.md`, and starts `ruddr run --detach ...` with its stderr in
/// `launch.stderr.log`, so the run outlives this TUI. Returns the state
/// directory once the launcher reports the controller running.
pub fn launch(
    exe: &Path,
    cwd: &Path,
    message: &str,
    build: impl FnOnce(&str, &str) -> Vec<String>,
    on_spawn: impl FnOnce(&Path),
) -> Result<PathBuf, String> {
    let base = ruddr_core::paths::launch_runs_dir(cwd);
    ruddr_core::paths::ensure_ignored_runs_dir(&base).map_err(|e| format!("create {}: {e}", base.display()))?;
    let dir = (0..4)
        .find_map(|_| {
            let dir = ruddr_core::paths::new_run_dir_name(&base);
            ruddr_core::fsutil::create_private_dir_new(&dir).ok().map(|_| dir)
        })
        .ok_or_else(|| format!("cannot create a run directory under {}", base.display()))?;
    let prompt = dir.join("prompt.md");
    {
        use std::io::Write;
        let mut file = ruddr_core::fsutil::create_private_file_new(&prompt).map_err(|e| e.to_string())?;
        writeln!(file, "{message}").map_err(|e| e.to_string())?;
    }
    let stderr_path = dir.join("launch.stderr.log");
    let stderr = ruddr_core::fsutil::create_private_file_new(&stderr_path).map_err(|e| e.to_string())?;
    let (prompt_s, dir_s) = (prompt.to_string_lossy().into_owned(), dir.to_string_lossy().into_owned());
    let mut child = Command::new(exe)
        .args(build(&prompt_s, &dir_s))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("start {}: {e}", exe.display()))?;
    // `run --detach` exits once the controller runs on its own or failed to start.
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        let log = ruddr_core::fsutil::read_tail(&stderr_path, 4096).unwrap_or_default();
        let log = log.trim();
        return Err(if log.is_empty() {
            format!("Session exited during startup ({status}); see {dir_s}")
        } else {
            log.to_string()
        });
    }
    on_spawn(&dir);
    Ok(dir)
}

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);

/// macOS: a copied image file wins over its Finder icon; otherwise the
/// clipboard's PNG data goes into the file named by the first argument.
#[cfg(target_os = "macos")]
const MAC_CLIPBOARD_SCRIPT: &[&str] = &[
    "on run argv",
    "try",
    "return \"file:\" & POSIX path of (the clipboard as «class furl»)",
    "end try",
    "set png to the clipboard as «class PNGf»",
    "set f to open for access (POSIX file (item 1 of argv)) with write permission",
    "set eof f to 0",
    "write png to f",
    "close access f",
    "return \"png\"",
    "end run",
];

#[cfg(windows)]
const WINDOWS_CLIPBOARD_SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
    $files = [System.Windows.Forms.Clipboard]::GetFileDropList(); \
    if ($files.Count -gt 0) { Write-Output ('file:' + $files[0]); exit 0 }; \
    $image = [System.Windows.Forms.Clipboard]::GetImage(); if ($null -eq $image) { exit 3 }; \
    $image.Save($env:RUDDR_PASTE_PATH, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'png'";

/// Saves the clipboard's image as a private PNG in `dir`, or returns the
/// image file the clipboard holds a copy of. The terminal never sees image
/// data, so this asks the platform clipboard tool directly.
pub fn paste_clipboard_image(dir: &Path) -> Result<PathBuf, String> {
    ruddr_core::fsutil::create_private_dir(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let target = PathBuf::from(format!("{}.png", ruddr_core::paths::new_run_dir_name(dir).display()));
    let file = ruddr_core::fsutil::create_private_file_new(&target).map_err(|e| e.to_string())?;
    let result = read_clipboard_into(&target, file);
    let saved = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    let outcome = match result {
        Ok(Some(copied)) => {
            let _ = std::fs::remove_file(&target);
            return ruddr_core::images::checked_image(&copied)
                .map_err(|_| "The copied file is not a png, jpg, gif, or webp image".to_string());
        }
        Ok(None) if saved > 0 => Ok(target.clone()),
        Ok(None) => Err("The clipboard holds no image".to_string()),
        Err(error) => Err(error),
    };
    if outcome.is_err() {
        let _ = std::fs::remove_file(&target);
    }
    outcome
}

/// Fills `target` with the clipboard image, or returns the path of a copied
/// file instead.
fn read_clipboard_into(target: &Path, file: std::fs::File) -> Result<Option<PathBuf>, String> {
    let copied = |stdout: &str| stdout.trim().strip_prefix("file:").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    {
        drop(file);
        let mut command = Command::new("osascript");
        for line in MAC_CLIPBOARD_SCRIPT {
            command.args(["-e", line]);
        }
        command.arg(target);
        let (stdout, _, ok, _) = run_bounded(command, CLIPBOARD_TIMEOUT, 64 * 1024)?;
        if !ok {
            return Err("The clipboard holds no image".into());
        }
        Ok(copied(&stdout))
    }
    #[cfg(windows)]
    {
        drop(file);
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", WINDOWS_CLIPBOARD_SCRIPT])
            .env("RUDDR_PASTE_PATH", target);
        let (stdout, _, ok, _) = run_bounded(command, CLIPBOARD_TIMEOUT, 64 * 1024)?;
        if !ok {
            return Err("The clipboard holds no image".into());
        }
        Ok(copied(&stdout))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (target, copied);
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some() && on_path("wl-paste");
        let mut command = if wayland {
            let mut c = Command::new("wl-paste");
            c.args(["--no-newline", "--type", "image/png"]);
            c
        } else if on_path("xclip") {
            let mut c = Command::new("xclip");
            c.args(["-selection", "clipboard", "-t", "image/png", "-o"]);
            c
        } else {
            return Err("Install wl-clipboard or xclip to paste images".into());
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) if status.success() => return Ok(None),
                Some(_) => return Err("The clipboard holds no image".into()),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("Reading the clipboard timed out".into());
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

/// Runs a command with a deadline and bounded output, like the Bun TUI's
/// git reads. Returns (stdout, stderr, success).
pub fn run_bounded(mut command: Command, timeout: Duration, max_bytes: usize) -> Result<(String, String, bool, bool), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let truncated = (&mut stdout).take(max_bytes as u64 + 1).read_to_end(&mut buffer).is_ok() && buffer.len() > max_bytes;
        // Keep draining so the child never blocks on a full pipe.
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        buffer.truncate(max_bytes);
        (buffer, truncated)
    });
    let err = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buffer);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}", ruddr_core::duration::format(timeout)));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let (stdout, truncated) = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        status.success(),
        truncated,
    ))
}

const DIFF_MAX_BYTES: usize = 2 * 1024 * 1024;
const DIFF_TIMEOUT: Duration = Duration::from_secs(3);

/// Tracked changes against HEAD. A repository without commits shows the
/// staged and unstaged changes instead.
/// Whether `cwd` is inside a Git work tree that `git diff` can describe.
pub fn is_git_work_tree(cwd: &str) -> bool {
    let mut command = Command::new("git");
    command.args(["-C", cwd, "rev-parse", "--is-inside-work-tree"]);
    matches!(run_bounded(command, DIFF_TIMEOUT, 64), Ok((out, _, true, _)) if out.trim() == "true")
}

/// The edits a run recorded in its own event log, as a git-style diff.
pub fn recorded_diff(events_path: &std::path::Path, cwd: &str) -> Result<String, String> {
    match std::fs::read_to_string(events_path) {
        Ok(text) => Ok(ruddr_history::app_server::run_diff(&text, cwd)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("Read {}: {e}", events_path.display())),
    }
}

pub fn workspace_diff(cwd: &str) -> Result<String, String> {
    let git = |extra: &[&str]| {
        let mut command = Command::new("git");
        command
            .args(["-C", cwd, "diff", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3"])
            .args(extra);
        run_bounded(command, DIFF_TIMEOUT, DIFF_MAX_BYTES).map_err(|e| format!("Git diff {e}."))
    };
    let note = |mut text: String, truncated: bool| {
        if truncated {
            text.push_str("\n\\ Diff truncated at 2 MiB");
        }
        text
    };
    let (head, head_err, ok, truncated) = git(&["HEAD", "--"])?;
    if ok || truncated {
        return Ok(note(head, truncated));
    }
    let (staged, _, staged_ok, staged_cut) = git(&["--cached", "--"])?;
    let (unstaged, _, unstaged_ok, unstaged_cut) = git(&["--"])?;
    if staged_ok && unstaged_ok {
        let joined = [staged, unstaged]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(note(joined, staged_cut || unstaged_cut));
    }
    let head_err = head_err.trim();
    Err(if head_err.is_empty() {
        "Git diff is unavailable.".into()
    } else {
        head_err.into()
    })
}

/// Files whose mtime is at or after the session start: the session's edits.
pub fn touched_since(cwd: &str, paths: &[String], started_at: &str) -> Vec<String> {
    let Some(since) = crate::core::parse_time(started_at) else {
        return vec![];
    };
    paths
        .iter()
        .filter(|path| {
            std::fs::metadata(Path::new(cwd).join(path))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|t| t.as_millis() as i64 >= since - 1000)
        })
        .cloned()
        .collect()
}

pub fn current_branch(cwd: &str) -> String {
    let mut command = Command::new("git");
    command.args(["-C", cwd, "branch", "--show-current"]);
    match run_bounded(command, Duration::from_secs(2), 4096) {
        Ok((out, _, true, _)) => out.trim().to_string(),
        _ => String::new(),
    }
}

/// Runs this binary with `args` and returns its trimmed stdout.
pub fn run_ruddr(exe: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new(exe);
    command.args(args);
    let (out, err, ok, _) = run_bounded(command, timeout, 4 * 1024 * 1024)?;
    if ok {
        Ok(out.trim().to_string())
    } else {
        let err = err.trim();
        Err(if err.is_empty() {
            format!("ruddr {} failed", args.first().unwrap_or(&""))
        } else {
            err.to_string()
        })
    }
}

/// Whether `binary` is on PATH (with PATHEXT suffixes on Windows).
pub fn on_path(binary: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false };
    let suffixes: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_string())
            .chain([String::new()])
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&paths).any(|dir| suffixes.iter().any(|suffix| dir.join(format!("{binary}{suffix}")).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::core::LaunchOverrides;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ruddr-tui-{name}-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn a_directory_outside_git_shows_the_runs_recorded_edits() {
        let dir = temp("nogit");
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        assert!(!is_git_work_tree(&cwd), "a temp directory is not a work tree");
        let events = dir.join("events.jsonl");
        let edit = serde_json::json!({"method": "item/completed", "params": {"item": {
            "type": "fileChange", "id": "1", "status": "completed", "toolName": "Write",
            "input": {"file_path": format!("{cwd}/notes.md"), "content": "hello\n"}}}});
        std::fs::write(&events, format!("{edit}\n")).unwrap();
        let diff = recorded_diff(&events, &cwd).unwrap();
        assert!(diff.starts_with("diff --git a/notes.md b/notes.md\nnew file mode"), "{diff}");
        assert!(diff.contains("+hello"));
        assert_eq!(
            recorded_diff(&dir.join("missing.jsonl"), &cwd).unwrap(),
            "",
            "no log yet means no edits"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn launch_writes_a_private_bundle_and_runs_detached() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = temp("launch");
        // A stand-in for `ruddr` that records its arguments.
        let fake = cwd.join("fake-ruddr");
        std::fs::write(&fake, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/args.txt\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = launch(
            &fake,
            &cwd,
            "fix the bug",
            |p, d| crate::core::new_session_args("codex", "/w", p, d, &LaunchOverrides::default(), None),
            |_| {},
        )
        .unwrap();
        assert!(dir.starts_with(cwd.join(".scratch/ruddr-tui")));
        assert_eq!(std::fs::read_to_string(cwd.join(".scratch/ruddr-tui/.gitignore")).unwrap(), "*\n");
        assert_eq!(std::fs::read_to_string(dir.join("prompt.md")).unwrap(), "fix the bug\n");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("prompt.md")), 0o600);
        assert_eq!(mode(&dir.join("launch.stderr.log")), 0o600);
        let args = std::fs::read_to_string(cwd.join("args.txt")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(&args[..2], ["run", "--detach"]);
        assert!(args.windows(2).any(|w| w == ["--state-dir", dir.to_str().unwrap()]));
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn launch_reports_startup_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = temp("launch-fail");
        let fake = cwd.join("fake-ruddr");
        std::fs::write(&fake, "#!/bin/sh\necho 'ruddr: unknown provider' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let accepted = std::cell::Cell::new(false);
        let error = launch(&fake, &cwd, "x", |_, _| vec!["run".into()], |_| accepted.set(true)).unwrap_err();
        assert!(!accepted.get(), "a failed launcher must not select a new session");
        let run = std::fs::read_dir(cwd.join(".scratch/ruddr-tui"))
            .unwrap()
            .flatten()
            .find(|e| e.path().is_dir())
            .unwrap()
            .path();
        assert_eq!(std::fs::read_to_string(run.join("prompt.md")).unwrap(), "x\n");
        assert_eq!(error, "ruddr: unknown provider");
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn control_refuses_dead_or_changed_sessions_without_sending() {
        let dir = temp("control");
        let mut state = crate::core::tests_support::session(Status::Active);
        state.state_dir = dir.to_string_lossy().into_owned();
        state.turn_id = Some("t1".into());
        state.pid = std::process::id() as i64;
        ruddr_core::state::persist_state(&state).unwrap();
        let error = send_prompt(&dir, PromptRoute::Steer, Some("t0"), "go", &[]).unwrap_err();
        assert!(error.contains("turn changed"), "{error}");
        let error = send_prompt(&dir, PromptRoute::Prompt, None, "go", &[]).unwrap_err();
        assert!(error.contains("now active"), "{error}");
        let error = stop(&dir, Status::Idle, None).unwrap_err();
        assert!(error.contains("now active"), "{error}");
        // A dead controller reads as stale: no route, nothing sent.
        state.pid = 0;
        ruddr_core::state::persist_state(&state).unwrap();
        let error = send_prompt(&dir, PromptRoute::Steer, Some("t1"), "go", &[]).unwrap_err();
        assert!(error.contains("stale"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn steer_reaches_the_controller_with_the_observed_turn() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let dir = temp("steer");
        let socket = dir.join(".ruddr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut state = crate::core::tests_support::session(Status::Active);
        state.state_dir = dir.to_string_lossy().into_owned();
        state.socket_path = socket.to_string_lossy().into_owned();
        state.turn_id = Some("turn-9".into());
        state.thread_id = Some("thread".into());
        state.pid = std::process::id() as i64;
        ruddr_core::state::persist_state(&state).unwrap();
        let reply = serde_json::json!({"ok": true, "state": state}).to_string();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            (&stream).write_all(format!("{reply}\n").as_bytes()).unwrap();
            line
        });
        assert_eq!(
            send_prompt(&dir, PromptRoute::Steer, Some("turn-9"), "go left", &[]).unwrap(),
            "Steer delivered"
        );
        let request: serde_json::Value = serde_json::from_str(server.join().unwrap().trim()).unwrap();
        assert_eq!(
            request,
            serde_json::json!({"command": "steer", "text": "go left", "expectedTurnId": "turn-9"})
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_prompt_can_wait_longer_than_the_generic_control_timeout() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let dir = temp("slow-prompt");
        let socket = dir.join(".ruddr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut state = crate::core::tests_support::session(Status::Idle);
        state.state_dir = dir.to_string_lossy().into_owned();
        state.socket_path = socket.to_string_lossy().into_owned();
        state.pid = std::process::id() as i64;
        ruddr_core::state::persist_state(&state).unwrap();
        let reply = serde_json::json!({"ok": true, "state": state}).to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            std::thread::sleep(control::DEFAULT_TIMEOUT + Duration::from_millis(100));
            stream.write_all(format!("{reply}\n").as_bytes()).unwrap();
        });
        let result = send_prompt(&dir, PromptRoute::Prompt, None, "next turn", &[]);
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(result.unwrap(), "Prompt sent");
    }

    #[test]
    fn bounded_commands_time_out() {
        let mut command = Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            command.args(["-n", "5", "127.0.0.1"]);
        } else {
            command.arg("5");
        }
        let started = Instant::now();
        assert!(
            run_bounded(command, Duration::from_millis(200), 1024)
                .unwrap_err()
                .contains("timed out")
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
