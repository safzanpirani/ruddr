//! Conversions between transcripts and app-server event lines.
//!
//! File edits come from a Ruddr run's own `events.jsonl`, for working
//! directories that `git diff` cannot describe. Codex reports `fileChange`
//! items with a `changes` list of `{path, kind: {type}, diff}`. Ruddr's
//! adapters report edit tools as items carrying the provider's `toolName`
//! and `input`.
//!
//! The other way, a past session's events become the app-server
//! notifications the TUI and the web dashboard's chat views already read.

use crate::{ChangeKind, Event, SessionInfo, Transcript, diff, edit_from_tool, str_field};
use serde_json::{Value, json};

/// Every completed, successful file edit in an app-server event log, in order.
pub fn edits(events_jsonl: &str) -> Vec<Event> {
    let mut edits = Vec::new();
    for line in events_jsonl.lines() {
        // Cheap filter before parsing: most lines are deltas and token counts.
        if !line.contains("item/completed") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
        if str_field(&event, "method") != Some("item/completed") {
            continue;
        }
        let item = event.pointer("/params/item").unwrap_or(&Value::Null);
        if matches!(str_field(item, "status"), Some("failed" | "declined")) {
            continue;
        }
        if let Some(changes) = item.get("changes").and_then(Value::as_array) {
            edits.extend(changes.iter().filter_map(codex_change));
        } else if let (Some(name), Some(input)) = (str_field(item, "toolName"), item.get("input"))
            && let Some(edit) = edit_from_tool(name, input)
        {
            edits.push(edit);
        }
    }
    edits
}

/// One Codex `FileUpdateChange`. An update's `diff` is a unified diff; an
/// add or delete may carry the file's content instead.
fn codex_change(change: &Value) -> Option<Event> {
    let path = str_field(change, "path")?.to_string();
    let kind = match change.pointer("/kind/type").and_then(Value::as_str) {
        Some("add") => ChangeKind::Add,
        Some("delete") => ChangeKind::Delete,
        _ => ChangeKind::Update,
    };
    let text = str_field(change, "diff").unwrap_or("");
    let hunks = match text.find("\n@@").map(|i| i + 1).or_else(|| text.starts_with("@@").then_some(0)) {
        Some(start) => text[start..].to_string(),
        None => match kind {
            ChangeKind::Add => diff::line_diff("", text),
            ChangeKind::Delete => diff::line_diff(text, ""),
            ChangeKind::Update => return None,
        },
    };
    Some(Event::FileChange { path, kind, hunks })
}

/// The run's edits as one git-style diff, paths relative to `cwd`.
pub fn run_diff(events_jsonl: &str, cwd: &str) -> String {
    let transcript = Transcript {
        info: SessionInfo {
            provider: crate::Provider::Codex,
            locator: String::new(),
            id: String::new(),
            cwd: cwd.to_string(),
            title: String::new(),
            updated_ms: 0,
        },
        events: edits(events_jsonl),
    };
    crate::unified_diff(&transcript)
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
    use serde_json::json;

    fn completed(item: Value) -> String {
        json!({"method": "item/completed", "params": {"threadId": "t", "item": item}}).to_string()
    }

    #[test]
    fn reads_codex_changes_and_adapter_edit_tools() {
        let log = [
            json!({"method": "item/agentMessage/delta", "params": {"delta": "x"}}).to_string(),
            completed(json!({"type": "fileChange", "id": "1", "status": "completed", "changes": [
                {"path": "/w/a.rs", "kind": {"type": "update", "move_path": null}, "diff": "--- a/a.rs\n+++ b/a.rs\n@@ -3 +3 @@\n-old\n+new\n"},
                {"path": "/w/new.txt", "kind": {"type": "add"}, "diff": "hello\n"}
            ]})),
            completed(json!({"type": "fileChange", "id": "2", "status": "completed", "toolName": "Edit",
                "input": {"file_path": "/w/b.rs", "old_string": "p\n", "new_string": "q\n"}})),
            completed(json!({"type": "fileChange", "id": "3", "status": "failed", "toolName": "Edit",
                "input": {"file_path": "/w/c.rs", "old_string": "x", "new_string": "y"}})),
            completed(json!({"type": "toolCall", "id": "4", "status": "completed", "toolName": "edit",
                "input": {"path": "/w/d.rs", "edits": [{"oldText": "1", "newText": "2"}]}})),
            completed(json!({"type": "commandExecution", "id": "5", "command": "ls", "status": "completed"})),
        ]
        .join("\n");
        let diff = run_diff(&log, "/w");
        for file in ["a.rs", "new.txt", "b.rs", "d.rs"] {
            assert!(diff.contains(&format!("diff --git a/{file} b/{file}")), "{file} missing:\n{diff}");
        }
        assert!(!diff.contains("c.rs"), "a failed edit changed nothing");
        assert!(diff.contains("@@ -3 +3 @@\n-old\n+new\n"), "Codex hunks keep their line numbers");
        assert!(diff.contains("new file mode") && diff.contains("+hello"));
        assert_eq!(diff.matches("+++ b/a.rs").count(), 1, "the recorded file headers are not repeated");
    }

    #[test]
    fn a_log_without_edits_has_no_diff() {
        assert_eq!(
            run_diff(&completed(json!({"type": "agentMessage", "id": "1", "text": "hi"})), "/w"),
            ""
        );
        assert_eq!(run_diff("not json\n", "/w"), "");
    }
}
