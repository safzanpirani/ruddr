//! OpenCode (`$XDG_DATA_HOME/opencode/{opencode,opencode-next,opencode-local}.db`).
//! Legacy stores keep `session`, `message`, and `part` rows; OpenCode 2
//! stores keep `session_v2` and one JSON `session_message` row per message.
//! A session with any `session_message` row is read from v2 only. Ported
//! from dejavu's `opencode-store.ts` and `transcript-view.ts`.

use crate::{Event, Provider, SessionInfo, edit_from_tool, str_field, text_of, title_from};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;

/// `opencode://<database>#<session>`.
pub fn locator(database: &Path, session: &str) -> String {
    format!("opencode://{}#{}", database.display(), session)
}

fn parse_locator(locator: &str) -> Result<(String, String), String> {
    let rest = locator
        .strip_prefix("opencode://")
        .ok_or_else(|| format!("not an OpenCode locator: {locator}"))?;
    let (database, session) = rest
        .rsplit_once('#')
        .ok_or_else(|| format!("OpenCode locator has no session: {locator}"))?;
    Ok((database.to_string(), session.to_string()))
}

/// A read-only connection. A WAL database with no `-shm` file cannot be
/// opened read-only, so that case reads the committed file as immutable.
fn open(database: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match Connection::open_with_flags(database, flags)
        .and_then(|c| c.query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(())).map(|_| c))
    {
        Ok(connection) => Ok(connection),
        Err(_) => Connection::open_with_flags(
            format!("file:{}?immutable=1", database.display()),
            flags | OpenFlags::SQLITE_OPEN_URI,
        ),
    }
}

fn has_table(connection: &Connection, name: &str) -> bool {
    connection
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| Ok(()))
        .is_ok()
}

/// The newest `limit` sessions in one database.
pub fn list(database: &Path, limit: usize) -> Vec<SessionInfo> {
    let Ok(connection) = open(database) else { return Vec::new() };
    let mut sessions = Vec::new();
    for table in ["session_v2", "session"] {
        if !has_table(&connection, table) {
            continue;
        }
        let sql = format!(
            "SELECT id, COALESCE(directory, ''), COALESCE(title, ''), COALESCE(time_updated, 0) FROM {table} \
             WHERE time_archived IS NULL ORDER BY time_updated DESC LIMIT ?1"
        );
        let Ok(mut statement) = connection.prepare(&sql) else { continue };
        let rows = statement.query_map([limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        });
        for (id, directory, title, updated) in rows.into_iter().flatten().flatten() {
            if sessions.iter().any(|s: &SessionInfo| s.id == id) {
                continue;
            }
            sessions.push(SessionInfo {
                provider: Provider::OpenCode,
                locator: locator(database, &id),
                id,
                cwd: directory,
                title: title_from(&title),
                updated_ms: updated,
            });
        }
    }
    sessions
}

pub fn events(locator: &str) -> Result<Vec<Event>, String> {
    let (database, session) = parse_locator(locator)?;
    let connection = open(Path::new(&database)).map_err(|e| format!("open {database}: {e}"))?;
    let uses_v2 = has_table(&connection, "session_message")
        && connection
            .query_row(
                "SELECT 1 FROM session_message WHERE session_id = ?1 LIMIT 1",
                [&session],
                |_| Ok(()),
            )
            .is_ok();
    if uses_v2 {
        v2_events(&connection, &session)
    } else {
        legacy_events(&connection, &session)
    }
    .map_err(|e| format!("read {locator}: {e}"))
}

