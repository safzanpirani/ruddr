//! File edits from a Ruddr run's own `events.jsonl`, for working directories
//! that `git diff` cannot describe. Codex reports `fileChange` items with a
//! `changes` list of `{path, kind: {type}, diff}`. Ruddr's adapters report
//! edit tools as items carrying the provider's `toolName` and `input`.

use crate::{ChangeKind, Event, SessionInfo, Transcript, diff, edit_from_tool, str_field};
use serde_json::Value;

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
