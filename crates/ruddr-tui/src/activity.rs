//! The activity feed: `trace.log` lines folded incrementally into semantic
//! rows (thoughts, tool runs with durations, messages, warnings), joined to
//! the tool details from `events.jsonl` so a tool row can expand to show its
//! command, status, input, and output. Mirrors parseTraceActivities and
//! attachToolDetails in tui/core.ts.

use crate::transcript::{ToolDetail, ToolStatus, clean_thought};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Thought,
    Tool,
    Message,
    Warning,
    Error,
    Status,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    /// Stable across appends: the render cache keys on it.
    pub uid: u64,
    pub version: u64,
    pub timestamp: String,
    pub kind: ActivityKind,
    pub text: String,
    pub label: Option<String>,
    pub tool_status: Option<ToolStatus>,
    pub duration_ms: Option<i64>,
}

/// How many activity rows the feed shows.
pub const HISTORY_LIMIT: usize = 200;
/// How many activities stay in memory.
const KEEP: usize = 2000;

#[derive(Debug, Default)]
pub struct Activities {
    items: Vec<Activity>,
    clock: u64,
    pub generation: u64,
}

/// Splits `TIMESTAMP [tag] text`.
fn split_trace_line(line: &str) -> Option<(&str, &str, &str)> {
    let (timestamp, rest) = line.split_once(' ')?;
    if timestamp.is_empty() {
        return None;
    }
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    let tag = &rest[..close];
    if tag.is_empty() {
        return None;
    }
    let after = &rest[close + 1..];
    let text = if after.is_empty() { "" } else { after.strip_prefix(' ')? };
    Some((timestamp, tag, text))
}

/// Turns `$ /bin/zsh -lc 'go test'` into ("shell", "go test").
pub fn parse_tool(text: &str) -> (String, String) {
    if text == "file changes" {
        return ("files".into(), "changed".into());
    }
    let command = text.strip_prefix("$ ").unwrap_or(text);
    for shell in ["/bin/zsh", "/bin/bash", "/bin/sh", "zsh", "bash", "sh"] {
        if let Some(script) = command.strip_prefix(shell).and_then(|r| r.strip_prefix(" -lc ")) {
            if script.is_empty() {
                break;
            }
            let quote = script.chars().next().unwrap();
            let mut script = if quote == '"' || quote == '\'' { &script[1..] } else { script };
            if let Some(stripped) = script.strip_suffix(quote) {
                script = stripped;
            }
            return ("shell".into(), script.to_string());
        }
    }
    let executable = command.split_whitespace().next().unwrap_or("");
    let base = executable.rsplit(['/', '\\']).next().unwrap_or("");
    (if base.is_empty() { "tool".into() } else { base.to_string() }, command.to_string())
}

impl Activities {
    #[cfg(test)]
    pub fn items(&self) -> &[Activity] {
        &self.items
    }

    fn stamp(&mut self) -> u64 {
        self.clock += 1;
        self.generation += 1;
        self.clock
    }

    fn push(&mut self, timestamp: &str, kind: ActivityKind, text: String, label: Option<String>, tool_status: Option<ToolStatus>) {
        let stamp = self.stamp();
        self.items.push(Activity {
            uid: stamp,
            version: stamp,
            timestamp: timestamp.into(),
            kind,
            text,
            label,
            tool_status,
            duration_ms: None,
        });
        if self.items.len() > KEEP {
            self.items.drain(..self.items.len() - KEEP);
        }
    }

    /// Folds one `trace.log` line in. Returns true when the feed changed.
    pub fn apply_line(&mut self, line: &str) -> bool {
        let Some((timestamp, tag, raw)) = split_trace_line(line) else {
            return false;
        };
        match tag {
            "usage" => return false,
            "think" => {
                let text = clean_thought(raw);
                if text.is_empty() {
                    return false;
                }
                self.push(timestamp, ActivityKind::Thought, text, None, None);
            }
            "say" => {
                if raw.is_empty() {
                    return false;
                }
                self.push(timestamp, ActivityKind::Message, raw.into(), None, None);
            }
            "in_progress" | "completed" | "failed" => {
                let status = match tag {
                    "in_progress" => ToolStatus::Running,
                    "completed" => ToolStatus::Completed,
                    _ => ToolStatus::Failed,
                };
                let (label, text) = parse_tool(raw);
                if status != ToolStatus::Running {
                    let running = self.items.iter().rposition(|a| {
                        a.kind == ActivityKind::Tool
                            && a.tool_status == Some(ToolStatus::Running)
                            && a.label.as_deref() == Some(&label)
                            && a.text == text
                    });
                    if let Some(index) = running {
                        let started = crate::core::parse_time(&self.items[index].timestamp);
                        let finished = crate::core::parse_time(timestamp);
                        let stamp = self.stamp();
                        let item = &mut self.items[index];
                        item.timestamp = timestamp.into();
                        item.tool_status = Some(status);
                        item.duration_ms = started.zip(finished).map(|(s, f)| (f - s).max(0));
                        item.version = stamp;
                        return true;
                    }
                }
                self.push(timestamp, ActivityKind::Tool, text, Some(label), Some(status));
            }
            "warn" => self.push(timestamp, ActivityKind::Warning, raw.into(), None, None),
            "error" => self.push(timestamp, ActivityKind::Error, raw.into(), None, None),
            other => self.push(timestamp, ActivityKind::Status, raw.into(), Some(other.into()), None),
        }
        true
    }

