//! Every agent's past sessions, read-only, beside Ruddr's own runs. The
//! session list shows them as completed runs whose state directory is
//! `history:<locator>`. A selected session is loaded once and converted to
//! the app-server event lines the chat view already reads, and the diff tab
//! shows the session's own edits instead of `git diff`.

use crate::core::Session;
use ruddr_history::{ChangeKind, Event, SessionInfo};
use serde_json::{Value, json};

pub const PREFIX: &str = "history:";

/// How many sessions the list loads across all providers.
pub const LIMIT: usize = 400;

pub fn is_history(state_dir: &str) -> bool {
    state_dir.starts_with(PREFIX)
}

/// A history session as a run the list can show.
pub fn run_state(info: &SessionInfo) -> Session {
    let updated = std::time::UNIX_EPOCH + std::time::Duration::from_millis(info.updated_ms.max(0) as u64);
    let updated = ruddr_core::time::format_rfc3339(updated);
    serde_json::from_value(json!({
        "version": 1,
        "provider": info.provider.name(),
        "pid": 0,
        "status": "completed",
        "threadId": info.id,
        "cwd": info.cwd,
        "stateDir": format!("{PREFIX}{}", info.locator),
        "startedAt": updated,
        "updatedAt": updated,
    }))
    .expect("a history run state is valid")
}

/// Shell tools render as commands; the rest as named tool calls.
fn is_shell(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bash" | "shell" | "exec_command" | "execute" | "run_shell_command" | "terminal" | "commandexecution"
    )
}

