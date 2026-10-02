//! Session history from every agent on this machine: Codex, Claude Code, Pi,
//! OpenCode, and Factory Droid, whether or not Ruddr started the session.
//!
//! The parsers follow dejavu's (`~/Development/projects/dejavu`): Claude and
//! Pi transcripts are trees, so only the active branch is read; Codex,
//! Droid, and OpenCode are linear. File edits are rebuilt as unified diffs:
//! Codex records them, Claude records exact patches beside its edit tools,
//! and the other providers' edit-tool inputs are diffed here.
//!
//! Only session transcripts are read. Auth files that sit beside them
//! (`~/.factory/auth.json`, Codex and Claude credentials) are never opened.

mod claude;
mod codex;
mod diff;
mod jsonl;
mod opencode;
mod pi;
mod stores;

pub use diff::{line_diff, unified_diff};
pub use stores::Stores;

use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Codex,
    Claude,
    Pi,
    OpenCode,
    Droid,
}

impl Provider {
    pub const ALL: [Provider; 5] = [Provider::Codex, Provider::Claude, Provider::Pi, Provider::OpenCode, Provider::Droid];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
            Provider::Pi => "pi",
            Provider::OpenCode => "opencode",
            Provider::Droid => "droid",
        }
    }
}

/// One session as the list shows it, read cheaply from the file's head.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub provider: Provider,
    /// A transcript path, or `opencode://<database>#<session>`.
    pub locator: String,
    /// The provider's session ID, used to resume it.
    pub id: String,
    /// The session's working directory, when the transcript records it.
    pub cwd: String,
    /// The provider's title, or the first user message.
    pub title: String,
    /// Last change, as Unix milliseconds.
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Add,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        input: Value,
        call_id: Option<String>,
    },
    ToolResult {
        name: Option<String>,
        call_id: Option<String>,
        output: String,
        is_error: bool,
    },
    /// One file's edit as unified-diff hunks (from `@@`, no file headers).
    FileChange {
        path: String,
        kind: ChangeKind,
        hunks: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub info: SessionInfo,
    pub events: Vec<Event>,
}