    /// The rows to show: one latest message (the full commentary update when
    /// the events carry one), capped at [`HISTORY_LIMIT`].
    pub fn view(&self, commentary: Option<&str>) -> Vec<Activity> {
        let latest_message = self.items.iter().rposition(|a| a.kind == ActivityKind::Message);
        let mut rows: Vec<Activity> = self
            .items
            .iter()
            .enumerate()
            .filter(|(i, a)| a.kind != ActivityKind::Message || (commentary.is_none() && Some(*i) == latest_message))
            .map(|(_, a)| a.clone())
            .collect();
        if let Some(text) = commentary {
            // Uid 0 never collides with a trace row; the version tracks the text.
            let version = text.len() as u64 ^ (text.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64)) << 1);
            rows.push(Activity {
                uid: 0,
                version,
                timestamp: String::new(),
                kind: ActivityKind::Message,
                text: text.into(),
                label: None,
                tool_status: None,
                duration_ms: None,
            });
        }
        let start = rows.len().saturating_sub(HISTORY_LIMIT);
        rows.drain(..start);
        rows
    }
}

/// Pairs each tool row with the event detail that describes it. Each detail
/// is used once; sub-agent rows match by timestamp because their labels are
/// all alike.
pub fn attach_details(activities: &[Activity], details: &[ToolDetail]) -> Vec<Option<usize>> {
    let mut used = vec![false; details.len()];
    activities
        .iter()
        .map(|activity| {
            if activity.kind != ActivityKind::Tool {
                return None;
            }
            let summary = activity.text.trim_end_matches('…').to_lowercase();
            let label = activity.label.as_deref().unwrap_or("");
            let label_lower = label.to_lowercase();
            let candidates: Vec<usize> = details
                .iter()
                .enumerate()
                .filter(|(i, detail)| {
                    if used[*i] {
                        return false;
                    }
                    let text = format!(
                        "{} {} {}",
                        detail.command.as_deref().unwrap_or(""),
                        detail.query.as_deref().unwrap_or(""),
                        detail.tool_name.as_deref().unwrap_or("")
                    )
                    .to_lowercase();
                    if !summary.is_empty() && (text.contains(&summary) || summary.contains(text.trim())) {
                        return true;
                    }
                    if label == "files" {
                        return detail.kind == "fileChange";
                    }
                    if label_lower.contains("websearch") {
                        return detail.kind == "webSearch";
                    }
                    if label_lower == "subagentactivity" {
                        return detail.kind == "subAgentActivity";
                    }
                    label == "shell" && detail.kind == "commandExecution"
                })
                .map(|(i, _)| i)
                .collect();
            let chosen = if label_lower == "subagentactivity" {
                let time = crate::core::parse_time(&activity.timestamp);
                candidates
                    .iter()
                    .filter_map(|&i| {
                        let distance = (details[i].timestamp_ms? - time?).abs();
                        (distance < 2000).then_some((distance, i))
                    })
                    .min()
                    .map(|(_, i)| i)
            } else {
                candidates.first().copied()
            };
            if let Some(i) = chosen {
                used[i] = true;
            }
            chosen
        })
        .collect()
}

