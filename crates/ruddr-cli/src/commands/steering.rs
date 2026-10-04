//! `steer`, `prompt`, `stop`, and `interrupt`: the commands that reach a
//! live controller over `ruddr_core::control`. A run whose controller died
//! fails promptly with exit code 4.

use super::args;
use super::runs::{self, Alive, RunRef, Selection, process_alive, read_view};
use ruddr_core::control::{self, Command, Request};
use ruddr_core::state::{RunState, Status, read_state};
use ruddr_core::{Error, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

const STEER_TIMEOUT: Duration = Duration::from_secs(30);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// The controller may take up to 30 seconds to settle an interrupt.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(35);

/// Reads the message from `--message-file` (`-` reads stdin, which is how
/// `--remote` forwards a local file) or from the positional arguments.
fn read_message(parsed: &args::Parsed, stdin: &mut dyn Read) -> Result<String> {
    let Some(file) = parsed.string("message-file") else {
        return Ok(parsed.positionals.join(" ").trim().to_string());
    };
    let mut raw = Vec::new();
    if file == "-" {
        stdin.read_to_end(&mut raw).map_err(|e| Error::failed(format!("read stdin: {e}")))?;
    } else {
        raw = std::fs::read(&file).map_err(|e| Error::failed(format!("open {file}: {e}")))?;
    }
    Ok(String::from_utf8_lossy(&raw).trim().to_string())
}

/// The `--image` files, checked and made absolute before any request goes out.
fn attached_images(parsed: &args::Parsed) -> Result<Vec<String>> {
    ruddr_core::images::checked_images(&parsed.all("image")).map_err(Error::usage)
}

fn state_dir_flag(parsed: &args::Parsed) -> Result<String> {
    parsed
        .string("state-dir")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| Error::usage("--state-dir is required"))
}

/// The displayed state, or a stale error when the controller is gone.
fn live_state(state_dir: &Path) -> Result<RunState> {
    let state = read_state(state_dir)?.displayed();
    if state.status == Status::Stale {
        return Err(Error::stale(state.error.unwrap_or_else(|| "persisted state is stale".into())));
    }
    Ok(state)
}

fn turn_of(state: &RunState) -> &str {
    state.turn_id.as_deref().unwrap_or("")
}

pub fn steer(out: &mut dyn Write, argv: Vec<String>, stdin: &mut dyn Read) -> Result<()> {
    let specs = [
        args::value("state-dir", "DIR", "Ruddr run state directory"),
        args::value("message-file", "FILE", "read steering text from this file; - reads stdin"),
        args::multi("image", "FILE", "attach a png, jpg, gif, or webp image (repeatable)"),
        args::value("expected-turn-id", "ID", "reject the steer if the active turn changed"),
        args::value("timeout", "DURATION", "control request timeout (default 30s)"),
    ];
    let parsed = args::parse("steer", &specs, &argv)?;
    let timeout = parsed.duration("timeout", STEER_TIMEOUT)?;
    let state_dir = state_dir_flag(&parsed)?;
    let message = read_message(&parsed, stdin)?;
    if message.is_empty() {
        return Err(Error::usage("steering text is required"));
    }
    let images = attached_images(&parsed)?;
    let state = live_state(Path::new(&state_dir))?;
    if state.status != Status::Active {
        return Err(Error::failed(format!("turn is not steerable: status={}", state.status)));
    }
    let expected = parsed.string("expected-turn-id").filter(|id| !id.is_empty());
    if let Some(expected) = &expected
        && turn_of(&state) != expected
    {
        return Err(Error::failed(format!(
            "active turn changed from {expected} to {}; steer was not sent",
            turn_of(&state)
        )));
    }
    // The controller requires the turn the steer is meant for, so a steer
    // never lands on a turn that started after this check.
    let expected = expected.unwrap_or_else(|| turn_of(&state).to_string());
    let request = Request {
        command: Command::Steer,
        images,
        text: Some(message),
        expected_turn_id: Some(expected),
    };
    let reply = control::call(Path::new(&state_dir), &request, timeout)?;
    writeln!(out, "steered turn {}", turn_of(&reply))?;
    Ok(())
}

pub fn prompt(out: &mut dyn Write, argv: Vec<String>, stdin: &mut dyn Read) -> Result<()> {
    let specs = [
        args::value("state-dir", "DIR", "Ruddr run state directory"),
        args::value("message-file", "FILE", "read prompt text from this file; - reads stdin"),
        args::multi("image", "FILE", "attach a png, jpg, gif, or webp image (repeatable)"),
        args::value("timeout", "DURATION", "control request timeout (default 1m0s)"),
    ];
    let parsed = args::parse("prompt", &specs, &argv)?;
    let timeout = parsed.duration("timeout", PROMPT_TIMEOUT)?;
    let state_dir = state_dir_flag(&parsed)?;
    let message = read_message(&parsed, stdin)?;
    if message.is_empty() {
        return Err(Error::usage("prompt text is required"));
    }
    let images = attached_images(&parsed)?;
    let state = live_state(Path::new(&state_dir))?;
    match state.status {
        Status::Idle => {}
        Status::Active => return Err(Error::failed("a turn is active; use steer")),
        other => return Err(Error::failed(format!("session is not idle: status={other}"))),
    }
    let request = Request {
        command: Command::Prompt,
        images,
        text: Some(message),
        expected_turn_id: None,
    };
    let reply = control::call(Path::new(&state_dir), &request, timeout)?;
    writeln!(out, "started turn {}", turn_of(&reply))?;
    Ok(())
}

