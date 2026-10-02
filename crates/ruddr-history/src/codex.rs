//! Codex (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`). Current Codex
//! writes `event_msg` `item_completed` rows that carry whole items: user and
//! agent messages, reasoning, commands with output, and file changes with
//! real unified diffs. Older rollouts only have `response_item` rows; those
//! are read the way dejavu reads them, and `apply_patch` inputs become diffs.

use crate::{ChangeKind, Event, Provider, SessionInfo, edit_from_tool, jsonl, str_field, text_of, title_from};
use serde_json::Value;
use std::path::Path;

pub fn info(path: &Path) -> Option<SessionInfo> {
    let head = crate::read_head(path, 256 * 1024)?;
    let mut id = String::new();
    let mut cwd = String::new();
    let mut title = String::new();
    for entry in jsonl::parse(&head) {
        let payload = entry.get("payload").cloned().unwrap_or(Value::Null);
        match (str_field(&entry, "type"), str_field(&payload, "type")) {
            (Some("session_meta"), _) => {
                id = str_field(&payload, "id").unwrap_or("").to_string();
                cwd = str_field(&payload, "cwd").unwrap_or("").to_string();
            }
            (Some("response_item"), Some("message")) if title.is_empty() && str_field(&payload, "role") == Some("user") => {
                title = visible_user_text(&text_of(payload.get("content").unwrap_or(&Value::Null)))
                    .map(|t| title_from(&t))
                    .unwrap_or_default();
            }
            (Some("event_msg"), Some("user_message")) if title.is_empty() => {
                title = visible_user_text(str_field(&payload, "message").unwrap_or(""))
                    .map(|t| title_from(&t))
                    .unwrap_or_default();
            }
            _ => {}
        }
        if !id.is_empty() && !title.is_empty() {
            break;
        }
    }
    if id.is_empty() {
        // rollout-2026-10-02T16-07-31-<uuid>.jsonl
        let stem = path.file_stem()?.to_string_lossy().into_owned();
        id = stem
            .rsplitn(6, '-')
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("-");
    }
    Some(SessionInfo {
        provider: Provider::Codex,
        locator: path.to_string_lossy().into_owned(),
        id,
        cwd,
        title,
        updated_ms: 0,
    })
}

/// Codex injects environment and instruction blocks as user messages.
fn visible_user_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let injected = [
        "<environment_context>",
        "<user_instructions>",
        "<permissions",
        "# AGENTS.md",
        "<INSTRUCTIONS>",
        "<turn_aborted>",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix));
    (!trimmed.is_empty() && !injected).then(|| trimmed.to_string())
}

pub fn events(text: &str) -> Vec<Event> {
    let entries = jsonl::parse(text);
    let has_items = entries
        .iter()
        .any(|e| str_field(e.get("payload").unwrap_or(&Value::Null), "type") == Some("item_completed"));
    if has_items {
        item_events(&entries)
    } else {
        response_events(&entries)
    }
}

fn item_events(entries: &[Value]) -> Vec<Event> {
    let mut events = Vec::new();
    for entry in entries {
        let Some(payload) = entry.get("payload") else { continue };
        if str_field(payload, "type") != Some("item_completed") {
            continue;
        }
        let Some(item) = payload.get("item") else { continue };
        match str_field(item, "type").unwrap_or("") {
            "UserMessage" => {
                if let Some(text) = visible_user_text(&text_of(item.get("content").unwrap_or(&Value::Null))) {
                    events.push(Event::User { text });
                }
            }
            "AgentMessage" => {
                let text = content_text(item.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    events.push(Event::Assistant { text });
                }
            }
            "Reasoning" => {
                let text = text_of(item.get("summary_text").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    events.push(Event::Thinking { text });
                }
            }
            "CommandExecution" => {
                let command = command_text(item);
                let call_id = str_field(item, "id").map(str::to_string);
                events.push(Event::ToolCall {
                    name: "shell".into(),
                    input: Value::String(command),
                    call_id: call_id.clone(),
                });
                let exit = item.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
                events.push(Event::ToolResult {
                    name: Some("shell".into()),
                    call_id,
                    output: str_field(item, "aggregated_output").unwrap_or("").to_string(),
                    is_error: exit != 0 || str_field(item, "status") == Some("failed"),
                });
            }
            "FileChange" => {
                let Some(changes) = item.get("changes").and_then(Value::as_object) else {
                    continue;
                };
                if str_field(item, "status") == Some("failed") {
                    continue;
                }
                for (path, change) in changes {
                    events.push(file_change(path, change));
                }
            }
            "WebSearch" | "McpToolCall" | "ImageView" => {
                let name = str_field(item, "type").unwrap_or("tool").to_string();
                events.push(Event::ToolCall {
                    name,
                    input: item.clone(),
                    call_id: str_field(item, "id").map(str::to_string),
                });
            }
            _ => {}
        }
    }
    events
}

