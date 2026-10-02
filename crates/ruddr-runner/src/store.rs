//! The controller's view of `state.json`: one in-memory copy, persisted
//! atomically on every change. Creating a store claims the state directory.

use crate::config::RunConfig;
use ruddr_core::state::{self, RunState, Status};
use ruddr_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct StateStore {
    state: Mutex<RunState>,
}

/// Artifacts whose presence means another run used the directory.
const RUN_ARTIFACTS: &[&str] = &[
    state::CLAIM_FILE,
    "state.json.tmp",
    state::EVENTS_FILE,
    state::TRACE_FILE,
    state::OUTPUT_FILE,
    "output.md.tmp",
    state::STDERR_FILE,
    ruddr_core::control::SOCKET_FILE,
];

impl StateStore {
    /// Creates the 0700 state directory, refuses one that holds an earlier
    /// run, picks the control socket location, and claims the directory with
    /// an exclusive `.ruddr.claim` before writing the first `state.json`.
    pub fn create(cfg: &RunConfig) -> Result<StateStore> {
        let provider = ruddr_core::provider::Provider::parse(&cfg.provider)?;
        let state_dir = ruddr_core::paths::absolute(&cfg.state_dir);
        ruddr_core::fsutil::create_private_dir(&state_dir).map_err(|e| Error::failed(format!("create {}: {e}", state_dir.display())))?;
        if std::fs::symlink_metadata(state_dir.join(state::STATE_FILE)).is_ok() {
            return match state::read_state(&state_dir) {
                Ok(existing) => Err(Error::failed(format!(
                    "state directory already contains a Ruddr run with status {}; use a new --state-dir",
                    existing.status
                ))),
                Err(e) => Err(e.context("read existing state")),
            };
        }
        for name in RUN_ARTIFACTS {
            match std::fs::symlink_metadata(state_dir.join(name)) {
                Ok(_) => {
                    return Err(Error::failed(format!(
                        "state directory already contains Ruddr artifact {name}; use a new --state-dir"
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::failed(format!("inspect state directory artifact {name}: {e}"))),
            }
        }
        let (socket_path, socket_dir) = control_socket_location(&state_dir)?;
        let now = ruddr_core::time::now_rfc3339();
        let dir = |name: &str| state_dir.join(name).to_string_lossy().into_owned();
        let initial = RunState {
            version: state::STATE_VERSION,
            provider: provider.as_str().into(),
            pid: std::process::id() as i64,
            child_pid: 0,
            status: Status::Starting,
            thread_id: None,
            turn_id: None,
            model: cfg.model.clone(),
            effort: (!cfg.effort.is_empty()).then(|| cfg.effort.clone()),
            cwd: cfg.cwd.to_string_lossy().into_owned(),
            sandbox: cfg.sandbox.clone(),
            state_dir: state_dir.to_string_lossy().into_owned(),
            socket_path,
            socket_dir: socket_dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
            events_path: dir(state::EVENTS_FILE),
            trace_path: dir(state::TRACE_FILE),
            output_path: dir(state::OUTPUT_FILE),
            stderr_path: dir(state::STDERR_FILE),
            steers: 0,
            idle: cfg.idle,
            turns: 0,
            last_turn: None,
            token_usage: None,
            started_at: now.clone(),
            updated_at: now,
            completed_at: None,
            error: None,
        };
        if let Some(hook) = &cfg.before_state_reserve {
            hook();
        }
        if let Err(e) = claim(&state_dir, &initial) {
            if let Some(dir) = &socket_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err(e);
        }
        if cfg.register_run {
            let _ = ruddr_core::registry::register(&state_dir);
        }
        Ok(StateStore {
            state: Mutex::new(initial),
        })
    }

    /// Wraps an existing state for unit tests that never touch the disk
    /// through `create`.
    #[cfg(test)]
    pub fn from_state(state: RunState) -> StateStore {
        StateStore { state: Mutex::new(state) }
    }

    /// Applies `change`, stamps `updatedAt`, and persists. The in-memory state
    /// changes only when the write succeeds.
    pub fn update(&self, change: impl FnOnce(&mut RunState)) -> Result<()> {
        let mut current = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = current.clone();
        change(&mut next);
        next.updated_at = ruddr_core::time::now_rfc3339();
        state::persist_state(&next)?;
        *current = next;
        Ok(())
    }

    pub fn snapshot(&self) -> RunState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn claim(state_dir: &Path, initial: &RunState) -> Result<()> {
    match ruddr_core::fsutil::create_private_file_new(&state_dir.join(state::CLAIM_FILE)) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::failed(
                "state directory was claimed by another Ruddr run; use a new --state-dir",
            ));
        }
        Err(e) => return Err(Error::failed(format!("reserve state directory: {e}"))),
    }
    ruddr_core::fsutil::set_mode(&state_dir.join(state::CLAIM_FILE), 0o600)?;
    state::persist_state(initial).map_err(|e| e.context("reserve state directory"))
}

/// Where the control channel listens. On Unix it is `.ruddr.sock` in the
/// state directory, or `control.sock` in a private temporary directory when
/// that path exceeds [`ruddr_core::control::MAX_SOCKET_PATH_BYTES`]; the
/// second value is that temporary directory. On Windows it is a named pipe.
pub fn control_socket_location(state_dir: &Path) -> Result<(String, Option<PathBuf>)> {
    if cfg!(windows) {
        return Ok((format!(r"\\.\pipe\ruddr-{}", ruddr_core::fsutil::random_hex(8)), None));
    }
    let inside = state_dir.join(ruddr_core::control::SOCKET_FILE);
    if inside.as_os_str().len() <= ruddr_core::control::MAX_SOCKET_PATH_BYTES {
        return Ok((inside.to_string_lossy().into_owned(), None));
    }
    let mut roots = vec![std::env::temp_dir()];
    if roots[0] != Path::new("/tmp") {
        roots.push(PathBuf::from("/tmp"));
    }
    for root in roots {
        for _ in 0..8 {
            let dir = root.join(format!("ruddr-{}", ruddr_core::fsutil::random_hex(6)));
            match ruddr_core::fsutil::create_private_dir_new(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => break,
            }
            let path = dir.join("control.sock");
            if path.as_os_str().len() <= ruddr_core::control::MAX_SOCKET_PATH_BYTES {
                return Ok((path.to_string_lossy().into_owned(), Some(dir)));
            }
            let _ = std::fs::remove_dir_all(&dir);
            break;
        }
    }
    Err(Error::failed(format!(
        "cannot create a private Unix socket path within {} bytes",
        ruddr_core::control::MAX_SOCKET_PATH_BYTES
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn config(state_dir: &Path) -> RunConfig {
        RunConfig {
            state_dir: state_dir.to_path_buf(),
            model: "test-model".into(),
            sandbox: "read-only".into(),
            cwd: std::env::temp_dir(),
            ..Default::default()
        }
    }

    #[test]
    fn state_is_private_and_redacted() {
        let dir = TempDir::new("store");
        std::fs::write(dir.join("prompt.md"), "TOP SECRET PROMPT").unwrap();
        let store = StateStore::create(&RunConfig {
            provider: "claude".into(),
            ..config(&dir.join("run"))
        })
        .unwrap();
        let state = store.snapshot();
        let raw = std::fs::read_to_string(state.state_path()).unwrap();
        assert!(!raw.contains("TOP SECRET"));
        assert_eq!(state.version, 2);
        assert_eq!(state.provider, "claude");
        assert!(state.stderr_path.ends_with("provider.stderr.log"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&state.state_path()), 0o600);
            assert_eq!(mode(&dir.join("run").join(state::CLAIM_FILE)), 0o600);
            assert_eq!(mode(&dir.join("run")), 0o700);
        }
    }

    #[test]
    fn refuses_directories_with_earlier_runs() {
        let dir = TempDir::new("reuse");
        let first = StateStore::create(&config(&dir)).unwrap();
        first.update(|s| s.status = Status::Completed).unwrap();
        let before = std::fs::read(first.snapshot().state_path()).unwrap();
        let error = StateStore::create(&config(&dir)).err().unwrap();
        assert!(
            error.message.contains("already contains a Ruddr run with status completed"),
            "{error}"
        );
        assert_eq!(std::fs::read(first.snapshot().state_path()).unwrap(), before);

        for name in [state::OUTPUT_FILE, state::CLAIM_FILE, "state.json.tmp", "output.md.tmp"] {
            let dir = TempDir::new("artifact");
            std::fs::write(dir.join(name), "preserve me\n").unwrap();
            let error = StateStore::create(&config(&dir)).err().unwrap();
            assert!(
                error.message.contains(&format!("already contains Ruddr artifact {name}")),
                "{error}"
            );
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), "preserve me\n");
        }
    }

    #[test]
    fn concurrent_creates_claim_once() {
        let dir = TempDir::new("claim");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["model-a", "model-b"]
            .into_iter()
            .map(|model| {
                let barrier = barrier.clone();
                let cfg = RunConfig {
                    model: model.into(),
                    before_state_reserve: Some(std::sync::Arc::new(move || {
                        barrier.wait();
                    })),
                    ..config(&dir)
                };
                std::thread::spawn(move || StateStore::create(&cfg).map(|s| s.snapshot().model))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(
            winners.len(),
            1,
            "{:?}",
            results
                .iter()
                .map(|r| r.as_ref().err().map(|e| e.message.clone()))
                .collect::<Vec<_>>()
        );
        let loser = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert!(loser.message.contains("claimed by another Ruddr run"), "{loser}");
        assert_eq!(&state::read_state(&dir).unwrap().model, winners[0]);
    }

    #[cfg(unix)]
    #[test]
    fn socket_location_is_private_and_short() {
        let short = TempDir::in_tmp("rr");
        let (path, dir) = control_socket_location(&short).unwrap();
        assert!(dir.is_none());
        assert_eq!(Path::new(&path).parent().unwrap(), &*short);

        let long = short.join("long-segment-".repeat(12));
        let (path, dir) = control_socket_location(&long).unwrap();
        let dir = dir.expect("a long state directory uses a private fallback");
        assert_eq!(Path::new(&path).parent().unwrap(), dir);
        assert!(path.len() <= ruddr_core::control::MAX_SOCKET_PATH_BYTES);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