pub fn stop(out: &mut dyn Write, argv: Vec<String>) -> Result<()> {
    let mut specs = runs::SELECTION_SPECS.to_vec();
    specs.push(args::value("timeout", "DURATION", "control request timeout (default 30s)"));
    let parsed = args::parse("stop", &specs, &argv)?;
    args::no_positionals("stop", &parsed)?;
    let timeout = parsed.duration("timeout", STOP_TIMEOUT)?;
    let selection = Selection::from_parsed(&parsed)?;
    let Some(single) = selection.single() else {
        let refs = selection.resolve()?;
        return broadcast(out, &refs, Action::Stop, timeout, &process_alive);
    };
    let state_dir = single.ok_or_else(|| Error::usage("--state-dir is required"))?;
    let request = Request {
        command: Command::Stop,
        images: vec![],
        text: None,
        expected_turn_id: None,
    };
    control::call(Path::new(state_dir), &request, timeout)?;
    writeln!(out, "shutdown requested")?;
    Ok(())
}

pub fn interrupt(out: &mut dyn Write, argv: Vec<String>) -> Result<()> {
    let mut specs = runs::SELECTION_SPECS.to_vec();
    specs.push(args::value(
        "expected-turn-id",
        "ID",
        "reject the interrupt if the active turn changed",
    ));
    specs.push(args::value("timeout", "DURATION", "control request timeout (default 35s)"));
    let parsed = args::parse("interrupt", &specs, &argv)?;
    args::no_positionals("interrupt", &parsed)?;
    let timeout = parsed.duration("timeout", INTERRUPT_TIMEOUT)?;
    let selection = Selection::from_parsed(&parsed)?;
    let expected = parsed.string("expected-turn-id").filter(|id| !id.is_empty());
    let Some(single) = selection.single() else {
        if expected.is_some() {
            return Err(Error::usage(
                "--expected-turn-id applies to one run; drop it or name a single --state-dir",
            ));
        }
        let refs = selection.resolve()?;
        return broadcast(out, &refs, Action::Interrupt, timeout, &process_alive);
    };
    let state_dir = single.ok_or_else(|| Error::usage("--state-dir is required"))?;
    let state = live_state(Path::new(state_dir))?;
    if state.status != Status::Active {
        return Err(Error::failed(format!("turn is not active: status={}", state.status)));
    }
    let expected = expected.unwrap_or_else(|| turn_of(&state).to_string());
    let request = Request {
        command: Command::Interrupt,
        images: vec![],
        text: None,
        expected_turn_id: Some(expected.clone()),
    };
    control::call(Path::new(state_dir), &request, timeout)?;
    writeln!(out, "interrupt requested for turn {expected}")?;
    Ok(())
}

/// A control command sent to a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Stop,
    Interrupt,
}

impl Action {
    /// The word printed per run; `shutdown` matches what earlier releases printed.
    fn name(self) -> &'static str {
        match self {
            Action::Stop => "shutdown",
            Action::Interrupt => "interrupt",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Action::Stop => "stop",
            Action::Interrupt => "interrupt",
        }
    }
    fn wanted(self) -> Status {
        match self {
            Action::Stop => Status::Idle,
            Action::Interrupt => Status::Active,
        }
    }
}

/// Sends one control command to every run whose status allows it and reports
/// the others as skipped. Fails if any send failed.
pub fn broadcast(out: &mut dyn Write, refs: &[RunRef], action: Action, timeout: Duration, alive: Alive) -> Result<()> {
    let wanted = action.wanted();
    let (mut acted, mut failed) = (0, 0);
    for run in refs {
        let view = read_view(run, alive);
        let Some(state) = view.state().filter(|s| s.status == wanted) else {
            writeln!(out, "{}: skipped (status={})", run.name, view.status())?;
            continue;
        };
        let request = match action {
            Action::Stop => Request {
                command: Command::Stop,
                images: vec![],
                text: None,
                expected_turn_id: None,
            },
            Action::Interrupt => Request {
                command: Command::Interrupt,
                images: vec![],
                text: None,
                expected_turn_id: Some(turn_of(state).to_string()),
            },
        };
        match control::call(&run.state_dir, &request, timeout) {
            Ok(_) => {
                writeln!(out, "{}: {} requested", run.name, action.name())?;
                acted += 1;
            }
            Err(error) => {
                writeln!(out, "{}: failed: {error}", run.name)?;
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(Error::failed(format!(
            "{} failed for {failed} of {} runs",
            action.name(),
            refs.len()
        )));
    }
    if acted == 0 {
        writeln!(out, "no {wanted} runs to {}", action.verb())?;
    }
    Ok(())
}
