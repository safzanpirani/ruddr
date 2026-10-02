//! Unified diffs for rebuilt edits. Codex and Claude record real hunks;
//! other providers record only the replaced and replacement text, which is
//! diffed here as a fragment whose position in the file is unknown.

use crate::{ChangeKind, Event, Transcript};

/// Hunks for an old/new text pair, numbered from line 1 because the
/// fragment's place in its file is not recorded.
pub fn line_diff(old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = split_lines(old);
    let b: Vec<&str> = split_lines(new);
    let mut body = String::new();
    // A full LCS table for typical edit fragments; very large pairs fall back
    // to a plain replacement so a huge Write cannot stall the UI.
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        for line in &a {
            body.push_str(&format!("-{line}\n"));
        }
        for line in &b {
            body.push_str(&format!("+{line}\n"));
        }
    } else {
        let (n, m) = (a.len(), b.len());
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * (m + 1) + j] = if a[i] == b[j] {
                    lcs[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && a[i] == b[j] {
                body.push_str(&format!(" {}\n", a[i]));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1]) {
                // Deletions come before additions, as in git's output.
                body.push_str(&format!("-{}\n", a[i]));
                i += 1;
            } else {
                body.push_str(&format!("+{}\n", b[j]));
                j += 1;
            }
        }
    }
    let old_start = if a.is_empty() { 0 } else { 1 };
    let new_start = if b.is_empty() { 0 } else { 1 };
    format!("@@ -{old_start},{} +{new_start},{} @@\n{body}", a.len(), b.len())
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.strip_suffix('\n').unwrap_or(text).split('\n').collect()
    }
}

fn shown_path(path: &str, cwd: &str, home: &str) -> String {
    if let Some(inside) = (!cwd.is_empty()).then(|| path.strip_prefix(&format!("{cwd}/"))).flatten() {
        return inside.to_string();
    }
    match (!home.is_empty()).then(|| path.strip_prefix(&format!("{home}/"))).flatten() {
        Some(rest) => format!("~/{rest}"),
        None => path.to_string(),
    }
}

/// Every file change in the transcript as one git-style diff, grouped by
/// file in the order files were first touched. Paths inside the session's
/// working directory are shown relative to it, and other paths in the home
/// directory as `~/...`.
pub fn unified_diff(transcript: &Transcript) -> String {
    let home = ruddr_core::paths::home_dir();
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    let cwd = transcript.info.cwd.trim_end_matches('/');
    let mut order: Vec<String> = Vec::new();
    let mut by_file: std::collections::HashMap<String, (ChangeKind, String)> = std::collections::HashMap::new();
    for event in &transcript.events {
        let Event::FileChange { path, kind, hunks } = event else { continue };
        let shown = shown_path(path, cwd, home);
        let entry = by_file.entry(shown.clone()).or_insert_with(|| {
            order.push(shown.clone());
            (*kind, String::new())
        });
        if *kind == ChangeKind::Delete {
            entry.0 = ChangeKind::Delete;
        }
        entry.1.push_str(hunks);
        if !hunks.ends_with('\n') && !hunks.is_empty() {
            entry.1.push('\n');
        }
    }
    let mut out = String::new();
    for path in order {
        let (kind, hunks) = &by_file[&path];
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        match kind {
            ChangeKind::Add => out.push_str(&format!("new file mode 100644\n--- /dev/null\n+++ b/{path}\n")),
            ChangeKind::Delete => out.push_str(&format!("deleted file mode 100644\n--- a/{path}\n+++ /dev/null\n")),
            ChangeKind::Update => out.push_str(&format!("--- a/{path}\n+++ b/{path}\n")),
        }
        out.push_str(hunks);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Provider, SessionInfo};

    #[test]
    fn line_diff_keeps_context_and_marks_changes() {
        assert_eq!(line_diff("a\nb\nc\n", "a\nx\nc\n"), "@@ -1,3 +1,3 @@\n a\n-b\n+x\n c\n");
        assert_eq!(line_diff("", "new\n"), "@@ -0,0 +1,1 @@\n+new\n");
        assert_eq!(line_diff("same", "same"), "");
    }

    #[test]
    fn paths_outside_the_workspace_stay_recognizable() {
        assert_eq!(shown_path("/w/src/a.rs", "/w", "/home/u"), "src/a.rs");
        assert_eq!(shown_path("/home/u/notes.md", "/w", "/home/u"), "~/notes.md");
        assert_eq!(shown_path("/tmp/x.sh", "/w", "/home/u"), "/tmp/x.sh");
        assert_eq!(shown_path("/w2/a.rs", "/w", "/home/u"), "/w2/a.rs");
    }

    #[test]
    fn unified_diff_groups_by_file_relative_to_cwd() {
        let info = SessionInfo {
            provider: Provider::Pi,
            locator: String::new(),
            id: String::new(),
            cwd: "/w".into(),
            title: String::new(),
            updated_ms: 0,
        };
        let events = vec![
            Event::FileChange {
                path: "/w/src/a.rs".into(),
                kind: ChangeKind::Update,
                hunks: line_diff("1\n", "2\n"),
            },
            Event::FileChange {
                path: "/w/new.md".into(),
                kind: ChangeKind::Add,
                hunks: line_diff("", "hi\n"),
            },
            Event::FileChange {
                path: "/w/src/a.rs".into(),
                kind: ChangeKind::Update,
                hunks: line_diff("3\n", "4\n"),
            },
        ];
        let diff = unified_diff(&Transcript { info, events });
        assert_eq!(diff.matches("diff --git").count(), 2);
        assert!(
            diff.starts_with(
                "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n-1\n+2\n@@ -1,1 +1,1 @@\n-3\n+4\n"
            ),
            "{diff}"
        );
        assert!(diff.contains("new file mode 100644\n--- /dev/null\n+++ b/new.md\n@@ -0,0 +1,1 @@\n+hi\n"));
    }
}
