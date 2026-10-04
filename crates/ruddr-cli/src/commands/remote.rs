//! `ruddr --remote SSH_TARGET COMMAND [args]`: a thin `ssh` passthrough. It
//! renders the command for the remote POSIX shell or PowerShell, sends local
//! prompt and message files over stdin, forces `run --detach` so a run
//! outlives the connection, and passes output and the exit status through.
//! It never handles credentials. Port of remote.go.

use ruddr_core::{Error, Result};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SSH_ENV: &str = "RUDDR_SSH";
pub const REMOTE_RUDDR_ENV: &str = "RUDDR_REMOTE_RUDDR";
pub const REMOTE_SHELL_ENV: &str = "RUDDR_REMOTE_SHELL";
/// Prints `Core` or `Desktop` in PowerShell, `.PSEdition` in a POSIX shell,
/// and the text unchanged in cmd.exe.
pub const SHELL_PROBE: &str = "echo $PSVersionTable.PSEdition";
/// Non-interactive SSH shells often skip the profile that puts user-level
/// installs on PATH.
pub const PATH_PREFIX: &str = r#"PATH="$HOME/.local/bin:$HOME/.bun/bin:$PATH"; export PATH; "#;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Posix,
    PowerShell,
}

impl Shell {
    pub fn name(self) -> &'static str {
        match self {
            Shell::Posix => "posix",
            Shell::PowerShell => "powershell",
        }
    }
    fn from_name(name: &str) -> Option<Shell> {
        match name {
            "posix" => Some(Shell::Posix),
            "powershell" => Some(Shell::PowerShell),
            _ => None,
        }
    }
}

/// Splits a long flag argument into its name and inline value. Only the GNU
/// form (`--name`, `--name=value`) counts as a flag.
fn split_flag(arg: &str) -> Option<(&str, Option<&str>)> {
    let body = arg.strip_prefix("--").filter(|b| !b.is_empty())?;
    Some(match body.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (body, None),
    })
}

/// Validates the SSH target that follows `--remote`.
pub fn check_target(target: Option<&str>) -> Result<String> {
    let Some(target) = target else {
        return Err(Error::usage("--remote requires an SSH target"));
    };
    if target.is_empty() || target.starts_with('-') {
        return Err(Error::usage(format!("invalid --remote SSH target {target:?}")));
    }
    Ok(target.to_string())
}

/// Whether `args` holds `--name` before any `--`.
pub fn has_flag(args: &[String], name: &str) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| split_flag(a).is_some_and(|(n, _)| n == name))
}

/// Replaces the value of the last `--name` before any `--` and returns the
/// new arguments and the old value. The command parsers use the last value.
pub fn replace_flag_value(args: &[String], name: &str, replacement: &str) -> (Vec<String>, Option<String>) {
    let mut out = args.to_vec();
    let boundary = out.iter().position(|a| a == "--").unwrap_or(out.len());
    let Some(index) = out[..boundary]
        .iter()
        .rposition(|a| split_flag(a).is_some_and(|(flag, _)| flag == name))
    else {
        return (out, None);
    };
    if let Some((_, Some(value))) = split_flag(&args[index]) {
        out[index] = format!("--{name}={replacement}");
        return (out, Some(value.to_string()));
    }
    match out[..boundary].get_mut(index + 1) {
        Some(next) => {
            let previous = std::mem::replace(next, replacement.to_string());
            (out, Some(previous))
        }
        None => (out, None),
    }
}

/// What the remote side runs and what travels over stdin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub tty: bool,
}