fn command_of(input: &Value) -> Option<String> {
    match input.get("command").or_else(|| input.get("cmd"))? {
        Value::String(command) => Some(command.clone()),
        Value::Array(argv) => Some(argv.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

fn item(method: &str, item: Value) -> String {
    json!({"method": method, "params": {"item": item}}).to_string()
}

fn tool_item(id: &str, name: &str, input: &Value, output: Option<&str>, failed: bool) -> Value {
    let status = if failed { "failed" } else { "completed" };
    match command_of(input).filter(|_| is_shell(name)) {
        Some(command) => json!({
            "type": "commandExecution", "id": id, "command": command, "status": status,
            "aggregatedOutput": output, "exitCode": if failed { 1 } else { 0 },
        }),
        None => json!({
            "type": "toolCall", "id": id, "toolName": name, "input": input, "status": status,
            "aggregatedOutput": output,
        }),
    }
}

/// The session as app-server notifications, in order. A tool call starts
/// where it was made and completes in place when its result arrives.
pub fn chat_lines(events: &[Event]) -> Vec<String> {
    let mut lines = Vec::new();
    // Call ID -> (line ID, name, input) for calls still waiting on a result.
    let mut open: Vec<(Option<String>, String, String, Value)> = Vec::new();
    let mut previous_was_call = false;
    for (index, event) in events.iter().enumerate() {
        let id = format!("h{index}");
        let was_call = previous_was_call;
        previous_was_call = false;
        match event {
            Event::User { text } => lines.push(item(
                "item/completed",
                json!({"type": "userMessage", "id": id, "content": [{"type": "text", "text": text}]}),
            )),
            Event::Assistant { text } => lines.push(item("item/completed", json!({"type": "agentMessage", "id": id, "text": text}))),
            Event::Thinking { text } => lines.push(item("item/completed", json!({"type": "reasoning", "id": id, "summary": [text]}))),
            Event::ToolCall { name, input, call_id } => {
                previous_was_call = true;
                let mut started = tool_item(&id, name, input, None, false);
                started["status"] = json!("inProgress");
                lines.push(item("item/started", started));
                open.push((call_id.clone(), id, name.clone(), input.clone()));
            }
            Event::ToolResult {
                call_id, output, is_error, ..
            } => {
                let position = match call_id {
                    Some(call) => open.iter().position(|o| o.0.as_deref() == Some(call)),
                    None => (!open.is_empty()).then(|| open.len() - 1),
                };
                if let Some(position) = position {
                    let (_, line_id, name, input) = open.remove(position);
                    lines.push(item("item/completed", tool_item(&line_id, &name, &input, Some(output), *is_error)));
                }
            }
            // An edit tool's change already shows as its call.
            Event::FileChange { path, kind, .. } if !was_call => {
                let verb = match kind {
                    ChangeKind::Add => "add",
                    ChangeKind::Update => "edit",
                    ChangeKind::Delete => "delete",
                };
                lines.push(item(
                    "item/completed",
                    json!({"type": "fileChange", "id": id, "toolName": format!("{verb} {path}"), "status": "completed"}),
                ));
            }
            Event::FileChange { .. } => {}
        }
    }
    // Calls the transcript never answered ended with the session.
    for (_, line_id, name, input) in open {
        lines.push(item("item/completed", tool_item(&line_id, &name, &input, None, false)));
    }
    lines
}

/// The output tab: every assistant message, in order, as Markdown.
pub fn output_lines(events: &[Event]) -> Vec<String> {
    let mut lines = Vec::new();
    for event in events {
        if let Event::Assistant { text } = event {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.extend(text.lines().map(str::to_string));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use ruddr_history::Provider;
    use std::time::Instant;

    #[test]
    fn a_history_session_renders_through_the_chat_transcript() {
        let events = vec![
            Event::User {
                text: "fix the bug".into(),
            },
            Event::Thinking { text: "look first".into() },
            Event::ToolCall {
                name: "Bash".into(),
                input: json!({"command": "cargo test"}),
                call_id: Some("a".into()),
            },
            Event::ToolCall {
                name: "Edit".into(),
                input: json!({"file_path": "/w/a.rs"}),
                call_id: Some("b".into()),
            },
            Event::FileChange {
                path: "/w/a.rs".into(),
                kind: ChangeKind::Update,
                hunks: "@@ -1 +1 @@\n-a\n+b\n".into(),
            },
            Event::ToolResult {
                name: None,
                call_id: Some("a".into()),
                output: "1 failed".into(),
                is_error: true,
            },
            Event::ToolResult {
                name: None,
                call_id: Some("b".into()),
                output: "ok".into(),
                is_error: false,
            },
            Event::FileChange {
                path: "/w/b.rs".into(),
                kind: ChangeKind::Add,
                hunks: String::new(),
            },
            Event::Assistant { text: "fixed".into() },
        ];
        let mut transcript = Transcript::new(None);
        let now = Instant::now();
        for line in chat_lines(&events) {
            transcript.apply_line(&line, now);
        }
        transcript.commit_all();
        let shown: Vec<String> = transcript
            .entries()
            .iter()
            .map(|e| format!("{:?} {} {:?}", e.kind, e.text, e.status))
            .collect();
        assert_eq!(
            shown,
            [
                "User fix the bug None",
                "Thought look first None",
                "Tool cargo test Some(Failed)",
                "Tool Edit Some(Completed)",
                "Tool add /w/b.rs Some(Completed)",
                "Agent fixed None",
            ],
        );
    }

    #[test]
    fn history_runs_are_completed_and_keyed_by_locator() {
        let info = SessionInfo {
            provider: Provider::Droid,
            locator: "/h/.factory/sessions/-w/s.jsonl".into(),
            id: "s".into(),
            cwd: "/w".into(),
            title: "add grape".into(),
            updated_ms: 1_790_000_000_000,
        };
        let run = run_state(&info);
        assert!(is_history(&run.state_dir));
        assert_eq!(run.state_dir, "history:/h/.factory/sessions/-w/s.jsonl");
        assert_eq!((run.provider.as_str(), run.status), ("droid", ruddr_core::state::Status::Completed));
        assert_eq!(ruddr_core::time::parse_rfc3339_ms(&run.updated_at), Some(1_790_000_000_000));
    }
}
