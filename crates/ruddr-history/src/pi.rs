//! Pi (`~/.pi/<profile>/sessions/--<project>--/<time>_<id>.jsonl`). The
//! first row is the session header with `id` and `cwd`; message rows form a
//! tree through `parentId`, and only the active branch is read.

use crate::{Event, Provider, SessionInfo, edit_from_tool, jsonl, str_field, text_of, title_from};
use serde_json::Value;
use std::path::Path;

pub fn info(path: &Path) -> Option<SessionInfo> {
    let head = crate::read_head(path, 256 * 1024)?;
    let entries = jsonl::parse(&head);
    let header = entries.iter().find(|e| str_field(e, "type") == Some("session"));
    let id = header.and_then(|h| str_field(h, "id")).map(str::to_string).unwrap_or_else(|| {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        stem.rsplit('_').next().unwrap_or(&stem).to_string()
    });
    let cwd = header.and_then(|h| str_field(h, "cwd")).unwrap_or("").to_string();
    let title = entries
        .iter()
        .filter(|e| str_field(e, "type") == Some("message"))
        .filter_map(|e| e.get("message"))
        .filter(|m| str_field(m, "role") == Some("user"))
        .map(|m| text_of(m.get("content").unwrap_or(&Value::Null)))
        .find(|t| !t.trim().is_empty())
        .map(|t| title_from(&t))
        .unwrap_or_default();
    Some(SessionInfo {
        provider: Provider::Pi,
        locator: path.to_string_lossy().into_owned(),
        id,
        cwd,
        title,
        updated_ms: 0,
    })
}

pub fn events(text: &str) -> Vec<Event> {
    let mut events = Vec::new();
    for entry in jsonl::pi_branch(jsonl::parse(text)) {
        if str_field(&entry, "type") != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else { continue };
        let role = str_field(message, "role").unwrap_or("");
        if role == "toolResult" {
            events.push(Event::ToolResult {
                name: str_field(message, "toolName").map(str::to_string),
                call_id: str_field(message, "toolCallId").map(str::to_string),
                output: text_of(message.get("content").unwrap_or(&Value::Null)),
                is_error: message.get("isError").and_then(Value::as_bool) == Some(true),
            });
            continue;
        }
        if role != "user" && role != "assistant" {
            continue;
        }
        let blocks = match message.get("content") {
            Some(Value::String(text)) => {
                push_text(&mut events, role, text);
                continue;
            }
            Some(Value::Array(blocks)) => blocks.clone(),
            _ => continue,
        };
        for block in &blocks {
            match str_field(block, "type") {
                Some("text") => push_text(&mut events, role, str_field(block, "text").unwrap_or("")),
                Some("thinking") => {
                    let text = str_field(block, "thinking").unwrap_or("");
                    if !text.trim().is_empty() {
                        events.push(Event::Thinking { text: text.to_string() });
                    }
                }
                Some("toolCall") => {
                    let name = str_field(block, "name").unwrap_or("unknown").to_string();
                    let input = block.get("arguments").cloned().unwrap_or(Value::Null);
                    let edit = edit_from_tool(&name, &input);
                    events.push(Event::ToolCall {
                        name,
                        input,
                        call_id: str_field(block, "id").map(str::to_string),
                    });
                    events.extend(edit);
                }
                Some("image") => push_text(&mut events, role, "[image]"),
                _ => {}
            }
        }
    }
    events
}

fn push_text(events: &mut Vec<Event>, role: &str, text: &str) {
    if !text.trim().is_empty() {
        events.push(if role == "user" {
            Event::User { text: text.to_string() }
        } else {
            Event::Assistant { text: text.to_string() }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_active_branch_with_edits_and_results() {
        let rows = [
            json!({"type": "session", "id": "s1", "cwd": "/w"}),
            json!({"type": "message", "id": "1", "parentId": null, "message": {"role": "user", "content": [{"type": "text", "text": "rename x"}]}}),
            json!({"type": "message", "id": "abandoned", "parentId": "1", "message": {"role": "assistant", "content": [{"type": "text", "text": "old branch"}]}}),
            json!({"type": "message", "id": "2", "parentId": "1", "message": {"role": "assistant", "content": [
                {"type": "toolCall", "id": "t", "name": "edit", "arguments": {"path": "src/a.rs", "edits": [{"oldText": "x", "newText": "y"}]}}
            ]}}),
            json!({"type": "message", "id": "3", "parentId": "2", "message": {"role": "toolResult", "toolCallId": "t", "toolName": "edit", "content": [{"type": "text", "text": "ok"}]}}),
        ];
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        let events = events(&text);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Assistant { text } if text == "old branch"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::FileChange { path, hunks, .. } if path == "src/a.rs" && hunks.contains("+y")))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolResult { name: Some(n), .. } if n == "edit"))
        );
    }
}
