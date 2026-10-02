//! Claude Code (`~/.claude/projects/<project>/<session>.jsonl`) and Factory
//! Droid (`~/.factory/sessions/<project>/<session>.jsonl`). Both store
//! `{message: {role, content: [blocks]}}` rows with text, thinking,
//! `tool_use`, and `tool_result` blocks. Claude rows form a tree; Droid rows
//! are linear and start with a `session_start` row.

use crate::{ChangeKind, Event, Provider, SessionInfo, edit_from_tool, jsonl, str_field, text_of, title_from};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub fn info(provider: Provider, path: &Path) -> Option<SessionInfo> {
    let head = crate::read_head(path, 256 * 1024)?;
    let entries = jsonl::parse(&head);
    let mut cwd = String::new();
    let mut title = String::new();
    let mut id = path.file_stem()?.to_string_lossy().into_owned();
    for entry in &entries {
        if provider == Provider::Droid && str_field(entry, "type") == Some("session_start") {
            id = str_field(entry, "id").unwrap_or(&id).to_string();
            cwd = str_field(entry, "cwd").unwrap_or("").to_string();
            title = str_field(entry, "title").map(title_from).unwrap_or_default();
        }
        if str_field(entry, "type") == Some("summary") && title.is_empty() {
            title = str_field(entry, "summary").map(title_from).unwrap_or_default();
        }
        if cwd.is_empty() {
            cwd = str_field(entry, "cwd").unwrap_or("").to_string();
        }
        if title.is_empty()
            && let Some(text) = user_text(entry)
        {
            title = title_from(&text);
        }
        if !cwd.is_empty() && !title.is_empty() {
            break;
        }
    }
    Some(SessionInfo {
        provider,
        locator: path.to_string_lossy().into_owned(),
        id,
        cwd,
        title,
        updated_ms: 0,
    })
}