fn content_text(content: &Value) -> String {
    match content {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| str_field(item, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        other => text_of(other),
    }
}

fn command_text(item: &Value) -> String {
    if let Some(parsed) = item.get("parsed_cmd").and_then(Value::as_array) {
        let commands: Vec<&str> = parsed.iter().filter_map(|p| str_field(p, "cmd")).collect();
        if !commands.is_empty() {
            return commands.join(" && ");
        }
    }
    match item.get("command") {
        Some(Value::Array(argv)) => argv.last().and_then(Value::as_str).unwrap_or("").to_string(),
        Some(Value::String(command)) => command.clone(),
        _ => String::new(),
    }
}

fn file_change(path: &str, change: &Value) -> Event {
    let kind = match str_field(change, "type") {
        Some("add") => ChangeKind::Add,
        Some("delete") => ChangeKind::Delete,
        _ => ChangeKind::Update,
    };
    let hunks = match (str_field(change, "unified_diff"), str_field(change, "content")) {
        (Some(diff), _) => diff.to_string(),
        (None, Some(content)) if kind == ChangeKind::Delete => crate::line_diff(content, ""),
        (None, Some(content)) => crate::line_diff("", content),
        _ => String::new(),
    };
    Event::FileChange {
        path: path.to_string(),
        kind,
        hunks,
    }
}

fn response_events(entries: &[Value]) -> Vec<Event> {
    let mut events = Vec::new();
    for entry in entries {
        if str_field(entry, "type") != Some("response_item") {
            continue;
        }
        let Some(payload) = entry.get("payload") else { continue };
        match str_field(payload, "type").unwrap_or("") {
            "message" => {
                let text = text_of(payload.get("content").unwrap_or(&Value::Null));
                match str_field(payload, "role") {
                    Some("user") => {
                        if let Some(text) = visible_user_text(&text) {
                            events.push(Event::User { text });
                        }
                    }
                    Some("assistant") if !text.trim().is_empty() => events.push(Event::Assistant { text }),
                    _ => {}
                }
            }
            "reasoning" => {
                let text = [
                    text_of(payload.get("summary").unwrap_or(&Value::Null)),
                    text_of(payload.get("content").unwrap_or(&Value::Null)),
                ]
                .into_iter()
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
                if !text.trim().is_empty() {
                    events.push(Event::Thinking { text });
                }
            }
            kind @ ("function_call" | "custom_tool_call" | "local_shell_call") => {
                let name = if kind == "local_shell_call" {
                    "shell"
                } else {
                    str_field(payload, "name").unwrap_or("unknown")
                }
                .to_string();
                let input = match payload.get("arguments").or_else(|| payload.get("input")) {
                    Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone())),
                    Some(other) => other.clone(),
                    None => payload.get("action").cloned().unwrap_or(Value::Null),
                };
                let call_id = str_field(payload, "call_id").map(str::to_string);
                let patch = apply_patch_text(&name, &input).map(parse_apply_patch);
                let edit = edit_from_tool(&name, &input);
                events.push(Event::ToolCall { name, input, call_id });
                events.extend(patch.into_iter().flatten());
                events.extend(edit);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let output = text_of(payload.get("output").unwrap_or(&Value::Null));
                let is_error = ["Script failed", "Error:", "Process exited with code ", "Exit code: "]
                    .iter()
                    .any(|prefix| output.trim_start().starts_with(prefix))
                    && !output.contains("exited with code 0")
                    && !output.contains("Exit code: 0");
                events.push(Event::ToolResult {
                    name: None,
                    call_id: str_field(payload, "call_id").map(str::to_string),
                    output,
                    is_error,
                });
            }
            _ => {}
        }
    }
    events
}

fn apply_patch_text(name: &str, input: &Value) -> Option<String> {
    let text = match input {
        Value::String(text) => text.clone(),
        other => str_field(other, "input")
            .or_else(|| str_field(other, "patch"))
            .unwrap_or("")
            .to_string(),
    };
    (name == "apply_patch" || text.trim_start().starts_with("*** Begin Patch"))
        .then_some(text)
        .filter(|t| t.contains("*** "))
}

