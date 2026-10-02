//! Session discovery for the TUI, the web dashboard, and multi-run commands:
//! explicit state directories, `state.json` files below roots, and the global
//! registry. Live runs sort first, then the most recently updated.

use crate::state::{RunState, STATE_FILE, Status};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// Displayed state: a dead non-terminal controller reads as `stale`.
    pub state: RunState,
    pub state_file: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Discover {
    pub state_dirs: Vec<PathBuf>,
    pub roots: Vec<PathBuf>,
    /// `None` reads the default registries; `Some(vec![])` skips them.
    pub registries: Option<Vec<PathBuf>>,
}

pub fn discover(options: &Discover) -> Vec<Session> {
    let mut files = BTreeSet::new();
    for dir in &options.state_dirs {
        files.insert(crate::paths::absolute(dir).join(STATE_FILE));
    }
    for root in &options.roots {
        collect_state_files(&crate::paths::absolute(root), &mut files);
    }
    let registries = options.registries.clone().unwrap_or_else(crate::paths::registry_dirs_for_discovery);
    for dir in crate::registry::registered_in(&registries) {
        files.insert(crate::paths::absolute(&dir).join(STATE_FILE));
    }
    let mut sessions: Vec<Session> = files
        .into_iter()
        .filter_map(|file| {
            let dir = file.parent()?;
            let state = crate::state::read_state(dir).ok()?;
            Some(Session {
                state: state.displayed(),
                state_file: file,
            })
        })
        .collect();
    sort_sessions(&mut sessions);
    sessions
}

fn collect_state_files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if kind.is_file() && name == STATE_FILE {
            out.insert(entry.path());
        } else if kind.is_dir() && name != ".git" && name != "node_modules" {
            collect_state_files(&entry.path(), out);
        }
    }
}

fn rank(status: Status) -> u8 {
    match status {
        Status::Active => 0,
        Status::Idle => 1,
        Status::Starting | Status::Stopping => 2,
        Status::Stale => 4,
        _ => 3,
    }
}

pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        rank(a.state.status).cmp(&rank(b.state.status)).then_with(|| {
            let t = |s: &Session| crate::time::parse_rfc3339_ms(&s.state.updated_at).unwrap_or(0);
            t(b).cmp(&t(a))
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{STATE_VERSION, persist_state};

    fn state(dir: &Path, status: Status, updated: &str) -> RunState {
        RunState {
            version: STATE_VERSION,
            provider: "codex".into(),
            pid: std::process::id() as i64,
            child_pid: 0,
            status,
            thread_id: None,
            turn_id: None,
            model: String::new(),
            effort: None,
            cwd: String::new(),
            sandbox: String::new(),
            state_dir: dir.to_string_lossy().into_owned(),
            socket_path: String::new(),
            socket_dir: None,
            events_path: String::new(),
            trace_path: String::new(),
            output_path: String::new(),
            stderr_path: String::new(),
            steers: 0,
            idle: false,
            turns: 0,
            last_turn: None,
            token_usage: None,
            started_at: updated.into(),
            updated_at: updated.into(),
            completed_at: None,
            error: None,
        }
    }

    #[test]
    fn discovers_roots_and_registry_and_sorts_live_first() {
        let root = std::env::temp_dir().join(format!("ruddr-discover-{}", crate::fsutil::random_hex(4)));
        let done = root.join(".scratch/a");
        let live = root.join(".scratch/b");
        let elsewhere = root.join("elsewhere/c");
        for dir in [&done, &live, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        persist_state(&state(&done, Status::Completed, "2026-10-02T10:00:00Z")).unwrap();
        persist_state(&state(&live, Status::Active, "2026-10-01T10:00:00Z")).unwrap();
        persist_state(&state(&elsewhere, Status::Failed, "2026-10-02T11:00:00Z")).unwrap();
        let registry = root.join("registry");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(registry.join("x.run"), format!("{}\n", elsewhere.display())).unwrap();
        // A forged state.json that claims another directory is ignored.
        let forged = root.join(".scratch/forged");
        std::fs::create_dir_all(&forged).unwrap();
        std::fs::write(
            forged.join("state.json"),
            serde_json::to_vec(&state(&live, Status::Completed, "2026-10-02T12:00:00Z")).unwrap(),
        )
        .unwrap();

        let sessions = discover(&Discover {
            roots: vec![root.join(".scratch")],
            registries: Some(vec![registry]),
            ..Default::default()
        });
        let order: Vec<String> = sessions.iter().map(|s| s.state.status.to_string()).collect();
        assert_eq!(order, vec!["active", "failed", "completed"]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
