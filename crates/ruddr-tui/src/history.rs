//! Every agent's past sessions, read-only, beside Ruddr's own runs. The
//! session list shows them as completed runs whose state directory is
//! `history:<locator>`. A selected session is loaded once and converted to
//! the app-server event lines the chat view already reads, and the diff tab
//! shows the session's own edits instead of `git diff`.

use crate::core::Session;
use ruddr_history::SessionInfo;
use serde_json::json;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use ruddr_history::app_server::chat_lines;
    use ruddr_history::{ChangeKind, Event, Provider};
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