/// Adapts a command for a remote ruddr: local files travel over stdin, `run`
/// starts detached so it outlives the SSH connection, and `tui` gets a
/// terminal.
pub fn plan(args: &[String], local_stdin: &mut dyn Read) -> Result<Plan> {
    let Some((command, rest)) = args.split_first() else {
        return Err(Error::usage("a command is required after --remote TARGET"));
    };
    let mut plan = Plan {
        args: args.to_vec(),
        ..Default::default()
    };
    match command.as_str() {
        "run" => {
            if !has_flag(rest, "cwd") {
                return Err(Error::usage(
                    "--cwd is required with --remote; the local directory does not exist on the remote host",
                ));
            }
            if !has_flag(rest, "prompt-file") {
                return Err(Error::usage("--prompt-file is required"));
            }
            let (mut rewritten, prompt_file) = replace_flag_value(rest, "prompt-file", "-");
            plan.stdin = Some(read_local_payload(prompt_file.as_deref(), local_stdin)?);
            if !has_flag(&rewritten, "detach") {
                rewritten.insert(0, "--detach".into());
            } else {
                // Override an explicit false value before the child command.
                let boundary = rewritten.iter().position(|a| a == "--").unwrap_or(rewritten.len());
                rewritten.insert(boundary, "--detach".into());
            }
            plan.args = std::iter::once("run".to_string()).chain(rewritten).collect();
        }
        "steer" | "prompt" if has_flag(rest, "message-file") => {
            let (rewritten, message_file) = replace_flag_value(rest, "message-file", "-");
            plan.stdin = Some(read_local_payload(message_file.as_deref(), local_stdin)?);
            plan.args = std::iter::once(command.clone()).chain(rewritten).collect();
        }
        "tui" => plan.tty = true,
        _ => {}
    }
    Ok(plan)
}

fn read_local_payload(path: Option<&str>, local_stdin: &mut dyn Read) -> Result<Vec<u8>> {
    match path {
        None | Some("") => Err(Error::usage("file flag requires a path")),
        Some("-") => {
            let mut data = Vec::new();
            local_stdin
                .read_to_end(&mut data)
                .map_err(|e| Error::failed(format!("read stdin: {e}")))?;
            Ok(data)
        }
        Some(path) => std::fs::read(path).map_err(|e| Error::failed(format!("open {path}: {e}"))),
    }
}

fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Quotes one word for a POSIX shell. A leading `~/` stays bare so the
/// remote shell expands it to the remote home.
fn posix_word(word: &str) -> String {
    match word.strip_prefix("~/") {
        Some("") => "~/".into(),
        Some(rest) => format!("~/{}", shell_quote(rest)),
        None => shell_quote(word),
    }
}

/// The command string for a remote POSIX shell.
pub fn posix_command(ruddr: &str, args: &[String]) -> String {
    let mut command = format!("{PATH_PREFIX}exec {}", posix_word(ruddr));
    for arg in args {
        command.push(' ');
        command.push_str(&posix_word(arg));
    }
    command
}

fn powershell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "''"))
}

/// Quotes one argument for PowerShell, which does not expand `~` in native
/// command arguments, so a leading `~/` or `~\` becomes `$HOME`.
fn powershell_word(word: &str) -> String {
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = word.strip_prefix(prefix) {
            if rest.is_empty() {
                return "$HOME".into();
            }
            return format!("($HOME + {})", powershell_quote(&format!("\\{rest}")));
        }
    }
    powershell_quote(word)
}

/// The command string for a remote PowerShell, the default OpenSSH shell on
/// many Windows hosts. It uses only single quotes, because Windows OpenSSH
/// does not preserve double quotes in the command string. `Stop` turns a
/// missing ruddr into a nonzero exit.
pub fn powershell_command(ruddr: &str, args: &[String]) -> String {
    let mut command = format!("$ErrorActionPreference = 'Stop'; & {}", powershell_word(ruddr));
    for arg in args {
        command.push(' ');
        command.push_str(&powershell_word(arg));
    }
    command.push_str("; exit $LASTEXITCODE");
    command
}

pub fn ssh_args(target: &str, plan: &Plan, ruddr: &str, shell: Shell) -> Vec<String> {
    let tty = if plan.tty { "-t" } else { "-T" };
    let command = match shell {
        Shell::Posix => posix_command(ruddr, &plan.args),
        Shell::PowerShell => powershell_command(ruddr, &plan.args),
    };
    vec![tty.into(), "--".into(), target.into(), command]
}

