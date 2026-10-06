//! Pi (`~/.pi/<profile>/sessions/--<project>--/<time>_<id>.jsonl`) and omp
//! (`~/.omp/agent/sessions/...`, the same layout). The session header row
//! has `id` and `cwd`; omp writes a `title` row before it. Message rows form
//! a tree through `parentId`, and only the active branch is read.
//!
//! omp's hashline `edit` arguments name anchored lines rather than old and
//! new text, so its edits come from the tool result instead: `details.diff`
//! (numbered rows), or `details.perFileResults` for a multi-file edit.

use crate::{ChangeKind, Event, Provider, SessionInfo, diff, edit_from_tool, jsonl, str_field, text_of, title_from};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn info(provider: Provider, path: &Path) -> Option<SessionInfo> {
    let head = crate::read_head(path, 256 * 1024)?;
    let entries = jsonl::parse(&head);
    let header = entries.iter().find(|e| str_field(e, "type") == Some("session"));
    let id = header.and_then(|h| str_field(h, "id")).map(str::to_string).unwrap_or_else(|| {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        stem.rsplit('_').next().unwrap_or(&stem).to_string()
    });
    let cwd = header.and_then(|h| str_field(h, "cwd")).unwrap_or("").to_string();
    let named = entries
        .iter()
        .filter(|e| str_field(e, "type") == Some("title"))
        .filter_map(|e| str_field(e, "title"))
        .find(|t| !t.trim().is_empty())
        .map(title_from);
    let title = named.unwrap_or_else(|| {
        entries
            .iter()
            .filter(|e| str_field(e, "type") == Some("message"))
            .filter_map(|e| e.get("message"))
            .filter(|m| str_field(m, "role") == Some("user"))
            .map(|m| text_of(m.get("content").unwrap_or(&Value::Null)))
            .find(|t| !t.trim().is_empty())
            .map(|t| title_from(&t))
            .unwrap_or_default()
    });
    Some(SessionInfo {
        provider,
        locator: path.to_string_lossy().into_owned(),
        id,
        cwd,
        title,
        updated_ms: 0,
    })
}

pub fn events(text: &str) -> Vec<Event> {
    let mut events = Vec::new();
    let mut edits = HashMap::new();
    let mut failed_edits = HashSet::new();
    // Edit calls whose arguments gave no diff, by call ID, with their path.
    let mut result_edits: HashMap<String, String> = HashMap::new();
    for entry in jsonl::pi_branch(jsonl::parse(text)) {
        if str_field(&entry, "type") != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else { continue };
        let role = str_field(message, "role").unwrap_or("");
        if role == "toolResult" {
            let call_id = str_field(message, "toolCallId");
            let is_error = message.get("isError").and_then(Value::as_bool) == Some(true);
            if let Some(index) = call_id.and_then(|id| edits.remove(id))
                && is_error
            {
                failed_edits.insert(index);
            }
            events.push(Event::ToolResult {
                name: str_field(message, "toolName").map(str::to_string),
                call_id: call_id.map(str::to_string),
                output: text_of(message.get("content").unwrap_or(&Value::Null)),
                is_error,
            });
            if let Some(path) = call_id.and_then(|id| result_edits.remove(id))
                && !is_error
                && let Some(details) = message.get("details")
            {
                events.extend(changes_from_details(details, &path));
            }
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
                    if edit.is_none()
                        && matches!(name.as_str(), "edit" | "ast_edit")
                        && let Some(id) = str_field(block, "id")
                    {
                        let path = str_field(&input, "path").unwrap_or_default().to_string();
                        result_edits.insert(id.to_string(), path);
                    }
                    events.push(Event::ToolCall {
                        name,
                        input,
                        call_id: str_field(block, "id").map(str::to_string),
                    });
                    if let Some(edit) = edit {
                        if let Some(id) = str_field(block, "id") {
                            edits.insert(id.to_string(), events.len());
                        }
                        events.push(edit);
                    }
                }
                Some("image") => push_text(&mut events, role, "[image]"),
                _ => {}
            }
        }
    }
    events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| (!failed_edits.contains(&index)).then_some(event))
        .collect()
}