/// Codex's `apply_patch` format: `*** Update File: PATH` sections whose `@@`
/// blocks hold context, `-`, and `+` lines without line numbers.
pub(crate) fn parse_apply_patch(text: String) -> Vec<Event> {
    let mut events = Vec::new();
    let mut current: Option<(String, ChangeKind, Vec<String>)> = None;
    let flush = |current: &mut Option<(String, ChangeKind, Vec<String>)>, events: &mut Vec<Event>| {
        if let Some((path, kind, lines)) = current.take() {
            let mut hunks = String::new();
            for block in lines.split(|l| l.starts_with("@@")).filter(|b| !b.is_empty()) {
                let old = block.iter().filter(|l| !l.starts_with('+')).count();
                let new = block.iter().filter(|l| !l.starts_with('-')).count();
                hunks.push_str(&format!("@@ -1,{old} +1,{new} @@\n"));
                for line in block {
                    let line = if line.is_empty() { " " } else { line.as_str() };
                    hunks.push_str(line);
                    hunks.push('\n');
                }
            }
            events.push(Event::FileChange { path, kind, hunks });
        }
    };
    for line in text.lines() {
        let header = |prefix: &str| line.strip_prefix(prefix).map(|p| p.trim().to_string());
        if let Some(path) = header("*** Update File: ") {
            flush(&mut current, &mut events);
            current = Some((path, ChangeKind::Update, Vec::new()));
        } else if let Some(path) = header("*** Add File: ") {
            flush(&mut current, &mut events);
            current = Some((path, ChangeKind::Add, Vec::new()));
        } else if let Some(path) = header("*** Delete File: ") {
            flush(&mut current, &mut events);
            events.push(Event::FileChange {
                path,
                kind: ChangeKind::Delete,
                hunks: String::new(),
            });
        } else if line.starts_with("*** ") {
            if line.starts_with("*** End Patch") {
                flush(&mut current, &mut events);
            }
        } else if let Some((_, _, lines)) = current.as_mut() {
            lines.push(line.to_string());
        }
    }
    flush(&mut current, &mut events);
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(rows: &[Value]) -> String {
        rows.iter().map(|r| format!("{r}\n")).collect()
    }

    #[test]
    fn current_rollouts_read_items_with_real_diffs() {
        let text = lines(&[
            json!({"type": "session_meta", "payload": {"id": "sess", "cwd": "/w"}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "text", "text": "<environment_context>x</environment_context>"}]}}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "text", "text": "fix the reader"}]}}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "CommandExecution", "id": "c", "command": ["/bin/zsh", "-lc", "cat a"], "parsed_cmd": [{"cmd": "cat a"}], "aggregated_output": "hi", "exit_code": 0, "status": "completed"}}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "FileChange", "status": "completed", "changes": {"/w/a.rs": {"type": "update", "unified_diff": "@@ -3,1 +3,1 @@\n-a\n+b\n"}}}}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "AgentMessage", "content": [{"type": "Text", "text": "done"}]}}}),
        ]);
        let events = events(&text);
        assert_eq!(
            events.iter().filter(|e| matches!(e, Event::User { .. })).count(),
            1,
            "the injected context is skipped"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolCall { input, .. } if input == "cat a"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::FileChange { path, hunks, .. } if path == "/w/a.rs" && hunks.starts_with("@@ -3,1")))
        );
        assert!(matches!(events.last(), Some(Event::Assistant { text }) if text == "done"));
    }

    #[test]
    fn older_rollouts_turn_apply_patch_into_changes() {
        let patch =
            "*** Begin Patch\n*** Update File: src/a.rs\n@@\n fn a() {\n-    1\n+    2\n }\n*** Add File: b.txt\n+hello\n*** End Patch\n";
        let text = lines(&[
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "change a"}]}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch", "call_id": "p", "input": patch}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "p", "output": "Done"}}),
        ]);
        let events = events(&text);
        let changes: Vec<&Event> = events.iter().filter(|e| matches!(e, Event::FileChange { .. })).collect();
        assert_eq!(changes.len(), 2);
        assert!(
            matches!(changes[0], Event::FileChange { path, hunks, .. } if path == "src/a.rs" && hunks == "@@ -1,3 +1,3 @@\n fn a() {\n-    1\n+    2\n }\n")
        );
        assert!(matches!(changes[1], Event::FileChange { kind: ChangeKind::Add, hunks, .. } if hunks.contains("+hello")));
    }

    #[test]
    fn rollout_ids_come_from_the_file_name_when_meta_is_missing() {
        let dir = std::env::temp_dir().join(format!("ruddr-history-codex-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-10-02T16-07-31-01a0fc30-ca77-7601-855e-298f2730fe7b.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        assert_eq!(info(&path).unwrap().id, "01a0fc30-ca77-7601-855e-298f2730fe7b");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
