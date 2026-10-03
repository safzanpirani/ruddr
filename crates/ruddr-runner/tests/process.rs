//! End-to-end tests in real processes: `examples/ruddr_run.rs` stands in for
//! the `ruddr` binary and `examples/fake_app_server.rs` for the provider.
//! They cover what in-process tests cannot: signal handling, `--detach`
//! (its own session, the startup wait, launch.stderr.log), and
//! `--prompt-file -`.

use ruddr_core::control::{self, Command, Request};
use ruddr_core::state::{self, RunState, Status};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

fn example(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "missing {}; build it with `mbx test -p ruddr-runner`",
        path.display()
    );
    path
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let root = base.join(format!("ruddr-proc-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("prompt.md"), "task from a file").unwrap();
        Fixture { root }
    }

    fn path(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// `ruddr_run run` with the common flags, `extra` flags, and the fake
    /// app-server with `fake` flags after `--`.
    fn command(&self, extra: &[&str], fake: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(example("ruddr_run"));
        command
            .arg("run")
            .args(["--state-dir", &self.path("run"), "--model", "test-model", "--sandbox", "read-only"]);
        command
            .args(extra)
            .arg("--")
            .arg(example("fake_app_server"))
            .args(["--request-log", &self.path("requests.jsonl")])
            .args(fake);
        // Defaults come from the built-in catalog, and nothing reaches the
        // user's registry.
        command
            .env("RUDDR_MODELS_FILE", self.path("models.json"))
            .env("RUDDR_REGISTRY_DIR", self.path("registry"));
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn wait_state(&self, ready: impl Fn(&RunState) -> bool) -> RunState {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(state) = state::read_state(&self.state_dir())
                && ready(&state)
            {
                return state;
            }
            assert!(Instant::now() < deadline, "the run never reached the expected state");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn turn_start_text(&self) -> String {
        let log = std::fs::read_to_string(self.path("requests.jsonl")).unwrap();
        let line = log.lines().find(|l| l.contains("\"turn/start\"")).unwrap();
        let request: serde_json::Value = serde_json::from_str(line).unwrap();
        request["params"]["input"][0]["text"].as_str().unwrap().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wait_output(mut child: Child, limit: Duration) -> Output {
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "the process did not exit within {limit:?}: {:?}",
                child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_dead(pid: i64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while ruddr_core::process::alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !ruddr_core::process::alive(pid)
}

#[cfg(unix)]
fn signal(pid: u32, name: &str) {
    assert!(
        std::process::Command::new("kill")
            .args([&format!("-{name}"), &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
fn signals_interrupt_the_run_and_end_the_provider_tree() {
    for name in ["TERM", "INT"] {
        let fx = Fixture::new();
        let grandchild_file = fx.path("grandchild.pid");
        let child = fx
            .command(
                &["--prompt-file", &fx.path("prompt.md")],
                &["--grandchild-pid-file", &grandchild_file, "--term-ignoring-grandchild"],
            )
            .spawn()
            .unwrap();
        let controller = child.id();
        let active = fx.wait_state(|s| s.status == Status::Active);
        assert_eq!(active.pid, controller as i64);
        let grandchild: i64 = std::fs::read_to_string(&grandchild_file).unwrap().trim().parse().unwrap();
        signal(controller, name);
        let output = wait_output(child, Duration::from_secs(10));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "SIG{name}: {stderr}");
        assert!(stderr.contains("interrupted"), "SIG{name}: {stderr}");
        let state = state::read_state(&fx.state_dir()).unwrap();
        assert_eq!(state.status, Status::Interrupted, "SIG{name}");
        assert!(wait_dead(active.child_pid), "SIG{name}: provider {} survived", active.child_pid);
        let grandchild_dead = wait_dead(grandchild);
        if !grandchild_dead {
            let _ = std::process::Command::new("kill").args(["-9", &grandchild.to_string()]).status();
        }
        assert!(grandchild_dead, "SIG{name}: the TERM-ignoring grandchild survived");
        assert!(
            std::fs::symlink_metadata(&state.socket_path).is_err(),
            "SIG{name}: the socket was left behind"
        );
    }
}

#[test]
fn a_detached_run_lives_on_after_the_launcher_returns() {
    let fx = Fixture::new();
    let launcher = fx
        .command(&["--detach", "--idle", "--prompt-file", &fx.path("prompt.md")], &["--multi-turn"])
        .spawn()
        .unwrap();
    let launcher_pid = launcher.id();
    let output = wait_output(launcher, Duration::from_secs(20));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout} {}", String::from_utf8_lossy(&output.stderr));
    assert!(
        stdout.starts_with(&format!("detached run: state-dir={} pid=", fx.state_dir().display())),
        "{stdout}"
    );
    let state = state::read_state(&fx.state_dir()).unwrap();
    assert_ne!(state.pid, launcher_pid as i64);
    assert!(ruddr_core::process::alive(state.pid), "the detached controller is not running");
    #[cfg(unix)]
    {
        // SAFETY: getsid only reads the session of a live process.
        let session = unsafe { libc::getsid(state.pid as libc::pid_t) };
        assert_eq!(session as i64, state.pid, "the detached controller leads its own session");
        use std::os::unix::fs::PermissionsExt;
        let log = fx.state_dir().join(ruddr_runner::detach::LAUNCH_STDERR_FILE);
        assert_eq!(std::fs::metadata(log).unwrap().permissions().mode() & 0o777, 0o600);
    }
    fx.wait_state(|s| s.status == Status::Idle);
    let stop = Request {
        command: Command::Stop,
        images: vec![],
        text: None,
        expected_turn_id: None,
    };
    assert!(control::send(&fx.state_dir(), &stop, Duration::from_secs(10)).unwrap().ok);
    let done = fx.wait_state(|s| s.status.is_terminal());
    assert_eq!(done.status, Status::Completed);
    assert!(wait_dead(done.pid));
    assert_eq!(fx.turn_start_text(), "task from a file");
}

#[test]
fn a_detached_startup_failure_reports_the_controller_stderr() {
    let fx = Fixture::new();
    // The launcher accepts any sandbox name; the detached controller rejects
    // it, and the later flag overrides the fixture's read-only.
    let launcher = fx
        .command(
            &["--detach", "--prompt-file", &fx.path("prompt.md"), "--sandbox", "yolo"],
            &["--complete-on-start"],
        )
        .spawn()
        .unwrap();
    let output = wait_output(launcher, Duration::from_secs(20));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("unsupported sandbox \"yolo\"") && stderr.contains("(state dir"),
        "{stderr}"
    );
}

#[test]
fn prompts_can_come_from_stdin() {
    let fx = Fixture::new();
    let mut child = fx
        .command(&["--prompt-file", "-"], &["--complete-on-start"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"task from stdin\n").unwrap();
    let output = wait_output(child, Duration::from_secs(10));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        std::fs::read_to_string(fx.state_dir().join("prompt.md")).unwrap(),
        "task from stdin\n"
    );
    assert_eq!(fx.turn_start_text(), "task from stdin\n");
    assert_eq!(state::read_state(&fx.state_dir()).unwrap().status, Status::Completed);
}

#[test]
fn a_detached_run_reads_its_prompt_from_stdin() {
    let fx = Fixture::new();
    let mut child = fx
        .command(&["--detach", "--prompt-file", "-"], &["--complete-on-start"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"detached stdin task").unwrap();
    let output = wait_output(child, Duration::from_secs(20));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let done = fx.wait_state(|s| s.status.is_terminal());
    assert_eq!(done.status, Status::Completed);
    assert_eq!(fx.turn_start_text(), "detached stdin task");
    assert!(Path::new(&fx.state_dir().join("prompt.md")).exists());
}
