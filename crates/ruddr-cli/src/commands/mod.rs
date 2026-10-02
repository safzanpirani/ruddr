//! Every command except run, app-server, tui, and web. Port of main.go,
//! group.go, result.go, thread_commands.go, skill.go, update.go, remote.go,
//! and the `models` command.

pub mod args;
pub mod models;
pub mod remote;
pub mod result;
pub mod runs;
pub mod skill;
pub mod steering;
pub mod thread;
pub mod update;

#[cfg(test)]
mod tests;

use ruddr_core::{Error, Result};

pub fn dispatch(command: &str, args: Vec<String>) -> Result<()> {
    let stdout = std::io::stdout();
    match command {
        "status" => runs::status(&mut stdout.lock(), args),
        "peek" => runs::peek(&mut stdout.lock(), args),
        "wait" => runs::wait(&mut stdout.lock(), args),
        "result" => result::run(&mut stdout.lock(), args, &runs::process_alive),
        "steer" => steering::steer(&mut stdout.lock(), args, &mut std::io::stdin()),
        "prompt" => steering::prompt(&mut stdout.lock(), args, &mut std::io::stdin()),
        "stop" => steering::stop(&mut stdout.lock(), args),
        "interrupt" => steering::interrupt(&mut stdout.lock(), args),
        "thread" => thread::thread_command(args),
        "models" => models::models_command(args),
        "skill" => skill::skill_command(args),
        "update" => update::update_command(args),
        other => {
            if let Some(target) = other.strip_prefix("--remote=") {
                return remote_with_target(Some(target), args);
            }
            print_usage();
            Err(Error::usage(format!("unknown command {other:?}")))
        }
    }
}

/// `ruddr --remote SSH_TARGET COMMAND [args]`; `args` starts with the target.
pub fn remote(mut args: Vec<String>) -> Result<()> {
    let target = (!args.is_empty()).then(|| args.remove(0));
    remote_with_target(target.as_deref(), args)
}

fn remote_with_target(target: Option<&str>, args: Vec<String>) -> Result<()> {
    let target = remote::check_target(target)?;
    let code = remote::run(&target, &args)?;
    if code != 0 {
        // The remote ruddr already printed its own error; pass its status on.
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
    Ok(())
}

pub fn version(args: Vec<String>) -> Result<()> {
    update::version_command(args)
}

pub fn print_usage() {
    let name = std::env::args()
        .next()
        .and_then(|arg0| std::path::Path::new(&arg0).file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "ruddr".into());
    eprint!("{}", usage_text(&name));
}

pub fn usage_text(name: &str) -> String {
    format!(
        "Ruddr {version} - live steering for coding agents

Usage:
  {name} run [--provider codex|claude|opencode|pi|droid] --prompt-file FILE [--state-dir DIR] [options]
         [-- APP_SERVER_COMMAND...]
  {name} thread list|search|read|turns|fork|name|archive|unarchive [--provider NAME] [options]
  {name} tui [--root DIR] [--state-dir DIR] [--all] [--theme NAME]
  {name} web [--host ADDR] [--port 4519] [--root DIR] [--open]  (browser dashboard)
  {name} steer --state-dir DIR \"new direction\"
  {name} prompt --state-dir DIR \"next task\"      (idle sessions started with --idle)
  {name} stop RUNS                               (gracefully end idle sessions)
  {name} models [--json]                         (list; add|default|remove PROVIDER ID edit it)
  {name} status RUNS [--json]
  {name} peek RUNS [-n 25]
  {name} interrupt RUNS [--expected-turn-id ID]
  {name} wait RUNS [--timeout 10m] [--any] [--turn]
  {name} result RUNS [--json]                     (print each run's final answer)
  {name} update [--check]                        (install the latest release)
  {name} skill install [--dir DIR]               (install the ruddr-delegate agent skill)
  {name} version
  {name} --remote SSH_TARGET COMMAND [args]      (run any command on another machine)

Flags are GNU style: --flag value or --flag=value. Every command prints its
flags with --help.

run --detach starts the controller in the background and returns once it is
running. --prompt-file - and --message-file - read the text from stdin.

RUNS is --state-dir DIR, repeatable, and/or --root DIR, which selects every run
below DIR. With several runs, status prints a table (a JSON array with --json),
peek prints the last 5 trace lines of each, wait returns when all finish and
fails unless all completed, stop ends the idle ones, and interrupt stops the
active turns. wait --any returns when the next still-running run finishes, so
repeated calls hand back runs one at a time. wait --turn also counts an idle
session as done and judges it by its last turn. result prints the last agent
message of each run's latest turn and fails for runs that did not complete.

thread prints the app-server's raw result as JSON. It asks codex app-server by
default; --provider names another provider's adapter, and a command after --
replaces the Codex app-server.

--remote runs ruddr on SSH_TARGET through ssh and passes output and exit status
through. Paths are remote paths, and POSIX and PowerShell remote shells both
work. Local --prompt-file and --message-file contents travel over stdin, run
always starts detached and needs --cwd, and tui gets a terminal. Set
RUDDR_REMOTE_RUDDR to the remote ruddr path when it is not on the remote PATH;
RUDDR_REMOTE_SHELL=posix|powershell skips the shell probe; RUDDR_SSH overrides
the ssh executable.

run without --state-dir uses CWD/.scratch/ruddr/<time>-<id>, which ignores
itself in Git, and prints the path. run --config KEY=VALUE (repeatable) passes
a Codex config override to the default codex app-server command.

models add|default|remove edit ~/.config/ruddr/models.json, which adds models,
changes provider defaults, or hides built-in models. models add codex ID
--config KEY=VALUE stores an override that every run on that model applies;
--unset-config KEY removes it. models path prints the file's location.

Exit codes: 0 success, 1 a run failed or another error, 2 bad usage, 3 still
running (wait timed out, or result on an unfinished run), 4 a controller died
and left stale state.

Ruddr checks GitHub for a newer release at most once a day and mentions it in
the TUI and after version; set RUDDR_NO_UPDATE_CHECK=1 to disable the check.
",
        version = ruddr_core::VERSION
    )
}

/// Before the TUI or web dashboard starts: expose the last check's newer
/// release as `RUDDR_UPDATE_AVAILABLE`, and refresh a day-old check in the
/// background so the next launch shows it.
pub fn prepare_dashboard() {
    let path = update::cache_path();
    let disabled = update::checks_disabled();
    if let Some(latest) = update::available_update(&path, disabled) {
        // SAFETY: called on the main thread before any other thread starts.
        unsafe { std::env::set_var("RUDDR_UPDATE_AVAILABLE", latest) };
    }
    std::thread::spawn(move || update::refresh_check(&update::Ureq, &path, disabled));
}
