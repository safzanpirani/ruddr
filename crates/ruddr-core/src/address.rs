//! `ruddr://` addresses. A run's address is `ruddr://` plus its absolute state
//! directory (`ruddr:///home/me/.scratch/ruddr/fix-login`), so it stays valid
//! for as long as the run's files do. Every `--state-dir` flag accepts one.
//! The provider session a run drives has a dejavu address,
//! `dejavu://<provider>/<thread id>`, because Ruddr's provider names are
//! dejavu's harness names.

use std::path::Path;

pub const SCHEME: &str = "ruddr://";

/// The address of the run whose state lives in `state_dir`.
pub fn of_state_dir(state_dir: &Path) -> String {
    format!("{SCHEME}{}", crate::paths::absolute(state_dir).display())
}

/// A `--state-dir` value with a `ruddr://` prefix removed.
pub fn state_dir_arg(value: &str) -> &str {
    value.strip_prefix(SCHEME).unwrap_or(value)
}

/// The dejavu address of the provider session behind a run, once it has one.
pub fn session(provider: &str, thread_id: Option<&str>) -> Option<String> {
    thread_id.filter(|id| !id.is_empty()).map(|id| format!("dejavu://{provider}/{id}"))
}

/// Adds `address` and, when the run has a thread, `session` to a run's JSON.
pub fn annotate(value: &mut serde_json::Value, state_dir: &Path, provider: &str, thread_id: Option<&str>) {
    let Some(object) = value.as_object_mut() else { return };
    object.insert("address".into(), of_state_dir(state_dir).into());
    if let Some(session) = session(provider, thread_id) {
        object.insert("session".into(), session.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_round_trips_through_state_dir_flags() {
        let dir = std::env::temp_dir().join("ruddr-address-test");
        let address = of_state_dir(&dir);
        assert!(address.starts_with(SCHEME));
        assert_eq!(Path::new(state_dir_arg(&address)), crate::paths::absolute(&dir));
        assert_eq!(state_dir_arg("runs/a"), "runs/a");
    }

    #[test]
    fn session_needs_a_thread() {
        assert_eq!(session("codex", Some("th-1")).as_deref(), Some("dejavu://codex/th-1"));
        assert_eq!(session("codex", Some("")), None);
        assert_eq!(session("droid", None), None);
    }
}