fn legacy_events(connection: &Connection, session: &str) -> rusqlite::Result<Vec<Event>> {
    if !has_table(connection, "part") {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT m.data, p.data FROM message m JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ?1 ORDER BY m.time_created, m.id, p.time_created, p.id",
    )?;
    let rows = statement.query_map([session], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    let mut events = Vec::new();
    for (message, part) in rows.flatten() {
        let message: Value = serde_json::from_str(&message).unwrap_or(Value::Null);
        let part: Value = serde_json::from_str(&part).unwrap_or(Value::Null);
        let role = if str_field(&message, "role") == Some("assistant") {
            "assistant"
        } else {
            "user"
        };
        match str_field(&part, "type") {
            Some("text") => push_text(&mut events, role, str_field(&part, "text").unwrap_or("")),
            Some("reasoning") => push_thinking(&mut events, str_field(&part, "text").unwrap_or("")),
            Some("file") => push_text(
                &mut events,
                role,
                &format!(
                    "[file: {}]",
                    str_field(&part, "filename")
                        .or_else(|| str_field(&part, "mime"))
                        .unwrap_or("attachment")
                ),
            ),
            Some("tool") => {
                let state = part.get("state").cloned().unwrap_or(Value::Null);
                push_tool(
                    &mut events,
                    str_field(&part, "tool").unwrap_or("unknown"),
                    str_field(&part, "callID"),
                    &state,
                    false,
                );
            }
            _ => {}
        }
    }
    Ok(events)
}

fn v2_events(connection: &Connection, session: &str) -> rusqlite::Result<Vec<Event>> {
    let mut statement = connection
        .prepare("SELECT type, data FROM session_message WHERE session_id = ?1 AND type IN ('user', 'assistant') ORDER BY seq")?;
    let rows = statement.query_map([session], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    let mut events = Vec::new();
    for (kind, data) in rows.flatten() {
        let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        if kind == "user" {
            push_text(&mut events, "user", str_field(&data, "text").unwrap_or(""));
            for file in data.get("files").and_then(Value::as_array).into_iter().flatten() {
                push_text(
                    &mut events,
                    "user",
                    &format!(
                        "[file: {}]",
                        str_field(file, "name").or_else(|| str_field(file, "mime")).unwrap_or("attachment")
                    ),
                );
            }
            continue;
        }
        for block in data.get("content").and_then(Value::as_array).into_iter().flatten() {
            match str_field(block, "type") {
                Some("text") => push_text(&mut events, "assistant", str_field(block, "text").unwrap_or("")),
                Some("reasoning") => push_thinking(&mut events, str_field(block, "text").unwrap_or("")),
                Some("tool") => {
                    let state = block.get("state").cloned().unwrap_or(Value::Null);
                    push_tool(
                        &mut events,
                        str_field(block, "name").unwrap_or("unknown"),
                        str_field(block, "id"),
                        &state,
                        true,
                    );
                }
                _ => {}
            }
        }
    }
    Ok(events)
}

fn push_tool(events: &mut Vec<Event>, name: &str, call_id: Option<&str>, state: &Value, v2: bool) {
    let input = state.get("input").cloned().unwrap_or(Value::Null);
    let status = str_field(state, "status");
    events.push(Event::ToolCall {
        name: name.to_string(),
        input: input.clone(),
        call_id: call_id.map(str::to_string),
    });
    if status == Some("completed")
        && let Some(edit) = edit_from_tool(name, &input)
    {
        events.push(edit);
    }
    if status == Some("completed") || status == Some("error") {
        let is_error = status == Some("error");
        let output = if is_error {
            state
                .get("error")
                .map(|e| str_field(e, "message").map(str::to_string).unwrap_or_else(|| text_of(e)))
                .unwrap_or_default()
        } else {
            text_of(state.get(if v2 { "content" } else { "output" }).unwrap_or(&Value::Null))
        };
        events.push(Event::ToolResult {
            name: Some(name.to_string()),
            call_id: call_id.map(str::to_string),
            output,
            is_error,
        });
    }
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

fn push_thinking(events: &mut Vec<Event>, text: &str) {
    if !text.trim().is_empty() {
        events.push(Event::Thinking { text: text.to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_legacy_and_v2_sessions() {
        let dir = std::env::temp_dir().join(format!("ruddr-history-opencode-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                r#"
            CREATE TABLE session (id TEXT, directory TEXT, title TEXT, time_updated INTEGER, time_archived INTEGER);
            CREATE TABLE message (id TEXT, session_id TEXT, data TEXT, time_created INTEGER);
            CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, data TEXT, time_created INTEGER);
            CREATE TABLE session_v2 (id TEXT, directory TEXT, title TEXT, time_updated INTEGER, time_archived INTEGER);
            CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, data TEXT, seq INTEGER);
            INSERT INTO session VALUES ('old', '/w', 'legacy work', 10, NULL);
            INSERT INTO message VALUES ('m1', 'old', '{"role":"user"}', 1), ('m2', 'old', '{"role":"assistant"}', 2);
            INSERT INTO part VALUES ('p1', 'm1', 'old', '{"type":"text","text":"edit it"}', 1),
              ('p2', 'm2', 'old', '{"type":"tool","tool":"edit","callID":"c","state":{"status":"completed","input":{"filePath":"/w/a.rs","oldString":"a","newString":"b"},"output":"ok"}}', 2);
            INSERT INTO session_v2 VALUES ('new', '/w', 'v2 work', 20, NULL);
            INSERT INTO session_message VALUES ('s1', 'new', 'user', '{"text":"write it"}', 1),
              ('s2', 'new', 'assistant', '{"content":[{"type":"tool","name":"write","id":"w","state":{"status":"completed","input":{"filePath":"/w/n.rs","content":"hi"},"content":"done"}},{"type":"text","text":"written"}]}', 2);
            "#,
            )
            .unwrap();
        drop(connection);
        let sessions = list(&db, 10);
        let titles: Vec<&str> = sessions.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["v2 work", "legacy work"]);
        let legacy = events(&locator(&db, "old")).unwrap();
        assert!(
            legacy
                .iter()
                .any(|e| matches!(e, Event::FileChange { path, hunks, .. } if path == "/w/a.rs" && hunks.contains("+b")))
        );
        let v2 = events(&locator(&db, "new")).unwrap();
        assert!(v2.iter().any(|e| matches!(e, Event::FileChange { path, .. } if path == "/w/n.rs")));
        assert!(matches!(v2.last(), Some(Event::Assistant { text }) if text == "written"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
