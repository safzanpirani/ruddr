//! `state.json`: the redacted, machine-readable record of one run. It holds
//! IDs, paths, lifecycle metadata, counts, timestamps, the controller PID, and
//! generic errors only. Prompt, completion, and tool text belong in the
//! private transcript files (`events.jsonl`, `trace.log`, `output.md`).
//!
//! Field names match what Go releases wrote, so older runs stay readable and
//! the web client and skills keep working.

use crate::error::{Context, Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Go releases since provider support wrote version 2.
pub const STATE_VERSION: u32 = 2;
pub const STATE_FILE: &str = "state.json";
pub const EVENTS_FILE: &str = "events.jsonl";
pub const TRACE_FILE: &str = "trace.log";
pub const OUTPUT_FILE: &str = "output.md";
pub const STDERR_FILE: &str = "provider.stderr.log";
pub const CLAIM_FILE: &str = ".ruddr.claim";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Starting,
    Active,
    Idle,
    Completed,
    Failed,
    Interrupted,
    /// Never persisted: shown when a non-terminal run's controller is gone.
    Stale,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Completed | Status::Failed | Status::Interrupted)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Active => "active",
            Status::Idle => "idle",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Interrupted => "interrupted",
            Status::Stale => "stale",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cumulative counters plus the latest context estimate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub input_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_input_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: i64,
    #[serde(default, skip_serializing_if = "is_zero_f64", rename = "costUsd")]
    pub cost_usd: f64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}
fn is_zero_f64(value: &f64) -> bool {
    *value == 0.0
}
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunState {
    pub version: u32,
    #[serde(default = "default_provider")]
    pub provider: String,
    pub pid: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub child_pid: i64,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub sandbox: String,
    pub state_dir: String,
    /// Unix socket path, or the Windows named-pipe name.
    #[serde(default)]
    pub socket_path: String,
    /// A private temporary parent created when the state directory's path was
    /// too long for a Unix socket; removed at shutdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_dir: Option<String>,
    #[serde(default)]
    pub events_path: String,
    #[serde(default)]
    pub trace_path: String,
    #[serde(default)]
    pub output_path: String,
    #[serde(default)]
    pub stderr_path: String,
    #[serde(default)]
    pub steers: u32,
    /// Started with `--idle`; `Status::Idle` then means "ready for a prompt".
    #[serde(default, skip_serializing_if = "is_false")]
    pub idle: bool,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub turns: u32,
    /// How the latest turn ended; survives the return to idle.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "lastTurnStatus")]
    pub last_turn: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    pub started_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "nonzero_time")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn default_provider() -> String {
    "codex".into()
}

/// Go wrote its zero time (`0001-01-01T00:00:00Z`) for "not completed".
fn nonzero_time<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Option<String>, D::Error> {
    let value: Option<String> = Option::deserialize(deserializer)?;
    Ok(value.filter(|v| crate::time::parse_rfc3339_ms(v).is_some()))
}

impl RunState {
    pub fn state_path(&self) -> PathBuf {
        Path::new(&self.state_dir).join(STATE_FILE)
    }

    /// The status to show: a non-terminal run whose controller is gone is
    /// `stale`. Wait and control commands must fail promptly on it.
    pub fn displayed(mut self) -> RunState {
        if !self.status.is_terminal() && !crate::process::alive(self.pid) {
            self.error = Some(format!("Ruddr pid {} is not running; persisted state is stale", self.pid));
            self.status = Status::Stale;
        }
        self
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// Reads `<state_dir>/state.json`. The recorded `stateDir` must name the same
/// directory; a copied or forged state file is refused.
pub fn read_state(state_dir: &Path) -> Result<RunState> {
    let path = state_dir.join(STATE_FILE);
    let bytes = std::fs::read(&path).context(format!("read {}", path.display()))?;
    let state: RunState = serde_json::from_slice(&bytes).context(format!("parse {}", path.display()))?;
    // Compare resolved paths: /tmp and /private/tmp name the same directory.
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| crate::paths::absolute(p));
    if resolve(Path::new(&state.state_dir)) != resolve(state_dir) {
        return Err(Error::failed(format!(
            "{} records stateDir {}, not this directory",
            path.display(),
            state.state_dir
        )));
    }
    Ok(state)
}

/// Writes `state.json` atomically as 0600.
pub fn persist_state(state: &RunState) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(state)?;
    data.push(b'\n');
    crate::fsutil::write_private_atomic(&state.state_path(), &data).context("persist state")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO_STATE: &str = r#"{
  "version": 1, "provider": "droid", "pid": 1, "status": "completed",
  "threadId": "t", "model": "glm-5.3-flash", "cwd": "/w", "sandbox": "workspace-write",
  "stateDir": "/w/.scratch/run", "socketPath": "/w/.scratch/run/.ruddr.sock",
  "eventsPath": "e", "tracePath": "t", "outputPath": "o", "stderrPath": "s", "steers": 1,
  "idle": true, "turns": 4, "lastTurnStatus": "interrupted",
  "tokenUsage": {"totalTokens": 257000, "contextTokens": 0, "contextWindow": 200000, "costUsd": 0.01},
  "startedAt": "2026-10-02T09:17:13.12Z", "updatedAt": "2026-10-02T09:19:44Z",
  "completedAt": "0001-01-01T00:00:00Z"
}"#;

    #[test]
    fn reads_go_written_state() {
        let state: RunState = serde_json::from_str(GO_STATE).unwrap();
        assert_eq!(state.status, Status::Completed);
        assert_eq!(state.last_turn, Some(Status::Interrupted));
        assert_eq!(state.completed_at, None, "Go's zero time means not completed");
        assert_eq!(state.token_usage.as_ref().unwrap().context_tokens, Some(0));
        let round: serde_json::Value = serde_json::to_value(&state).unwrap();
        assert_eq!(round["lastTurnStatus"], "interrupted");
        assert_eq!(round["tokenUsage"]["costUsd"], 0.01);
        assert!(round.get("completedAt").is_none());
    }

    #[test]
    fn dead_controller_reads_as_stale() {
        let mut state: RunState = serde_json::from_str(GO_STATE).unwrap();
        state.status = Status::Active;
        state.pid = 0;
        let shown = state.displayed();
        assert_eq!(shown.status, Status::Stale);
        assert!(shown.error.unwrap().contains("stale"));
    }
}