/// Search text for a row: every field a user might look for.
pub fn search_text(activity: &Activity, detail: Option<&ToolDetail>) -> String {
    let mut parts: Vec<String> = vec![activity.label.clone().unwrap_or_default(), activity.text.clone()];
    if let Some(status) = activity.tool_status {
        parts.push(status.as_str().into());
    }
    if let Some(d) = detail {
        for field in [
            &d.command,
            &d.cwd,
            &d.output,
            &d.query,
            &d.tool_name,
            &d.agent_thread_id,
            &d.agent_path,
            &d.activity_kind,
        ]
        .into_iter()
        .flatten()
        {
            parts.push(field.clone());
        }
        parts.push(d.status().as_str().into());
        if let Some(input) = &d.input {
            parts.push(input.to_string());
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" ")
}

/// What `c` copies for a row.
pub fn copy_text(activity: &Activity, detail: Option<&ToolDetail>) -> String {
    let Some(d) = detail else { return activity.text.clone() };
    let mut parts = vec![
        d.command
            .clone()
            .or_else(|| d.query.clone())
            .unwrap_or_else(|| activity.text.clone()),
        format!(
            "status: {}{}",
            d.status().as_str(),
            d.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default()
        ),
    ];
    if let Some(cwd) = &d.cwd {
        parts.push(format!("cwd: {cwd}"));
    }
    if let Some(thread) = &d.agent_thread_id {
        parts.push(format!("thread: {thread}"));
    }
    if let Some(output) = &d.output {
        parts.push(output.clone());
    }
    parts.retain(|p| !p.is_empty());
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(lines: &[&str]) -> Activities {
        let mut a = Activities::default();
        for line in lines {
            a.apply_line(line);
        }
        a
    }

    #[test]
    fn turns_trace_lines_into_a_semantic_feed() {
        let a = feed(&[
            "2026-08-25T18:00:00Z [think] **Inspecting tests** **Planning fix**",
            "2026-08-25T18:00:01Z [in_progress] $ /bin/zsh -lc 'go test ./...'",
            "2026-08-25T18:00:03Z [completed] $ /bin/zsh -lc 'go test ./...'",
            "2026-08-25T18:00:03Z [usage] updated",
            "2026-08-25T18:00:03Z [say] Still working.",
            "2026-08-25T18:00:04Z [in_progress] file changes",
            "2026-08-25T18:00:05Z [say] Tests are green.",
            "2026-08-25T18:00:06Z [in_progress] $ /bin/zsh -lc 'rg -n \"needle\" very-long-path…",
        ]);
        let view = a.view(None);
        type Summary<'a> = (ActivityKind, &'a str, Option<&'a str>, Option<ToolStatus>, Option<i64>);
        let summary: Vec<Summary> = view
            .iter()
            .map(|a| (a.kind, a.text.as_str(), a.label.as_deref(), a.tool_status, a.duration_ms))
            .collect();
        assert_eq!(
            summary,
            vec![
                (ActivityKind::Thought, "Inspecting tests · Planning fix", None, None, None),
                (
                    ActivityKind::Tool,
                    "go test ./...",
                    Some("shell"),
                    Some(ToolStatus::Completed),
                    Some(2000)
                ),
                (ActivityKind::Tool, "changed", Some("files"), Some(ToolStatus::Running), None),
                (ActivityKind::Message, "Tests are green.", None, None, None),
                (
                    ActivityKind::Tool,
                    "rg -n \"needle\" very-long-path…",
                    Some("shell"),
                    Some(ToolStatus::Running),
                    None
                ),
            ]
        );
        assert_eq!(view[1].timestamp, "2026-08-25T18:00:03Z");
    }

    #[test]
    fn commentary_replaces_trace_messages() {
        let a = feed(&["2026-08-25T18:00:03Z [say] short", "2026-08-25T18:00:04Z [warn] careful"]);
        let view = a.view(Some("Full latest update"));
        assert_eq!(view.len(), 2);
        assert_eq!(view[0].kind, ActivityKind::Warning);
        assert_eq!(view[1].text, "Full latest update");
    }

    #[test]
    fn completion_updates_the_running_row_in_place() {
        let mut a = feed(&["2026-08-25T18:00:01Z [in_progress] $ ls"]);
        let (uid, version) = (a.items()[0].uid, a.items()[0].version);
        assert!(a.apply_line("2026-08-25T18:00:02Z [failed] $ ls"));
        assert_eq!(a.items().len(), 1);
        assert_eq!(a.items()[0].uid, uid);
        assert!(a.items()[0].version > version);
        assert_eq!(a.items()[0].label.as_deref(), Some("ls"));
        assert!(!a.apply_line("garbage"));
    }

    #[test]
    fn matches_sub_agent_details_by_time() {
        let a = feed(&[
            "2026-08-28T18:45:21Z [completed] subAgentActivity",
            "2026-08-28T18:45:25Z [completed] subAgentActivity",
            "2026-08-28T18:47:22Z [completed] subAgentActivity",
            "2026-08-28T18:47:27Z [completed] subAgentActivity",
        ]);
        let detail = |id: &str, at: &str| ToolDetail {
            id: id.into(),
            kind: "subAgentActivity".into(),
            status: Some(ToolStatus::Completed),
            tool_name: Some("subAgentActivity".into()),
            timestamp_ms: crate::core::parse_time(at),
            ..Default::default()
        };
        let details = [
            detail("interacted", "2026-08-28T18:47:22.964Z"),
            detail("completed", "2026-08-28T18:47:27.417Z"),
        ];
        let attached = attach_details(&a.view(None), &details);
        assert_eq!(attached, vec![None, None, Some(0), Some(1)]);
    }

    #[test]
    fn matches_shell_rows_to_command_details() {
        let a = feed(&["2026-08-25T18:00:01Z [completed] $ /bin/zsh -lc 'go test ./...'"]);
        let details = [ToolDetail {
            id: "c".into(),
            kind: "commandExecution".into(),
            command: Some("/bin/zsh -lc 'go test ./...'".into()),
            output: Some("ok".into()),
            ..Default::default()
        }];
        let view = a.view(None);
        let attached = attach_details(&view, &details);
        assert_eq!(attached, vec![Some(0)]);
        assert!(copy_text(&view[0], Some(&details[0])).ends_with("\nok"));
        assert!(search_text(&view[0], Some(&details[0])).contains("ok"));
    }
}
