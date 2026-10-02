//! `ruddr result`: the last agent message of each run's latest turn, read
//! from `events.jsonl`. Port of result.go.

use super::args;
use super::runs::{self, Alive, RunRef, Selection, View, read_view};
use ruddr_core::{Error, Exit, Result};
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// One run's entry in `result --json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub name: String,
    pub state_dir: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_turn_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

pub fn run(out: &mut dyn Write, argv: Vec<String>, alive: Alive) -> Result<()> {
    let mut specs = runs::SELECTION_SPECS.to_vec();
    specs.push(args::flag("json", "print a JSON array of results"));
    let parsed = args::parse("result", &specs, &argv)?;
    args::no_positionals("result", &parsed)?;
    let selection = Selection::from_parsed(&parsed)?;
    let single = selection.single();
    let refs = match single {
        Some(Some(dir)) => vec![RunRef {
            name: dir.to_string(),
            state_dir: PathBuf::from(dir),
        }],
        Some(None) => return Err(Error::usage("--state-dir is required")),
        None => selection.resolve()?,
    };
    let results: Vec<RunResult> = refs.iter().map(|run| collect_result(run, alive)).collect();
    let (mut failed, mut running, mut stale) = (0, 0, 0);
    for result in &results {
        if result.error.is_empty() {
            continue;
        }
        failed += 1;
        match result.status.as_str() {
            "active" | "starting" | "stopping" => running += 1,
            "stale" | "unreadable" => stale += 1,
            _ => {}
        }
    }
    // A run still going outranks a dead one, which outranks a failed one.
    let code = if running > 0 {
        Exit::Running
    } else if stale > 0 {
        Exit::Stale
    } else {
        Exit::Failed
    };
    if parsed.bool("json") {
        runs::print_json(out, &results)?;
    } else if single.is_some() {
        if results[0].error.is_empty() {
            writeln!(out, "{}", results[0].message)?;
        }
    } else {
        print_results(out, &results)?;
    }
    if single.is_some() && !results[0].error.is_empty() {
        return Err(Error::new(code, results[0].error.clone()));
    }
    if failed > 0 {
        return Err(Error::new(code, format!("{failed} of {} runs did not complete", results.len())));
    }
    Ok(())
}

/// A run's final answer, or why it has none.
pub fn collect_result(run: &RunRef, alive: Alive) -> RunResult {
    let view = read_view(run, alive);
    let mut result = RunResult {
        name: run.name.clone(),
        state_dir: run.state_dir.display().to_string(),
        status: view.status().to_string(),
        last_turn_status: view.state().and_then(|s| s.last_turn).map(|s| s.to_string()).unwrap_or_default(),
        ..Default::default()
    };
    if matches!(result.status.as_str(), "active" | "starting" | "stopping") {
        result.error = format!("run is still {}", result.status);
        return result;
    }
    if !view.succeeded() {
        result.error = view.row_error();
        if result.error.is_empty() {
            result.error = format!("run ended with status {}", result.status);
        }
        return result;
    }
    let events = match &view {
        View::Run(state) => state.events_path.clone(),
        View::Unreadable { .. } => String::new(),
    };
    match last_agent_message(&events) {
        Err(error) => result.error = error.message,
        Ok(message) if message.is_empty() => result.error = "the latest turn produced no agent message".into(),
        Ok(message) => result.message = message,
    }
    result
}

/// Scans `events.jsonl` for the last completed `agentMessage` after the
/// latest `turn/started`. Earlier turns' answers are not reported.
pub fn last_agent_message(events_path: &str) -> Result<String> {
    if events_path.is_empty() {
        return Err(Error::failed("state has no events path"));
    }
    let path = Path::new(events_path);
    let file = std::fs::File::open(path).map_err(|e| Error::failed(format!("open {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut message = String::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(message);
        }
        if !contains(&line, b"\"turn/started\"") && !contains(&line, b"\"agentMessage\"") {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        let method = event.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let item = event.pointer("/params/item");
        let text = item.and_then(|i| i.get("text")).and_then(|t| t.as_str()).unwrap_or("");
        let kind = item.and_then(|i| i.get("type")).and_then(|t| t.as_str()).unwrap_or("");
        if method == "turn/started" {
            message.clear();
        } else if method == "item/completed" && kind == "agentMessage" && !text.is_empty() {
            message = text.to_string();
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn print_results(out: &mut dyn Write, results: &[RunResult]) -> Result<()> {
    for (i, result) in results.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }
        writeln!(out, "== {}: {} ==", result.name, result.status)?;
        if !result.error.is_empty() {
            writeln!(out, "error: {}", result.error)?;
            continue;
        }
        writeln!(out, "{}", result.message)?;
    }
    Ok(())
}
