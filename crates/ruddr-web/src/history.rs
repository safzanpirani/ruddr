//! Every agent's past sessions, read-only, beside Ruddr's own runs: the
//! dashboard's counterpart of the TUI's history list. A history session's
//! state directory is `history:<locator>`. The server reads only sessions
//! it listed or found itself, never a path the client supplies.

use ruddr_history::{Provider, SessionInfo, Stores};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

pub const PREFIX: &str = "history:";

/// How many sessions the list loads across all providers.
pub const LIMIT: usize = 400;

/// The sessions the client may open, keyed by state directory.
#[derive(Default)]
pub struct Known {
    listed: Mutex<HashMap<String, SessionInfo>>,
    /// Sessions opened from a deja search, which may be older than the list.
    found: Mutex<HashMap<String, SessionInfo>>,
}

fn state_dir(info: &SessionInfo) -> String {
    format!("{PREFIX}{}", info.locator)
}

impl Known {
    /// Lists the newest sessions and remembers them.
    pub fn list(&self) -> Vec<Value> {
        let sessions = ruddr_history::list_sessions(&Stores::discover(), LIMIT);
        let body = sessions.iter().map(session_json).collect();
        *self.listed.lock().unwrap() = sessions.into_iter().map(|info| (state_dir(&info), info)).collect();
        body
    }

    /// Finds one session by provider and ID, at any age, and remembers it.
    pub fn find(&self, provider: &str, id: &str) -> Option<Value> {
        let provider = Provider::ALL.into_iter().find(|p| p.name() == provider)?;
        let info = ruddr_history::find_session(&Stores::discover(), provider, id)?;
        let body = session_json(&info);
        self.found.lock().unwrap().insert(state_dir(&info), info);
        Some(body)
    }

    pub fn get(&self, state_dir: &str) -> Option<SessionInfo> {
        let listed = self.listed.lock().unwrap().get(state_dir).cloned();
        listed.or_else(|| self.found.lock().unwrap().get(state_dir).cloned())
    }
}

/// A history session in the shape of a run's state, plus its title.
pub fn session_json(info: &SessionInfo) -> Value {
    let updated = std::time::UNIX_EPOCH + std::time::Duration::from_millis(info.updated_ms.max(0) as u64);
    let updated = ruddr_core::time::format_rfc3339(updated);
    json!({
        "version": 1,
        "provider": info.provider.name(),
        "pid": 0,
        "status": "completed",
        "threadId": info.id,
        "cwd": info.cwd,
        "stateDir": state_dir(info),
        "stateFile": "",
        "startedAt": updated,
        "updatedAt": updated,
        "title": info.title,
    })
}

/// The whole session for the chat, output, and diff tabs. The chat is
/// app-server event lines, the shape a run's `events.jsonl` has.
pub fn load(info: &SessionInfo) -> Result<Value, String> {
    let transcript = ruddr_history::load(info)?;
    let mut chat = ruddr_history::app_server::chat_lines(&transcript.events).join("\n");
    if !chat.is_empty() {
        chat.push('\n');
    }
    Ok(json!({
        "chat": chat,
        "output": ruddr_history::app_server::output_lines(&transcript.events).join("\n"),
        "diff": {
            "content": ruddr_history::unified_diff(&transcript),
            "recorded": "Agent history",
            "cwd": info.cwd,
            "untracked": [],
            "touched": [],
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_session_looks_like_a_finished_run() {
        let info = SessionInfo {
            provider: Provider::Claude,
            locator: "/h/.claude/projects/-w/s.jsonl".into(),
            id: "s".into(),
            cwd: "/w".into(),
            title: "fix the parser".into(),
            updated_ms: 1_790_000_000_000,
        };
        let body = session_json(&info);
        assert_eq!(body["stateDir"], "history:/h/.claude/projects/-w/s.jsonl");
        assert_eq!(
            (body["status"].as_str(), body["provider"].as_str()),
            (Some("completed"), Some("claude"))
        );
        assert_eq!(body["threadId"], "s");
        assert_eq!(body["title"], "fix the parser");
        assert_eq!(
            ruddr_core::time::parse_rfc3339_ms(body["updatedAt"].as_str().unwrap()),
            Some(1_790_000_000_000)
        );
    }

    #[test]
    fn only_listed_or_found_sessions_can_be_read() {
        let known = Known::default();
        assert!(known.get("history:/etc/passwd").is_none());
        let info = SessionInfo {
            provider: Provider::Codex,
            locator: "/c/rollout.jsonl".into(),
            id: "c".into(),
            cwd: String::new(),
            title: String::new(),
            updated_ms: 0,
        };
        known.found.lock().unwrap().insert(state_dir(&info), info.clone());
        assert_eq!(known.get("history:/c/rollout.jsonl"), Some(info));
        assert!(known.find("nope", "c").is_none(), "an unknown provider finds nothing");
    }
}