/// Maps the probe's output to a supported shell.
pub fn classify_shell(output: &str) -> Result<Shell> {
    let trimmed = output.trim();
    match trimmed {
        "Core" | "Desktop" => Ok(Shell::PowerShell),
        "" | ".PSEdition" => Ok(Shell::Posix),
        _ if trimmed == &SHELL_PROBE["echo ".len()..] => Err(Error::failed(format!(
            "the remote default shell is cmd.exe; set the OpenSSH DefaultShell to PowerShell, or set {REMOTE_SHELL_ENV}=powershell if commands run under PowerShell anyway"
        ))),
        _ => Err(Error::failed(format!(
            "cannot tell the remote shell from {trimmed:?}; set {REMOTE_SHELL_ENV} to posix or powershell"
        ))),
    }
}

/// `remote-shells.json` beside the run registry.
pub fn shell_cache_path() -> PathBuf {
    let registry = ruddr_core::paths::registry_dir();
    registry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(registry)
        .join("remote-shells.json")
}

/// The target's shell from the override, the per-target cache, or one probe
/// over ssh whose answer is cached.
pub fn resolve_shell(ssh: &str, target: &str, configured: Option<&str>, cache_path: &Path) -> Result<Shell> {
    if let Some(configured) = configured.filter(|c| !c.is_empty()) {
        return Shell::from_name(configured)
            .ok_or_else(|| Error::failed(format!("{REMOTE_SHELL_ENV} must be posix or powershell, not {configured:?}")));
    }
    let mut cache: serde_json::Map<String, serde_json::Value> = std::fs::read(cache_path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default();
    if let Some(shell) = cache.get(target).and_then(|v| v.as_str()).and_then(Shell::from_name) {
        return Ok(shell);
    }
    let output = run_probe(ssh, target).map_err(|e| e.context(format!("probe the remote shell on {target}")))?;
    let shell = classify_shell(&output)?;
    cache.insert(target.to_string(), serde_json::Value::String(shell.name().into()));
    if let Some(parent) = cache_path.parent()
        && ruddr_core::fsutil::create_private_dir(parent).is_ok()
        && let Ok(mut raw) = serde_json::to_vec_pretty(&cache)
    {
        raw.push(b'\n');
        let _ = ruddr_core::fsutil::write_private_atomic(cache_path, &raw);
    }
    Ok(shell)
}

fn run_probe(ssh: &str, target: &str) -> Result<String> {
    let mut child = Command::new(ssh)
        .args(["-T", "--", target, SHELL_PROBE])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Error::failed(e.to_string()))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::failed(format!(
                "timed out after {}",
                ruddr_core::duration::format(PROBE_TIMEOUT)
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::failed(format!("ssh {status}")));
    }
    Ok(output)
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Runs `args` on `target` and returns the remote exit status.
pub fn run(target: &str, args: &[String]) -> Result<i32> {
    let plan = plan(args, &mut std::io::stdin())?;
    if plan.tty && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err(Error::failed("the TUI requires an interactive terminal"));
    }
    let ssh = env_or(SSH_ENV, "ssh");
    let ruddr = env_or(REMOTE_RUDDR_ENV, "ruddr");
    let configured = std::env::var(REMOTE_SHELL_ENV).ok();
    let shell = resolve_shell(&ssh, target, configured.as_deref(), &shell_cache_path())?;
    let mut command = Command::new(&ssh);
    command
        .args(ssh_args(target, &plan, &ruddr, shell))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command.stdin(match (&plan.stdin, plan.tty) {
        (_, true) => Stdio::inherit(),
        (Some(_), false) => Stdio::piped(),
        (None, false) => Stdio::null(),
    });
    let _ = std::io::stdout().flush();
    let mut child = command.spawn().map_err(|e| Error::failed(format!("ssh {target}: {e}")))?;
    let writer = match (child.stdin.take(), plan.stdin) {
        (Some(mut stdin), Some(payload)) => Some(std::thread::spawn(move || {
            // A remote side that exits early closes the pipe; its status is
            // what matters, so a write error here is not reported.
            let _ = stdin.write_all(&payload);
        })),
        _ => None,
    };
    let status = child.wait().map_err(|e| Error::failed(format!("ssh {target}: {e}")))?;
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    match status.code() {
        Some(code) => Ok(code),
        None => Err(Error::failed(format!("ssh {target}: {status}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ruddr-remote-{name}-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn checks_targets() {
        assert_eq!(check_target(Some("ampere")).unwrap(), "ampere");
        assert_eq!(check_target(Some("user@host")).unwrap(), "user@host");
        assert!(check_target(None).is_err());
        assert!(check_target(Some("-oProxyCommand=evil")).is_err());
        assert!(check_target(Some("")).is_err());
    }

    #[test]
    fn run_sends_the_prompt_over_stdin_and_detaches() {
        let dir = temp_dir("plan");
        let prompt = dir.join("brief.md");
        std::fs::write(&prompt, "fix the bug\n").unwrap();
        let args = strings(&[
            "run",
            "--provider",
            "codex",
            "--cwd",
            "~/proj",
            &format!("--prompt-file={}", prompt.display()),
            "--state-dir",
            "runs/x",
            "--",
            "codex",
            "--prompt-file",
            "keep",
        ]);
        let plan = plan(&args, &mut std::io::empty()).unwrap();
        let want = strings(&[
            "run",
            "--detach",
            "--provider",
            "codex",
            "--cwd",
            "~/proj",
            "--prompt-file=-",
            "--state-dir",
            "runs/x",
            "--",
            "codex",
            "--prompt-file",
            "keep",
        ]);
        assert_eq!(plan.args, want);
        assert_eq!(plan.stdin.as_deref(), Some(&b"fix the bug\n"[..]));
        assert!(!plan.tty);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_requires_cwd_and_prompt_file() {
        let error = plan(
            &strings(&["run", "--prompt-file", "brief.md", "--state-dir", "x"]),
            &mut std::io::empty(),
        )
        .unwrap_err();
        assert!(error.message.contains("--cwd"), "{}", error.message);
        let error = plan(&strings(&["run", "--cwd", "/w"]), &mut std::io::empty()).unwrap_err();
        assert_eq!(error.exit, ruddr_core::Exit::Usage);
    }

    #[test]
    fn repeated_payload_flags_forward_the_last_value() {
        for (command, flag, required) in [
            ("run", "--prompt-file", vec!["--cwd", "/w"]),
            ("steer", "--message-file", vec!["--state-dir", "run"]),
            ("prompt", "--message-file", vec!["--state-dir", "run"]),
        ] {
            for inline in [false, true] {
                let mut args = strings(&[command]);
                args.extend(strings(&required));
                args.extend(strings(&[flag, "/missing/ignored.md"]));
                if inline {
                    args.push(format!("{flag}=-"));
                } else {
                    args.extend(strings(&[flag, "-"]));
                }
                let planned = plan(&args, &mut &b"last payload"[..]).unwrap();
                assert_eq!(planned.stdin.as_deref(), Some(&b"last payload"[..]));
                let (_, effective) = replace_flag_value(&planned.args, &flag[2..], "-");
                assert_eq!(effective.as_deref(), Some("-"));
            }
        }
    }

    #[test]
    fn remote_run_cannot_disable_detachment() {
        let planned = plan(
            &strings(&[
                "run",
                "--cwd",
                "/w",
                "--prompt-file",
                "-",
                "--detach=false",
                "--",
                "provider",
                "--detach=false",
            ]),
            &mut &b"task"[..],
        )
        .unwrap();
        let parsed = ruddr_runner::args::parse(&planned.args[1..]).unwrap();
        assert!(parsed.detach);
        assert_eq!(parsed.child_args, Some(strings(&["provider", "--detach=false"])));
    }

    #[test]
    fn forwards_message_files_and_local_stdin() {
        let plan_one = plan(
            &strings(&["steer", "--state-dir", "x", "--message-file", "-"]),
            &mut &b"new direction"[..],
        )
        .unwrap();
        assert_eq!(plan_one.args, strings(&["steer", "--state-dir", "x", "--message-file", "-"]));
        assert_eq!(plan_one.stdin.as_deref(), Some(&b"new direction"[..]));

        let plain = plan(&strings(&["prompt", "--state-dir", "x", "next", "task"]), &mut std::io::empty()).unwrap();
        assert_eq!(plain.stdin, None);
        assert!(!plain.tty);
        assert!(plan(&strings(&["tui", "--mobile"]), &mut std::io::empty()).unwrap().tty);
    }

    #[test]
    fn quotes_posix_arguments() {
        let got = posix_command("ruddr", &strings(&["steer", "--state-dir", "~/runs/it's", "a b; rm -rf /", "~/"]));
        let want = format!(r"{PATH_PREFIX}exec 'ruddr' 'steer' '--state-dir' ~/'runs/it'\''s' 'a b; rm -rf /' ~/");
        assert_eq!(got, want);
        let args = ssh_args(
            "ampere",
            &Plan {
                args: strings(&["status"]),
                ..Default::default()
            },
            "ruddr",
            Shell::Posix,
        );
        assert_eq!(args[..3], strings(&["-T", "--", "ampere"]));
        let tui = Plan {
            args: strings(&["tui"]),
            tty: true,
            ..Default::default()
        };
        assert_eq!(ssh_args("ampere", &tui, "ruddr", Shell::Posix)[0], "-t");
    }

    #[test]
    fn quotes_powershell_arguments() {
        let got = powershell_command(
            r"C:\tools\ruddr.exe",
            &strings(&["steer", "--state-dir", "~/runs/it's", r#"a "b"; $x"#, "~/"]),
        );
        let want = r#"$ErrorActionPreference = 'Stop'; & 'C:\tools\ruddr.exe' 'steer' '--state-dir' ($HOME + '\runs/it''s') 'a "b"; $x' $HOME; exit $LASTEXITCODE"#;
        assert_eq!(got, want);
        let args = ssh_args(
            "main",
            &Plan {
                args: strings(&["status"]),
                ..Default::default()
            },
            "ruddr",
            Shell::PowerShell,
        );
        assert!(args[3].starts_with("$ErrorActionPreference"));
    }

    #[test]
    fn classifies_probe_output() {
        for (output, want) in [
            ("Core\r\n", Shell::PowerShell),
            ("Desktop\n", Shell::PowerShell),
            (".PSEdition\n", Shell::Posix),
            ("", Shell::Posix),
        ] {
            assert_eq!(classify_shell(output).unwrap(), want, "{output:?}");
        }
        assert!(
            classify_shell("$PSVersionTable.PSEdition\r\n")
                .unwrap_err()
                .message
                .contains("cmd.exe")
        );
        assert!(classify_shell("fish?").is_err());
    }

    #[test]
    fn replaces_only_flags_before_the_child_command() {
        let (args, old) = replace_flag_value(&strings(&["--", "--prompt-file", "x"]), "prompt-file", "-");
        assert_eq!(old, None);
        assert_eq!(args, strings(&["--", "--prompt-file", "x"]));
        assert!(!has_flag(&strings(&["-cwd", "x"]), "cwd"), "only GNU long flags count");
        let input = strings(&["--prompt-file", "--", "provider"]);
        let (args, old) = replace_flag_value(&input, "prompt-file", "-");
        assert_eq!(old, None);
        assert_eq!(args, input);
    }

    /// The probe runs once per target; later commands read the cached answer.
    #[cfg(unix)]
    #[test]
    fn probes_once_and_caches() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("probe");
        let calls = dir.join("calls");
        let ssh = dir.join("ssh");
        std::fs::write(&ssh, format!("#!/bin/sh\necho probe >> '{}'\necho Core\n", calls.display())).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cache = dir.join("state").join("remote-shells.json");
        for _ in 0..2 {
            assert_eq!(
                resolve_shell(ssh.to_str().unwrap(), "main", None, &cache).unwrap(),
                Shell::PowerShell
            );
        }
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "probe\n");
        let cached: serde_json::Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!(cached["main"], "powershell");
        assert!(resolve_shell(ssh.to_str().unwrap(), "main", Some("fish"), &cache).is_err());
        assert_eq!(
            resolve_shell(ssh.to_str().unwrap(), "other", Some("posix"), &cache).unwrap(),
            Shell::Posix
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "probe\n", "an override skips the probe");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