/// The newest `limit` sessions across every provider, newest first.
pub fn list_sessions(stores: &Stores, limit: usize) -> Vec<SessionInfo> {
    let mut files = stores.transcript_files();
    files.sort_by_key(|f| std::cmp::Reverse(f.2));
    let mut sessions: Vec<SessionInfo> = files
        .into_iter()
        .take(limit)
        .filter_map(|(provider, path, mtime)| {
            let mut info = match provider {
                Provider::Claude | Provider::Droid => claude::info(provider, &path),
                Provider::Codex => codex::info(&path),
                Provider::Pi => pi::info(&path),
                Provider::OpenCode => None,
            }?;
            info.updated_ms = mtime;
            Some(info)
        })
        .collect();
    for database in &stores.opencode {
        sessions.extend(opencode::list(database, limit));
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    sessions.truncate(limit);
    sessions
}

/// Reads a whole session.
pub fn load(info: &SessionInfo) -> Result<Transcript, String> {
    let events = match info.provider {
        Provider::OpenCode => opencode::events(&info.locator)?,
        provider => {
            let text = std::fs::read_to_string(Path::new(&info.locator)).map_err(|e| format!("read {}: {e}", info.locator))?;
            match provider {
                Provider::Claude | Provider::Droid => claude::events(provider, &text),
                Provider::Codex => codex::events(&text),
                Provider::Pi => pi::events(&text),
                Provider::OpenCode => unreachable!(),
            }
        }
    };
    Ok(Transcript {
        info: info.clone(),
        events,
    })
}

// --- shared helpers -------------------------------------------------------

/// Flattens the text-bearing shapes every store uses: a string, or a list
/// of text-like blocks.
pub(crate) fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(block) => block
                    .get("text")
                    .or_else(|| block.get("thinking"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        matches!(block.get("type").and_then(Value::as_str), Some("image" | "input_image")).then(|| "[image]".into())
                    }),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(crate) fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A one-line title: the first non-empty line, at most 120 characters.
pub(crate) fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    if line.chars().count() > 120 {
        format!("{}…", line.chars().take(119).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Reads at most `max` bytes from the start of a file, cut at the last line.
pub(crate) fn read_head(path: &Path, max: usize) -> Option<String> {
    use std::io::Read;
    let mut buffer = vec![0u8; max];
    let mut file = std::fs::File::open(path).ok()?;
    let mut filled = 0;
    while filled < max {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    buffer.truncate(filled);
    if filled == max
        && let Some(end) = buffer.iter().rposition(|b| *b == b'\n')
    {
        buffer.truncate(end + 1);
    }
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// Turns an edit tool's input into a file change, whatever the provider
/// calls its fields. Returns `None` for tools that do not edit files.
pub(crate) fn edit_from_tool(name: &str, input: &Value) -> Option<Event> {
    let path = ["file_path", "filePath", "path"]
        .iter()
        .find_map(|key| str_field(input, key))?
        .to_string();
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "write" | "create" => {
            let content = ["content", "contents"].iter().find_map(|key| str_field(input, key)).unwrap_or("");
            Some(Event::FileChange {
                path,
                kind: ChangeKind::Add,
                hunks: diff::line_diff("", content),
            })
        }
        "edit" | "multiedit" | "str_replace" | "str_replace_editor" => {
            let mut pairs: Vec<(String, String)> = Vec::new();
            if let Some(edits) = input.get("edits").and_then(Value::as_array) {
                for edit in edits {
                    pairs.push((
                        pick(edit, &["old_string", "oldString", "old_str", "oldText"]),
                        pick(edit, &["new_string", "newString", "new_str", "newText"]),
                    ));
                }
            } else {
                pairs.push((
                    pick(input, &["old_string", "oldString", "old_str", "oldText"]),
                    pick(input, &["new_string", "newString", "new_str", "newText"]),
                ));
            }
            let hunks: String = pairs.iter().map(|(old, new)| diff::line_diff(old, new)).collect();
            (!hunks.is_empty()).then_some(Event::FileChange {
                path,
                kind: ChangeKind::Update,
                hunks,
            })
        }
        _ => None,
    }
}

fn pick(value: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|key| str_field(value, key)).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_tools_from_every_provider_become_changes() {
        let claude = edit_from_tool(
            "Edit",
            &json!({"file_path": "/w/a.rs", "old_string": "a\nb\n", "new_string": "a\nc\n"}),
        )
        .unwrap();
        let droid = edit_from_tool("Edit", &json!({"file_path": "/w/a.rs", "old_str": "x", "new_str": "y"})).unwrap();
        let pi = edit_from_tool("edit", &json!({"path": "/w/a.rs", "edits": [{"oldText": "1", "newText": "2"}]})).unwrap();
        let opencode = edit_from_tool("edit", &json!({"filePath": "/w/a.rs", "oldString": "p", "newString": "q"})).unwrap();
        let write = edit_from_tool("Write", &json!({"file_path": "/w/new.rs", "content": "hello\n"})).unwrap();
        for event in [&claude, &droid, &pi, &opencode] {
            assert!(
                matches!(
                    event,
                    Event::FileChange {
                        kind: ChangeKind::Update,
                        ..
                    }
                ),
                "{event:?}"
            );
        }
        let Event::FileChange { hunks, .. } = &claude else { unreachable!() };
        assert!(
            hunks.contains("-b\n") && hunks.contains("+c\n") && hunks.contains(" a\n"),
            "{hunks}"
        );
        let Event::FileChange { kind, hunks, .. } = &write else {
            unreachable!()
        };
        assert_eq!(*kind, ChangeKind::Add);
        assert!(hunks.contains("+hello\n"));
        assert!(edit_from_tool("Read", &json!({"file_path": "/w/a.rs"})).is_none());
        assert!(edit_from_tool("Bash", &json!({"command": "ls"})).is_none());
    }

    #[test]
    fn titles_are_one_trimmed_line() {
        assert_eq!(title_from("\n  fix the parser \nmore"), "fix the parser");
        assert_eq!(title_from(&"x".repeat(200)).chars().count(), 120);
    }
}