/// The visible text of a user row, skipping meta rows and injected
/// command/caveat wrappers.
fn user_text(entry: &Value) -> Option<String> {
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let message = entry.get("message")?;
    if str_field(message, "role") != Some("user") {
        return None;
    }
    let text = match message.get("content")? {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| str_field(b, "type") == Some("text"))
            .filter_map(|b| str_field(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty() && !injected(trimmed)).then(|| trimmed.to_string())
}

/// Text the harness wrote into a user row: reminders, slash-command
/// echoes, and caveats. The user did not type it.
fn injected(text: &str) -> bool {
    let text = text.trim_start();
    ["<command-", "<local-command", "<system-reminder>", "Caveat:", "<bash-"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

pub fn events(provider: Provider, text: &str) -> Vec<Event> {
    let entries = jsonl::parse(text);
    let entries = if provider == Provider::Claude {
        jsonl::claude_branch(entries)
    } else {
        entries
    };
    let mut events = Vec::new();
    // Edit tool call ID -> the FileChange event rebuilt from its input, so a
    // recorded patch can replace it.
    let mut edits: HashMap<String, usize> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    for entry in &entries {
        if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(message) = entry.get("message") else { continue };
        let role = str_field(message, "role").unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let blocks = match message.get("content") {
            Some(Value::String(text)) => {
                push_text(&mut events, role, text);
                continue;
            }
            Some(Value::Array(blocks)) => blocks,
            _ => continue,
        };
        for block in blocks {
            match str_field(block, "type") {
                Some("text") => push_text(&mut events, role, str_field(block, "text").unwrap_or("")),
                Some("thinking") => {
                    let text = str_field(block, "thinking").unwrap_or("");
                    if !text.trim().is_empty() {
                        events.push(Event::Thinking { text: text.to_string() });
                    }
                }
                Some("tool_use") => {
                    let name = str_field(block, "name").unwrap_or("unknown").to_string();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let call_id = str_field(block, "id").map(str::to_string);
                    if let Some(id) = &call_id {
                        names.insert(id.clone(), name.clone());
                    }
                    let edit = edit_from_tool(&name, &input);
                    events.push(Event::ToolCall {
                        name,
                        input,
                        call_id: call_id.clone(),
                    });
                    if let Some(edit) = edit {
                        if let Some(id) = call_id {
                            edits.insert(id, events.len());
                        }
                        events.push(edit);
                    }
                }
                Some("tool_result") => {
                    let call_id = str_field(block, "tool_use_id").map(str::to_string);
                    let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                    if !is_error
                        && let Some(index) = call_id.as_ref().and_then(|id| edits.get(id))
                        && let Some(patched) = recorded_patch(entry.get("toolUseResult"))
                        && let Event::FileChange { path, .. } = &events[*index]
                    {
                        events[*index] = Event::FileChange {
                            path: path.clone(),
                            kind: patched.0,
                            hunks: patched.1,
                        };
                    }
                    if is_error && let Some(index) = call_id.as_ref().and_then(|id| edits.remove(id)) {
                        // A failed edit changed nothing.
                        events[index] = Event::Thinking { text: String::new() };
                    }
                    events.push(Event::ToolResult {
                        name: call_id.as_ref().and_then(|id| names.get(id).cloned()),
                        call_id,
                        output: text_of(block.get("content").unwrap_or(&Value::Null)),
                        is_error,
                    });
                }
                Some("image") => push_text(&mut events, role, "[image]"),
                _ => {}
            }
        }
    }
    events.retain(|e| !matches!(e, Event::Thinking { text } if text.is_empty()));
    events
}

fn push_text(events: &mut Vec<Event>, role: &str, text: &str) {
    if text.trim().is_empty() || (role == "user" && injected(text)) {
        return;
    }
    events.push(if role == "user" {
        Event::User { text: text.to_string() }
    } else {
        Event::Assistant { text: text.to_string() }
    });
}

/// Claude's `toolUseResult.structuredPatch`: hunks with real line numbers.
fn recorded_patch(result: Option<&Value>) -> Option<(ChangeKind, String)> {
    let result = result?;
    let hunks = result.get("structuredPatch")?.as_array()?;
    let mut out = String::new();
    for hunk in hunks {
        let num = |key: &str| hunk.get(key).and_then(Value::as_i64).unwrap_or(0);
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            num("oldStart"),
            num("oldLines"),
            num("newStart"),
            num("newLines")
        ));
        for line in hunk.get("lines").and_then(Value::as_array).into_iter().flatten() {
            if let Some(line) = line.as_str() {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if out.is_empty() {
        // A create has no patch; its content is the whole file.
        if str_field(result, "type") == Some("create") {
            return Some((ChangeKind::Add, crate::line_diff("", str_field(result, "content").unwrap_or(""))));
        }
        return None;
    }
    let kind = if str_field(result, "type") == Some("create") {
        ChangeKind::Add
    } else {
        ChangeKind::Update
    };
    Some((kind, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(rows: &[Value]) -> String {
        rows.iter().map(|r| format!("{r}\n")).collect()
    }

    #[test]
    fn claude_uses_the_recorded_patch_and_skips_failed_edits() {
        let text = lines(&[
            json!({"uuid": "1", "parentUuid": null, "cwd": "/w", "message": {"role": "user", "content": "fix it"}}),
            json!({"uuid": "2", "parentUuid": "1", "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan"},
                {"type": "tool_use", "id": "t1", "name": "Edit", "input": {"file_path": "/w/a.rs", "old_string": "x", "new_string": "y"}},
                {"type": "tool_use", "id": "t2", "name": "Edit", "input": {"file_path": "/w/b.rs", "old_string": "p", "new_string": "q"}}
            ]}}),
            json!({"uuid": "3", "parentUuid": "2", "toolUseResult": {"structuredPatch": [{"oldStart": 40, "oldLines": 1, "newStart": 40, "newLines": 1, "lines": ["-x", "+y"]}]},
                   "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}}),
            json!({"uuid": "4", "parentUuid": "3", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "is_error": true, "content": "no match"}]}}),
            json!({"uuid": "5", "parentUuid": "4", "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}]}}),
        ]);
        let events = events(Provider::Claude, &text);
        let changes: Vec<&Event> = events.iter().filter(|e| matches!(e, Event::FileChange { .. })).collect();
        assert_eq!(changes.len(), 1, "the failed edit is dropped: {events:?}");
        let Event::FileChange { path, hunks, .. } = changes[0] else {
            unreachable!()
        };
        assert_eq!(path, "/w/a.rs");
        assert_eq!(hunks, "@@ -40,1 +40,1 @@\n-x\n+y\n");
        assert!(matches!(events.first(), Some(Event::User { text }) if text == "fix it"));
        assert!(matches!(events.last(), Some(Event::Assistant { text }) if text == "done"));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolResult { name: Some(n), .. } if n == "Edit"))
        );
    }

    #[test]
    fn droid_reads_its_session_start_row() {
        let dir = std::env::temp_dir().join(format!("ruddr-history-droid-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            lines(&[
                json!({"type": "session_start", "id": "abc", "title": "add grape", "cwd": "/w"}),
                json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "<system-reminder>\nUnified tool catalog"}]}}),
                json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "Append grape"}]}}),
                json!({"type": "message", "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "e", "name": "Edit", "input": {"file_path": "/w/README.md", "old_str": "fig", "new_str": "fig\ngrape"}}
                ]}}),
            ]),
        )
        .unwrap();
        let info = info(Provider::Droid, &path).unwrap();
        assert_eq!(
            (info.id.as_str(), info.cwd.as_str(), info.title.as_str()),
            ("abc", "/w", "add grape")
        );
        let events = events(Provider::Droid, &std::fs::read_to_string(&path).unwrap());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::FileChange { hunks, .. } if hunks.contains("+grape")))
        );
        let users: Vec<&Event> = events.iter().filter(|e| matches!(e, Event::User { .. })).collect();
        assert_eq!(
            users,
            [&Event::User {
                text: "Append grape".into()
            }],
            "injected reminders are not user turns"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn titles_skip_meta_and_injected_rows() {
        let entry = json!({"isMeta": true, "message": {"role": "user", "content": "meta"}});
        assert_eq!(user_text(&entry), None);
        let entry = json!({"message": {"role": "user", "content": "<command-name>/clear</command-name>"}});
        assert_eq!(user_text(&entry), None);
        let entry = json!({"message": {"role": "user", "content": [{"type": "text", "text": " real ask "}]}});
        assert_eq!(user_text(&entry).as_deref(), Some("real ask"));
    }
}
