//! `run --request-id`: a retry-safe launch. The key names the default state
//! directory, `CWD/.scratch/ruddr/req-KEY`, and `state.json` records it, so
//! a launch repeated after a lost response or a tool timeout finds the run
//! the first launch started and reports it instead of starting a second.

use ruddr_core::state::{self, RunState, Status};
use ruddr_core::{Error, Result};
use std::path::{Path, PathBuf};

const MAX_LEN: usize = 64;

/// Accepts 1 to 64 ASCII letters, digits, `.`, `-`, and `_`, not starting
/// with `.`, so the key is safe as a directory name on every platform.
pub fn checked_request_id(value: &str) -> Result<String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_LEN
        && !value.starts_with('.')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !valid {
        return Err(Error::usage(format!(
            "invalid --request-id {value:?}: use 1-{MAX_LEN} letters, digits, '.', '-', or '_', not starting with '.'"
        )));
    }
    Ok(value.into())
}

/// The default state directory for a request: `CWD/.scratch/ruddr/req-KEY`.
pub fn request_state_dir(cwd: &Path, key: &str) -> Result<PathBuf> {
    let base = ruddr_core::paths::default_runs_dir(&ruddr_core::paths::absolute(cwd));
    ruddr_core::paths::ensure_ignored_runs_dir(&base).map_err(|e| Error::failed(format!("choose a state directory: {e}")))?;
    Ok(base.join(format!("req-{key}")))
}

/// The run an earlier launch with `key` started in `state_dir`, with a dead
/// controller shown as stale. `None` when the directory holds no run yet.
/// A run started without `key`, or with another key, is an error.
pub fn existing_run(state_dir: &Path, key: &str) -> Result<Option<RunState>> {
    if std::fs::symlink_metadata(state_dir.join(state::STATE_FILE)).is_err() {
        return Ok(None);
    }
    let existing = state::read_state(state_dir).map_err(|e| e.context("read existing state"))?;
    if existing.request_id.as_deref() != Some(key) {
        return Err(Error::failed(format!(
            "state directory {} holds a run with a different request ID; use a new --state-dir or --request-id",
            state_dir.display()
        )));
    }
    Ok(Some(existing.displayed()))
}

/// Reports an existing run with the exit status its own launch would give.
/// A detached launch prints its usual `detached run:` line and succeeds
/// unless the run failed or went stale. A foreground launch exits 0 for a
/// completed run and 3 for one still running, since it does not wait.
pub fn report_existing(run: &RunState, key: &str, detach: bool) -> Result<()> {
    eprintln!("ruddr: request-id {key} already started the run in {}", run.state_dir);
    let ended = |status: Status| format!("run ended with status {status}");
    match run.status {
        Status::Stale => Err(Error::stale(format!(
            "{} (state dir {})",
            run.error.clone().unwrap_or_else(|| "run is stale".into()),
            run.state_dir
        ))),
        Status::Failed | Status::Interrupted => Err(Error::failed(format!(
            "{} (state dir {})",
            run.error.clone().unwrap_or_else(|| ended(run.status)),
            run.state_dir
        ))),
        status if detach => {
            println!("detached run: state-dir={} pid={} status={status}", run.state_dir, run.pid);
            Ok(())
        }
        Status::Completed => Ok(()),
        status => Err(Error::running(format!(
            "run is still {status} (state dir {}); follow it with ruddr wait",
            run.state_dir
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use ruddr_core::Exit;

    #[test]
    fn request_ids_are_safe_directory_names() {
        for good in ["a", "task-7", "A.b_c-9", &"x".repeat(MAX_LEN)] {
            assert_eq!(checked_request_id(good).unwrap(), good);
        }
        for bad in ["", ".hidden", "a/b", "a\\b", "..", "sp ace", "ü", &"x".repeat(MAX_LEN + 1)] {
            assert_eq!(checked_request_id(bad).unwrap_err().exit, Exit::Usage, "{bad:?}");
        }
    }

    /// A run state whose controller (this test process) is alive.
    fn state(dir: &Path, status: Status, request_id: Option<&str>) -> RunState {
        let mut run: RunState = serde_json::from_value(serde_json::json!({
            "version": state::STATE_VERSION, "pid": std::process::id(), "status": status,
            "stateDir": dir, "startedAt": "2026-10-07T00:00:00Z", "updatedAt": "2026-10-07T00:00:00Z",
        }))
        .unwrap();
        run.request_id = request_id.map(str::to_string);
        run
    }

    fn persisted(dir: &Path, status: Status, request_id: Option<&str>) {
        state::persist_state(&state(dir, status, request_id)).unwrap();
    }

    #[test]
    fn existing_runs_match_their_request_id() {
        let dir = TempDir::new("request");
        assert!(existing_run(&dir, "k").unwrap().is_none());
        persisted(&dir, Status::Completed, Some("k"));
        assert_eq!(existing_run(&dir, "k").unwrap().unwrap().status, Status::Completed);
        assert!(existing_run(&dir, "other").unwrap_err().message.contains("different request ID"));
        persisted(&dir, Status::Completed, None);
        assert!(existing_run(&dir, "k").is_err());
        assert_eq!(
            request_state_dir(&dir, "k").unwrap(),
            dir.join(".scratch").join("ruddr").join("req-k")
        );
    }

    #[test]
    fn existing_runs_report_the_launch_exit_status() {
        let dir = TempDir::new("request-exit");
        let run = |status| state(&dir, status, Some("k"));
        assert!(report_existing(&run(Status::Completed), "k", false).is_ok());
        assert!(report_existing(&run(Status::Active), "k", true).is_ok());
        assert!(report_existing(&run(Status::Completed), "k", true).is_ok());
        assert_eq!(report_existing(&run(Status::Active), "k", false).unwrap_err().exit, Exit::Running);
        assert_eq!(report_existing(&run(Status::Idle), "k", false).unwrap_err().exit, Exit::Running);
        for detach in [false, true] {
            assert_eq!(report_existing(&run(Status::Failed), "k", detach).unwrap_err().exit, Exit::Failed);
            assert_eq!(
                report_existing(&run(Status::Interrupted), "k", detach).unwrap_err().exit,
                Exit::Failed
            );
            assert_eq!(report_existing(&run(Status::Stale), "k", detach).unwrap_err().exit, Exit::Stale);
        }
    }
}