/// File changes from an edit result's `details`: one per file, each with
/// its own path when omp records one, else the path the call named.
fn changes_from_details(details: &Value, call_path: &str) -> Vec<Event> {
    let files: Vec<&Value> = match details.get("perFileResults").and_then(Value::as_array) {
        Some(files) => files.iter().collect(),
        None => vec![details],
    };
    files
        .into_iter()
        .filter_map(|file| {
            let path = str_field(file, "path").unwrap_or(call_path);
            let hunks = match (str_field(file, "diff"), str_field(file, "oldText"), str_field(file, "newText")) {
                (Some(numbered), ..) if !numbered.is_empty() => ruddr_core::diff::numbered_diff(numbered),
                (_, Some(old), Some(new)) => diff::line_diff(old, new),
                _ => String::new(),
            };
            let kind = match str_field(file, "op") {
                Some("create") => ChangeKind::Add,
                Some("delete") => ChangeKind::Delete,
                _ => ChangeKind::Update,
            };
            (!path.is_empty() && !hunks.is_empty()).then(|| Event::FileChange {
                path: path.to_string(),
                kind,
                hunks,
            })
        })
        .collect()
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
    fn failed_edit_results_remove_only_the_matching_change() {
        let rows = [
            json!({"type": "message", "id": "1", "parentId": null, "message": {"role": "assistant", "content": [
                {"type": "toolCall", "id": "failed", "name": "edit", "arguments": {"path": "failed.rs", "oldText": "x", "newText": "y"}},
                {"type": "toolCall", "id": "ok", "name": "edit", "arguments": {"path": "ok.rs", "oldText": "a", "newText": "b"}}
            ]}}),
            json!({"type": "message", "id": "2", "parentId": "1", "message": {"role": "toolResult", "toolCallId": "ok", "toolName": "edit", "isError": false, "content": "ok"}}),
            json!({"type": "message", "id": "3", "parentId": "2", "message": {"role": "toolResult", "toolCallId": "failed", "toolName": "edit", "isError": true, "content": "text not found"}}),
        ];
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        let events = events(&text);
        let changed: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                Event::FileChange { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(changed, ["ok.rs"]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ToolResult { call_id: Some(id), is_error: true, .. } if id == "failed"))
        );
        assert_eq!(events.iter().filter(|event| matches!(event, Event::ToolCall { .. })).count(), 2);
    }

    #[test]
    fn omp_edits_come_from_the_numbered_diff_in_the_result() {
        let rows = [
            json!({"type": "title", "v": 1, "title": "Rename the flag"}),
            json!({"type": "session", "version": 3, "id": "o1", "cwd": "/w"}),
            json!({"type": "message", "id": "1", "parentId": null, "message": {"role": "assistant", "content": [
                {"type": "toolCall", "id": "hash", "name": "edit", "arguments": {"path": "/w/a.rs",
                    "edits": [{"op": "replace", "pos": "2#VY", "lines": ["new"]}]}},
                {"type": "toolCall", "id": "bad", "name": "edit", "arguments": {"path": "/w/b.rs",
                    "edits": [{"op": "replace", "pos": "1#AA", "lines": ["x"]}]}},
                {"type": "toolCall", "id": "multi", "name": "edit", "arguments": {"edits": []}}
            ]}}),
            json!({"type": "message", "id": "2", "parentId": "1", "message": {"role": "toolResult", "toolCallId": "hash",
                "toolName": "edit", "isError": false, "content": [{"type": "text", "text": "ok"}],
                "details": {"diff": " 1|keep\n-2|old\n+2|new\n 3|...", "op": "update"}}}),
            json!({"type": "message", "id": "3", "parentId": "2", "message": {"role": "toolResult", "toolCallId": "bad",
                "toolName": "edit", "isError": true, "content": "stale anchor", "details": {"diff": "+1|x"}}}),
            json!({"type": "message", "id": "4", "parentId": "3", "message": {"role": "toolResult", "toolCallId": "multi",
            "toolName": "edit", "isError": false, "content": "ok", "details": {"perFileResults": [
                {"path": "/w/c.rs", "diff": "+1|created", "op": "create"},
                {"path": "/w/d.rs", "oldText": "a\n", "newText": "b\n", "op": "update"}
            ]}}}),
        ];
        let mut text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        // omp rewrites its title row in place; one written last is still not the leaf.
        text.push_str(&format!("{}\n", json!({"type": "title", "v": 1, "title": "Rename the flag"})));
        let changes: Vec<(String, ChangeKind, String)> = events(&text)
            .into_iter()
            .filter_map(|event| match event {
                Event::FileChange { path, kind, hunks } => Some((path, kind, hunks)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes,
            [
                ("/w/a.rs".into(), ChangeKind::Update, "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n".into()),
                ("/w/c.rs".into(), ChangeKind::Add, "@@ -0,0 +1,1 @@\n+created\n".into()),
                ("/w/d.rs".into(), ChangeKind::Update, "@@ -1,1 +1,1 @@\n-a\n+b\n".into()),
            ]
        );

        let dir = std::env::temp_dir().join(format!("ruddr-history-omp-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("2026-10-06T08-24-49-835Z_o1.jsonl");
        std::fs::write(&file, &text).unwrap();
        let info = info(Provider::Omp, &file).unwrap();
        assert_eq!((info.provider, info.id.as_str(), info.cwd.as_str()), (Provider::Omp, "o1", "/w"));
        assert_eq!(info.title, "Rename the flag");
        std::fs::remove_dir_all(dir).unwrap();
    }

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
