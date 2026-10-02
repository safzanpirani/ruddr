//! `ruddr run` as the CLI calls it: flag parsing, the default state
//! directory, registry registration, and the rules for a custom app-server
//! command. These tests set RUDDR_REGISTRY_DIR, so they live in their own
//! test binary and run one at a time.

use ruddr_core::Exit;
use ruddr_core::state::{self, Status};
use ruddr_runner::run_command;
use std::path::PathBuf;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn fake_app_server() -> String {
    let exe = std::env::current_exe().unwrap();
    let path = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(format!("fake_app_server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "missing {}; build it with `mbx test -p ruddr-runner`",
        path.display()
    );
    path.to_string_lossy().into_owned()
}

struct Workspace {
    root: PathBuf,
    registry: PathBuf,
}

impl Workspace {
    fn new() -> Workspace {
        let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let root = base.join(format!("ruddr-cmd-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("prompt.md"), "task").unwrap();
        let registry = root.join("registry");
        // SAFETY: SERIAL keeps the tests in this binary from racing on the environment.
        unsafe {
            std::env::set_var("RUDDR_REGISTRY_DIR", &registry);
            // Defaults come from the built-in catalog, not the user's models.json.
            std::env::set_var("RUDDR_MODELS_FILE", root.join("models.json"));
        }
        Workspace { root, registry }
    }

    fn arg(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("RUDDR_REGISTRY_DIR");
            std::env::remove_var("RUDDR_MODELS_FILE");
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| a.to_string()).collect()
}

#[test]
fn a_run_without_state_dir_uses_an_ignored_default_and_registers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let ws = Workspace::new();
    let fake = fake_app_server();
    let prompt = ws.arg("prompt.md");
    let cwd = ws.root.to_string_lossy().into_owned();
    run_command(args(&[
        "--prompt-file",
        &prompt,
        "--cwd",
        &cwd,
        "--sandbox",
        "read-only",
        "--",
        &fake,
        "--complete-on-start",
    ]))
    .unwrap();

    let base = ws.root.join(".scratch").join("ruddr");
    assert_eq!(std::fs::read_to_string(base.join(".gitignore")).unwrap(), "*\n");
    let runs: Vec<PathBuf> = std::fs::read_dir(&base)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = state::read_state(&runs[0]).unwrap();
    assert_eq!(run.status, Status::Completed);
    assert_eq!(run.model, "gpt-6-astra", "Codex runs use the catalog default");
    let registered = ruddr_core::registry::registered_in(std::slice::from_ref(&ws.registry));
    assert_eq!(registered.len(), 1);
    assert_eq!(
        std::fs::canonicalize(&registered[0]).unwrap(),
        std::fs::canonicalize(&runs[0]).unwrap()
    );
}

#[test]
fn a_custom_command_needs_the_separator() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let ws = Workspace::new();
    let fake = fake_app_server();
    let prompt = ws.arg("prompt.md");
    let bare = run_command(args(&["--prompt-file", &prompt, "--state-dir", &ws.arg("bare"), &fake])).unwrap_err();
    assert!(bare.message.contains("after --"), "{bare}");
    let empty = run_command(args(&["--prompt-file", &prompt, "--state-dir", &ws.arg("empty"), "--"])).unwrap_err();
    assert!(empty.message.contains("after -- is empty"), "{empty}");
    run_command(args(&[
        "--prompt-file",
        &prompt,
        "--state-dir",
        &ws.arg("explicit"),
        "--model",
        "m",
        "--",
        &fake,
        "--complete-on-start",
    ]))
    .unwrap();
    assert_eq!(state::read_state(&ws.root.join("explicit")).unwrap().status, Status::Completed);
}

#[test]
fn usage_errors_exit_two() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let ws = Workspace::new();
    let missing = run_command(args(&["--state-dir", &ws.arg("run")])).unwrap_err();
    assert_eq!((missing.exit, missing.message.as_str()), (Exit::Usage, "--prompt-file is required"));
    assert_eq!(run_command(args(&["--bogus"])).unwrap_err().exit, Exit::Usage);
    assert_eq!(run_command(args(&["-prompt-file", "x"])).unwrap_err().exit, Exit::Usage);
    let provider = run_command(args(&["--prompt-file", "x", "--provider", "other"])).unwrap_err();
    assert_eq!(provider.exit, Exit::Usage);
    let config = run_command(args(&["--prompt-file", "x", "--config", "a=b", "--", "broker"])).unwrap_err();
    assert_eq!(config.exit, Exit::Usage);
    run_command(args(&["--help"])).unwrap();
    assert!(!ws.root.join("run").exists(), "a usage error created the state directory");
}
