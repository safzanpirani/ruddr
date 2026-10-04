//! Lifecycle tests: a real controller drives the fake app-server in
//! `examples/fake_app_server.rs` over pipes. Each test asserts the returned
//! result, the persisted state, and the JSON-RPC requests the fake observed;
//! where relevant also socket cleanup and child termination. No real
//! provider ever runs.

use ruddr_core::control::{self, Command, Request, Response};
use ruddr_core::state::{self, RunState, Status};
use ruddr_runner::{CancelToken, RunConfig, run_controller};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ----- fixtures -----

fn fake_app_server() -> PathBuf {
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
        "missing {}; build it with `mbx test -p ruddr-runner` (examples build with the tests)",
        path.display()
    );
    path
}

struct Fixture {
    root: PathBuf,
    state_dir: PathBuf,
    prompt: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(prompt: &str) -> Fixture {
        // /tmp keeps the socket path short enough to live in the state dir.
        let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let root = base.join(format!("ruddr-life-{}", ruddr_core::fsutil::random_hex(4)));
        ruddr_core::fsutil::create_private_dir(&root).unwrap();
        let prompt_path = root.join("prompt.md");
        std::fs::write(&prompt_path, prompt).unwrap();
        Fixture {
            state_dir: root.join("run"),
            prompt: prompt_path,
            log: root.join("requests.jsonl"),
            root,
        }
    }

    fn config(&self, fake_flags: &[&str]) -> RunConfig {
        let mut command = vec![
            fake_app_server().to_string_lossy().into_owned(),
            "--request-log".into(),
            self.log.to_string_lossy().into_owned(),
        ];
        command.extend(fake_flags.iter().map(|f| f.to_string()));
        RunConfig {
            cwd: self.root.clone(),
            prompt_file: self.prompt.clone(),
            state_dir: self.state_dir.clone(),
            model: "test-model".into(),
            sandbox: "read-only".into(),
            approval_policy: "never".into(),
            child_command: command,
            ..Default::default()
        }
    }

    fn idle_config(&self, fake_flags: &[&str]) -> RunConfig {
        let mut flags = vec!["--multi-turn"];
        flags.extend_from_slice(fake_flags);
        RunConfig {
            idle: true,
            idle_timeout: Duration::from_secs(60),
            ..self.config(&flags)
        }
    }

    fn state(&self) -> RunState {
        state::read_state(&self.state_dir).unwrap()
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.state_dir.join(name)).unwrap()
    }

    /// Every JSON-RPC line the fake received, in order.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn request(&self, method: &str) -> Value {
        self.requests()
            .into_iter()
            .find(|r| r["method"] == method)
            .unwrap_or_else(|| panic!("the fake saw no {method} request"))
    }

    fn requests_for(&self, method: &str) -> Vec<Value> {
        self.requests().into_iter().filter(|r| r["method"] == method).collect()
    }

    fn wait_status(&self, want: Status) -> RunState {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(state) = state::read_state(&self.state_dir)
                && state.status == want
            {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "run did not reach {want}; state: {:?}",
                state::read_state(&self.state_dir).map(|s| s.status)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&self, command: Command, text: Option<&str>, expected: Option<&str>) -> Response {
        let request = Request {
            command,
            text: text.map(String::from),
            expected_turn_id: expected.map(String::from),
            images: vec![],
        };
        control::send(&self.state_dir, &request, Duration::from_secs(10)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Running {
    result: mpsc::Receiver<ruddr_core::Result<()>>,
    cancel: CancelToken,
    _thread: JoinHandle<()>,
}

impl Running {
    fn finish(&self) -> ruddr_core::Result<()> {
        self.finish_within(Duration::from_secs(10))
    }
    fn finish_within(&self, limit: Duration) -> ruddr_core::Result<()> {
        self.result
            .recv_timeout(limit)
            .unwrap_or_else(|_| panic!("the controller did not exit within {limit:?}"))
    }
}

fn start(cfg: RunConfig) -> Running {
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let (tx, result) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = tx.send(run_controller(cfg, &token));
    });
    Running {
        result,
        cancel,
        _thread: thread,
    }
}

fn alive(pid: i64) -> bool {
    ruddr_core::process::alive(pid)
}

fn wait_dead(pid: i64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    !alive(pid)
}

#[cfg_attr(not(unix), allow(dead_code))]
fn read_pid(path: &Path) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(pid) = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()) {
            return pid;
        }
        assert!(Instant::now() < deadline, "the fake recorded no grandchild pid");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn kill(pid: i64) {
    #[cfg(not(unix))]
    let _ = pid;
    #[cfg(unix)]
    if pid > 0 {
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

fn socket_gone(state: &RunState) {
    assert!(
        std::fs::symlink_metadata(&state.socket_path).is_err(),
        "the control socket {} is still there",
        state.socket_path
    );
    if let Some(dir) = &state.socket_dir {
        assert!(!Path::new(dir).exists(), "the private socket directory {dir} is still there");
    }
}

// ----- fresh runs and the handshake -----

#[test]
fn live_steer_over_the_control_socket() {
    let fx = Fixture::new("Initially say ORIGINAL");
    let run = start(fx.config(&[]));
    let active = fx.wait_status(Status::Active);
    assert_eq!(active.turn_id.as_deref(), Some("turn-test"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&active.socket_path), 0o600);
        assert_eq!(mode(Path::new(&active.socket_path).parent().unwrap().to_str().unwrap()), 0o700);
    }
    let response = fx.send(Command::Steer, Some("Say STEERED"), None);
    assert!(response.ok, "{response:?}");
    run.finish().unwrap();
    assert_eq!(fx.file("output.md").trim(), "STEERED");
    let state = fx.state();
    assert_eq!((state.steers, state.status), (1, Status::Completed));
    assert!(state.completed_at.is_some());
    socket_gone(&state);

    let initialize = fx.request("initialize");
    assert_eq!(initialize["params"]["clientInfo"]["name"], "ruddr");
    assert_eq!(initialize["params"]["capabilities"]["experimentalApi"], true);
    assert!(
        initialize["id"].as_str().unwrap().starts_with("ruddr-"),
        "Ruddr-originated calls use string IDs"
    );
    let initialized = fx.request("initialized");
    assert!(initialized.get("id").is_none(), "initialized is a notification");
    let start = fx.request("thread/start")["params"].clone();
    assert_eq!(start["cwd"], fx.root.to_string_lossy().as_ref());
    assert_eq!(
        (start["approvalPolicy"].as_str(), start["sandbox"].as_str()),
        (Some("never"), Some("read-only"))
    );
    assert_eq!(
        (start["provider"].as_str(), start["model"].as_str()),
        (Some("codex"), Some("test-model"))
    );
    assert_eq!(
        (start["ephemeral"].as_bool(), start["serviceName"].as_str()),
        (Some(false), Some("ruddr"))
    );
    let turn = fx.request("turn/start")["params"].clone();
    assert_eq!(turn["threadId"], "thread-test");
    assert_eq!(turn["input"][0]["text"], "Initially say ORIGINAL");
    let steer = fx.request("turn/steer")["params"].clone();
    assert_eq!(
        (steer["threadId"].as_str(), steer["expectedTurnId"].as_str()),
        (Some("thread-test"), Some("turn-test"))
    );
    assert_eq!(steer["input"][0]["text"], "Say STEERED");
}

#[test]
fn ephemeral_runs_pass_the_thread_option() {
    let fx = Fixture::new("ephemeral task");
    let cfg = RunConfig {
        ephemeral: true,
        effort: "high".into(),
        ..fx.config(&["--expect-ephemeral", "--complete-on-start"])
    };
    start(cfg).finish().unwrap();
    assert_eq!(fx.request("thread/start")["params"]["ephemeral"], true);
    assert_eq!(fx.request("turn/start")["params"]["effort"], "high");
    assert_eq!(fx.state().effort.as_deref(), Some("high"));
}

#[test]
fn images_ride_with_the_first_turn_and_steers() {
    let fx = Fixture::new("Initially say ORIGINAL");
    let (first, second) = (fx.root.join("first.png"), fx.root.join("second.jpg"));
    std::fs::write(&first, b"png").unwrap();
    std::fs::write(&second, b"jpg").unwrap();
    let run = start(RunConfig {
        images: vec![first.clone()],
        ..fx.config(&[])
    });
    fx.wait_status(Status::Active);
    let send = |images: Vec<String>| {
        let request = Request {
            command: Command::Steer,
            text: Some("Say STEERED".into()),
            expected_turn_id: None,
            images,
        };
        control::send(&fx.state_dir, &request, Duration::from_secs(10)).unwrap()
    };
    let missing = send(vec![fx.root.join("gone.png").to_string_lossy().into_owned()]);
    assert!(
        !missing.ok && missing.error.as_deref().unwrap_or_default().contains("not a readable file"),
        "{missing:?}"
    );
    assert!(fx.requests_for("turn/steer").is_empty(), "a bad image never reaches the provider");
    let response = send(vec![second.to_string_lossy().into_owned()]);
    assert!(response.ok, "{response:?}");
    run.finish().unwrap();
    let image = |path: &Path| json!({"type": "localImage", "path": path.to_string_lossy()});
    let turn = fx.request("turn/start")["params"]["input"].clone();
    assert_eq!(turn, json!([{"type": "text", "text": "Initially say ORIGINAL"}, image(&first)]));
    let steer = fx.request("turn/steer")["params"]["input"].clone();
    assert_eq!(steer, json!([{"type": "text", "text": "Say STEERED"}, image(&second)]));
}

#[test]
fn child_arguments_never_reach_the_trace() {
    let fx = Fixture::new("complete");
    let mut cfg = fx.config(&["--complete-on-start"]);
    cfg.child_command.push("TOP-SECRET-BEARER-TOKEN".into());
    start(cfg).finish().unwrap();
    let trace = fx.file("trace.log");
    assert!(!trace.contains("TOP-SECRET-BEARER-TOKEN"), "{trace}");
    assert!(trace.contains("[start] child pid="), "{trace}");
    assert!(
        trace.lines().all(|l| l.len() > 21 && l.as_bytes()[20] == b' '),
        "every record starts with a timestamp: {trace}"
    );
}

#[test]
fn server_requests_are_rejected_with_their_exact_id() {
    let fx = Fixture::new("needs approval");
    start(fx.config(&["--server-request"])).finish().unwrap();
    let raw = std::fs::read_to_string(&fx.log).unwrap();
    let rejection = raw
        .lines()
        .find(|l| l.contains("9007199254740993"))
        .expect("the controller answered the server request");
    let rejection: Value = serde_json::from_str(rejection).unwrap();
    assert_eq!(rejection["id"].as_u64(), Some(9_007_199_254_740_993));
    assert_eq!(rejection["error"]["code"], -32601);
    assert!(
        fx.file("trace.log")
            .contains("[warn] unsupported server request item/commandExecution/requestApproval")
    );
}

// ----- threads: resume and fork -----

#[test]
fn a_fork_that_reuses_the_source_thread_fails_before_starting_a_turn() {
    let fx = Fixture::new("fork safely");
    let cfg = RunConfig {
        fork_thread_id: "source-thread".into(),
        ..fx.config(&["--fork-same-id"])
    };
    let error = run_controller(cfg, &CancelToken::new()).unwrap_err();
    assert!(error.message.contains("source thread id"), "{error}");
    let state = fx.state();
    assert_eq!(state.status, Status::Failed);
    assert!(state.thread_id.is_none());
    assert!(fx.requests_for("turn/start").is_empty());
    assert!(!alive(state.child_pid));
    socket_gone(&state);
}

#[test]
fn resume_continues_the_source_thread_with_clean_params() {
    let fx = Fixture::new("continue the task");
    let cfg = RunConfig {
        ephemeral: true,
        resume_thread_id: "source-thread".into(),
        ..fx.config(&["--complete-on-start"])
    };
    start(cfg).finish().unwrap();
    let state = fx.state();
    assert_eq!(
        (state.thread_id.as_deref(), state.status),
        (Some("thread-resumed"), Status::Completed)
    );
    let params = fx.request("thread/resume")["params"].clone();
    assert_eq!(
        (params["threadId"].as_str(), params["excludeTurns"].as_bool()),
        (Some("source-thread"), Some(true))
    );
    assert!(params.get("ephemeral").is_none() && params.get("serviceName").is_none(), "{params}");
    assert!(fx.requests_for("thread/start").is_empty());
    assert!(fx.file("trace.log").contains("[thread] resumed thread-resumed"));
}

#[test]
fn fork_returns_a_new_thread() {
    let fx = Fixture::new("alternate approach");
    let cfg = RunConfig {
        fork_thread_id: "source-thread".into(),
        ..fx.config(&["--complete-on-start"])
    };
    start(cfg).finish().unwrap();
    let state = fx.state();
    assert_eq!(
        (state.thread_id.as_deref(), state.status),
        (Some("thread-forked"), Status::Completed)
    );
    let params = fx.request("thread/fork")["params"].clone();
    assert_eq!(
        (params["threadId"].as_str(), params["excludeTurns"].as_bool()),
        (Some("source-thread"), Some(true))
    );
    assert!(params.get("serviceName").is_none() && params.get("beforeTurnId").is_none() && params.get("lastTurnId").is_none());
    assert_eq!(params["ephemeral"], false);
}

#[test]
fn fork_boundary_selectors_map_to_one_field_each() {
    for (before, through) in [("turn-old", ""), ("", "turn-new")] {
        let fx = Fixture::new("alternate approach");
        let mut flags = vec!["--complete-on-start"];
        if before.is_empty() {
            flags.extend(["--expect-fork-through", through]);
        } else {
            flags.extend(["--expect-fork-before", before]);
        }
        let cfg = RunConfig {
            fork_thread_id: "source-thread".into(),
            fork_before_turn_id: before.into(),
            fork_through_turn_id: through.into(),
            ..fx.config(&flags)
        };
        start(cfg).finish().unwrap();
        assert_eq!(fx.state().thread_id.as_deref(), Some("thread-forked"));
        let params = fx.request("thread/fork")["params"].clone();
        if before.is_empty() {
            assert_eq!(params["lastTurnId"], through);
            assert!(params.get("beforeTurnId").is_none());
        } else {
            assert_eq!(params["beforeTurnId"], before);
            assert!(params.get("lastTurnId").is_none());
        }
    }
}

#[test]
fn an_invalid_fork_turn_fails_the_run() {
    let fx = Fixture::new("alternate approach");
    let cfg = RunConfig {
        fork_thread_id: "source-thread".into(),
        fork_before_turn_id: "turn-missing".into(),
        ..fx.config(&[])
    };
    let error = start(cfg).finish().unwrap_err();
    assert!(error.message.contains("unknown turn"), "{error}");
    assert_eq!(error.exit, ruddr_core::Exit::Failed);
    let state = fx.state();
    assert_eq!((state.status, state.thread_id), (Status::Failed, None));
    assert_eq!(state.error.as_deref(), Some("turn failed; see trace.log and provider.stderr.log"));
}

// ----- output, events, and redaction -----

#[test]
fn agent_messages_are_kept_in_order_and_errors_redacted() {
    let fx = Fixture::new("sensitive user prompt");
    let error = start(fx.config(&["--multi-output-error"])).finish().unwrap_err();
    assert!(
        error.message.contains("SECRET_ECHO"),
        "the private result keeps the provider error: {error}"
    );
    assert_eq!(fx.file("output.md"), "FIRST\n\nSECOND\n");
    let raw = fx.file("state.json");
    assert!(!raw.contains("SECRET_ECHO") && !raw.contains("sensitive user prompt"), "{raw}");
    let state = fx.state();
    assert_eq!(state.status, Status::Failed);
    assert!(state.error.as_deref().unwrap().contains("see trace.log"));
    assert!(fx.file("trace.log").contains("[error] SECRET_ECHO from prompt"));
}

#[test]
fn nested_turns_do_not_replace_or_complete_the_root_turn() {
    let fx = Fixture::new("delegate and finish");
    start(fx.config(&["--nested-turn"])).finish().unwrap();
    let state = fx.state();
    assert_eq!(
        (state.thread_id.as_deref(), state.turn_id.as_deref()),
        (Some("thread-test"), Some("turn-test"))
    );
    assert_eq!(fx.file("output.md").trim(), "ROOT DONE");
    assert!(
        fx.file("trace.log")
            .contains("[turn] nested completed thread=thread-child turn=turn-child")
    );
}

#[test]
fn the_accepted_prompt_precedes_early_provider_output() {
    let fx = Fixture::new("EARLY PROMPT");
    start(fx.config(&["--complete-before-turn-response"])).finish().unwrap();
    let events = fx.file("events.jsonl");
    let prompt = events.find(r#""origin":"ruddr""#).unwrap();
    let output = events.find(r#""text":"EARLY""#).unwrap();
    assert!(prompt < output, "{events}");
    assert!(events.contains(r#""method":"ruddr/prompt/accepted""#));
    assert_eq!(fx.file("output.md"), "EARLY\n");
}

#[test]
fn provider_events_show_while_turn_start_is_pending() {
    let fx = Fixture::new("VISIBLE PROMPT");
    let run = start(fx.config(&[
        "--early-item-before-turn-response",
        "--turn-response-delay-ms",
        "500",
        "--complete-on-start",
    ]));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let events = std::fs::read_to_string(fx.state_dir.join("events.jsonl")).unwrap_or_default();
        if events.contains(r#""origin":"ruddr""#) && events.contains(r#""text":"LIVE BEFORE RESPONSE""#) {
            assert_eq!(
                fx.state().status,
                Status::Starting,
                "provider output became visible only after turn/start resolved"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "start-time provider activity was not visible before the response"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    run.finish().unwrap();
}

#[test]
fn a_turn_start_without_an_id_records_an_unknown_outcome() {
    let fx = Fixture::new("UNKNOWN PROMPT");
    let error = start(fx.config(&["--turn-response-no-id"])).finish().unwrap_err();
    assert!(error.message.contains("no turn id"), "{error}");
    let events = fx.file("events.jsonl");
    assert!(
        events.contains(r#""method":"ruddr/prompt/unknown""#) && !events.contains(r#""method":"ruddr/prompt/accepted""#),
        "{events}"
    );
    assert_eq!(fx.state().status, Status::Failed);
}

// ----- interrupt, cancellation, and the watchdog -----

#[test]
fn interrupt_preserves_the_interrupted_status() {
    let fx = Fixture::new("stay active");
    let run = start(fx.config(&[]));
    fx.wait_status(Status::Active);
    let response = fx.send(Command::Interrupt, None, None);
    assert!(response.ok, "{response:?}");
    let error = run.finish().unwrap_err();
    assert!(error.message.contains("interrupted"), "{error}");
    let state = fx.state();
    assert_eq!(state.status, Status::Interrupted);
    assert_eq!(state.error, None);
    let params = fx.request("turn/interrupt")["params"].clone();
    assert_eq!(
        (params["threadId"].as_str(), params["turnId"].as_str()),
        (Some("thread-test"), Some("turn-test"))
    );
    socket_gone(&state);
}

#[cfg(unix)]
#[test]
fn an_acknowledged_interrupt_tears_down_the_whole_tree() {
    let fx = Fixture::new("stay active");
    let pid_file = fx.root.join("grandchild.pid");
    let run = start(fx.config(&[
        "--interrupt-ack-only",
        "--term-ignoring-grandchild",
        "--grandchild-pid-file",
        pid_file.to_str().unwrap(),
    ]));
    let active = fx.wait_status(Status::Active);
    let grandchild = read_pid(&pid_file);
    let response = fx.send(Command::Interrupt, None, None);
    assert!(response.ok, "{response:?}");
    let result = run.finish_within(Duration::from_secs(4));
    let grandchild_dead = wait_dead(grandchild);
    if !grandchild_dead {
        kill(grandchild);
    }
    assert!(result.unwrap_err().message.contains("interrupted"));
    assert_eq!(fx.state().status, Status::Interrupted);
    assert!(wait_dead(active.child_pid), "child pid {} is still alive", active.child_pid);
    assert!(grandchild_dead, "the TERM-ignoring grandchild {grandchild} survived");
    socket_gone(&active);
}

#[cfg(unix)]
#[test]
fn cancellation_cleans_up_child_socket_and_state() {
    let fx = Fixture::new("stay active");
    // A long state directory moves the socket into a private temporary parent.
    let long = fx.root.join("long-segment-".repeat(8));
    let pid_file = fx.root.join("grandchild.pid");
    let cfg = RunConfig {
        state_dir: long.clone(),
        ..fx.config(&["--grandchild-pid-file", pid_file.to_str().unwrap()])
    };
    let run = start(cfg);
    let deadline = Instant::now() + Duration::from_secs(5);
    let active = loop {
        if let Ok(state) = state::read_state(&long)
            && state.status == Status::Active
        {
            break state;
        }
        assert!(Instant::now() < deadline, "the run never became active");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        active.socket_dir.is_some(),
        "a long state directory uses a private socket directory"
    );
    let grandchild = read_pid(&pid_file);
    run.cancel.cancel();
    let error = run.finish_within(Duration::from_secs(6)).unwrap_err();
    assert!(error.message.contains("interrupted"), "{error}");
    let state = state::read_state(&long).unwrap();
    assert_eq!(state.status, Status::Interrupted);
    assert!(wait_dead(active.child_pid));
    let dead = wait_dead(grandchild);
    if !dead {
        kill(grandchild);
    }
    assert!(dead, "grandchild {grandchild} survived cancellation");
    socket_gone(&active);
    assert!(std::fs::read_to_string(long.join("trace.log")).unwrap().contains("[interrupt]"));
}

#[test]
fn the_turn_watchdog_stops_a_hung_run() {
    let fx = Fixture::new("stay active");
    let cfg = RunConfig {
        turn_timeout: Duration::from_millis(50),
        ..fx.config(&[])
    };
    let error = start(cfg).finish().unwrap_err();
    assert!(error.message.contains("watchdog"), "{error}");
    let state = fx.state();
    assert_eq!(state.status, Status::Failed);
    assert!(wait_dead(state.child_pid));
    assert!(fx.file("trace.log").contains("[error] active turn exceeded watchdog"));
}

// ----- persistence failures -----

#[cfg(unix)]
#[test]
fn a_state_persistence_failure_never_reports_success() {
    use std::io::Write;
    let fx = Fixture::new("stay active");
    let run = start(fx.config(&[]));
    let active = fx.wait_status(Status::Active);
    // Connect first: the open connection survives moving the directory.
    let mut connection = std::os::unix::net::UnixStream::connect(&active.socket_path).unwrap();
    let saved = fx.root.join("run-saved");
    std::fs::rename(&fx.state_dir, &saved).unwrap();
    std::fs::write(&fx.state_dir, "blocked").unwrap();
    connection.write_all(b"{\"command\":\"steer\",\"text\":\"finish now\"}\n").unwrap();
    let error = run.finish().unwrap_err();
    assert!(error.message.contains("persist"), "{error}");
    // read_state refuses a moved directory, so parse the saved file directly.
    let durable: RunState = serde_json::from_slice(&std::fs::read(saved.join("state.json")).unwrap()).unwrap();
    assert_eq!(durable.status, Status::Active, "the last durable state stays readable");
}

#[test]
fn an_output_persistence_failure_fails_the_run() {
    let fx = Fixture::new("stay active");
    let run = start(fx.config(&[]));
    fx.wait_status(Status::Active);
    let output = fx.state_dir.join("output.md");
    std::fs::rename(&output, fx.state_dir.join("output.md.saved")).unwrap();
    std::fs::create_dir(&output).unwrap();
    let _ = control::send(
        &fx.state_dir,
        &Request {
            command: Command::Steer,
            images: vec![],
            text: Some("finish now".into()),
            expected_turn_id: None,
        },
        Duration::from_secs(5),
    );
    let error = run.finish().unwrap_err();
    assert!(error.message.contains("persist agent output"), "{error}");
    assert_eq!(fx.state().status, Status::Failed);
    assert_eq!(fx.file("output.md.saved"), "");
}

// ----- idle sessions -----

#[test]
fn an_idle_prompt_starts_a_second_turn() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&[]));
    fx.wait_status(Status::Idle);
    let response = fx.send(Command::Prompt, Some("SECOND SECRET TASK"), None);
    assert!(response.ok, "{response:?}");
    let state = fx.wait_status(Status::Idle);
    assert_eq!(state.turns, 2);
    assert_eq!(state.last_turn, Some(Status::Completed));
    let usage = state.token_usage.clone().unwrap();
    assert_eq!(
        (usage.total_tokens, usage.context_window, usage.context_tokens),
        (200, 1000, Some(90))
    );
    assert!(!fx.file("state.json").contains("SECRET TASK"));
    assert!(fx.file("output.md").contains("TURN 1\n\n---\n\nTURN 2"));
    let events = fx.file("events.jsonl");
    assert!(events.contains(r#""userMessage""#) && events.contains("SECOND SECRET TASK"));
    assert_eq!(fx.requests_for("turn/start").len(), 2);
    let response = fx.send(Command::Stop, None, None);
    assert!(response.ok, "{response:?}");
    run.finish().unwrap();
    let state = fx.state();
    assert_eq!(state.status, Status::Completed);
    assert!(state.completed_at.is_some());
    socket_gone(&state);
}

#[test]
fn a_steer_while_idle_is_rejected_and_never_becomes_a_turn() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&[]));
    fx.wait_status(Status::Idle);
    let response = fx.send(Command::Steer, Some("nope"), None);
    assert!(
        !response.ok && response.error.as_deref().unwrap().contains("not steerable"),
        "{response:?}"
    );
    assert_eq!(fx.state().turns, 1);
    assert_eq!(fx.requests_for("turn/start").len(), 1);
    assert!(fx.requests_for("turn/steer").is_empty());
    assert!(fx.send(Command::Stop, None, None).ok);
    run.finish().unwrap();
}

#[test]
fn a_prompt_while_active_is_rejected_and_interrupt_returns_to_idle() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--multi-turn-hold", "--interrupt-complete-first"]));
    let active = fx.wait_status(Status::Active);
    let response = fx.send(Command::Prompt, Some("too early"), None);
    assert!(
        !response.ok && response.error.as_deref().unwrap().contains("steer it instead"),
        "{response:?}"
    );
    assert!(fx.send(Command::Interrupt, None, None).ok);
    fx.wait_status(Status::Idle);
    assert!(alive(active.child_pid), "an idle-mode interrupt killed the provider");
    assert!(fx.send(Command::Prompt, Some("after interrupt"), None).ok);
    assert_eq!(fx.wait_status(Status::Active).turns, 2);
    assert!(fx.send(Command::Interrupt, None, None).ok);
    fx.wait_status(Status::Idle);
    assert!(fx.send(Command::Stop, None, None).ok);
    assert!(run.finish().is_err(), "a run whose last turn was interrupted does not succeed");
    assert_eq!(fx.state().status, Status::Interrupted);
}

#[test]
fn an_idle_interrupt_waits_for_the_provider_to_settle() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--multi-turn-hold", "--interrupt-completion-delay-ms", "250"]));
    fx.wait_status(Status::Active);
    let state_dir = fx.state_dir.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let request = Request {
            command: Command::Interrupt,
            images: vec![],
            text: None,
            expected_turn_id: None,
        };
        let _ = tx.send(control::send(&state_dir, &request, Duration::from_secs(5)));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "interrupt returned before the provider settled"
    );
    assert_eq!(fx.state().status, Status::Active);
    let response = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    assert!(response.ok, "{response:?}");
    fx.wait_status(Status::Idle);
    assert!(fx.send(Command::Stop, None, None).ok);
    assert!(run.finish().unwrap_err().message.contains("interrupted"));
}

#[test]
fn an_idle_interrupt_that_never_settles_fails_the_session() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let cfg = RunConfig {
        interrupt_timeout: Some(Duration::from_millis(50)),
        ..fx.idle_config(&["--multi-turn-hold", "--interrupt-ack-only"])
    };
    let run = start(cfg);
    fx.wait_status(Status::Active);
    let response = fx.send(Command::Interrupt, None, None);
    assert!(
        !response.ok && response.error.as_deref().unwrap().contains("did not settle"),
        "{response:?}"
    );
    assert!(run.finish().unwrap_err().message.contains("did not settle"));
    let state = fx.state();
    assert_eq!(state.status, Status::Failed);
    assert!(wait_dead(state.child_pid));
}

#[test]
fn an_idle_interrupt_reports_a_provider_exit_after_the_acknowledgement() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--multi-turn-hold", "--exit-after-interrupt-ack"]));
    fx.wait_status(Status::Active);
    let response = fx.send(Command::Interrupt, None, None);
    assert!(
        !response.ok && response.error.as_deref().unwrap().contains("provider output closed"),
        "{response:?}"
    );
    assert!(run.finish().unwrap_err().message.contains("provider output closed"));
    assert_eq!(fx.state().status, Status::Failed);
}

#[test]
fn an_ambiguous_second_turn_start_fails_the_session() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let cfg = RunConfig {
        idle_turn_start_timeout: Some(Duration::from_millis(50)),
        ..fx.idle_config(&["--ambiguous-second-turn"])
    };
    let run = start(cfg);
    fx.wait_status(Status::Idle);
    let response = fx.send(Command::Prompt, Some("ambiguous turn"), None);
    assert!(!response.ok, "{response:?}");
    assert!(run.finish().unwrap_err().message.contains("ambiguous"));
    let state = fx.state();
    assert_eq!(state.status, Status::Failed);
    assert!(wait_dead(state.child_pid));
}

#[test]
fn a_rejected_second_turn_restores_idle() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--reject-second-turn"]));
    fx.wait_status(Status::Idle);
    let response = fx.send(Command::Prompt, Some("rejected turn"), None);
    assert!(
        !response.ok && response.error.as_deref().unwrap().contains("second turn rejected"),
        "{response:?}"
    );
    fx.wait_status(Status::Idle);
    assert!(fx.send(Command::Prompt, Some("accepted turn"), None).ok);
    assert_eq!(fx.wait_status(Status::Idle).turns, 2);
    assert_eq!(fx.file("output.md"), "TURN 1\n\n---\n\nTURN 2\n");
    let events = fx.file("events.jsonl");
    assert_eq!(events.matches(r#""origin":"ruddr""#).count(), 3);
    assert_eq!(events.matches(r#""method":"ruddr/prompt/accepted""#).count(), 2);
    assert_eq!(events.matches(r#""method":"ruddr/prompt/rejected""#).count(), 1);
    assert!(fx.send(Command::Stop, None, None).ok);
    run.finish().unwrap();
}

#[test]
fn a_second_turn_steer_carries_the_current_thread_and_turn() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--multi-turn-hold"]));
    fx.wait_status(Status::Active);
    assert!(fx.send(Command::Interrupt, None, None).ok);
    fx.wait_status(Status::Idle);
    assert!(fx.send(Command::Prompt, Some("second turn"), None).ok);
    assert_eq!(fx.wait_status(Status::Active).turn_id.as_deref(), Some("turn-2"));
    let stale = fx.send(Command::Steer, Some("stale direction"), Some("turn-1"));
    assert!(
        !stale.ok && stale.error.as_deref().unwrap().contains("active turn changed"),
        "{stale:?}"
    );
    assert_eq!(fx.wait_status(Status::Active).steers, 0);
    assert!(fx.requests_for("turn/steer").is_empty(), "a stale steer reached the provider");
    assert!(fx.send(Command::Steer, Some("new direction"), Some("turn-2")).ok);
    fx.wait_status(Status::Idle);
    let steer = fx.request("turn/steer")["params"].clone();
    assert_eq!(
        (steer["threadId"].as_str(), steer["expectedTurnId"].as_str()),
        (Some("thread-test"), Some("turn-2"))
    );
    assert!(fx.send(Command::Stop, None, None).ok);
    run.finish().unwrap();
}

#[test]
fn prompt_needs_an_idle_session() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(RunConfig {
        idle: false,
        ..fx.idle_config(&["--multi-turn-hold"])
    });
    fx.wait_status(Status::Active);
    let response = fx.send(Command::Prompt, Some("nope"), None);
    assert!(
        !response.ok && response.error.as_deref().unwrap().contains("--idle"),
        "{response:?}"
    );
    let stop = fx.send(Command::Stop, None, None);
    assert!(!stop.ok && stop.error.as_deref().unwrap().contains("--idle"), "{stop:?}");
    assert!(fx.send(Command::Interrupt, None, None).ok);
    assert!(run.finish().is_err());
}

#[test]
fn the_idle_timeout_exits_with_the_last_status() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let cfg = RunConfig {
        idle_timeout: Duration::from_millis(200),
        ..fx.idle_config(&[])
    };
    start(cfg).finish().unwrap();
    let state = fx.state();
    assert_eq!(state.status, Status::Completed);
    assert!(state.completed_at.is_some());
    assert!(fx.file("trace.log").contains("[idle] timeout after"));
}

#[test]
fn a_child_exit_while_idle_fails_the_session() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let error = start(fx.idle_config(&["--exit-after-turn"])).finish().unwrap_err();
    assert!(error.message.contains("provider output closed"), "{error}");
    assert_eq!(fx.state().status, Status::Failed);
}

#[test]
fn cancelling_an_idle_session_persists_interrupted() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&[]));
    let idle = fx.wait_status(Status::Idle);
    run.cancel.cancel();
    assert!(run.finish().unwrap_err().message.contains("interrupted"));
    assert_eq!(fx.state().status, Status::Interrupted);
    assert!(wait_dead(idle.child_pid));
    socket_gone(&idle);
}

#[test]
fn concurrent_idle_prompts_accept_one_generation() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--delay-turn-start"]));
    fx.wait_status(Status::Idle);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = ["second-a", "second-b"]
        .into_iter()
        .map(|text| {
            let (barrier, state_dir) = (barrier.clone(), fx.state_dir.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let request = Request {
                    command: Command::Prompt,
                    images: vec![],
                    text: Some(text.into()),
                    expected_turn_id: None,
                };
                control::send(&state_dir, &request, Duration::from_secs(10)).unwrap()
            })
        })
        .collect();
    let accepted = handles.into_iter().map(|h| h.join().unwrap()).filter(|r| r.ok).count();
    assert_eq!(accepted, 1);
    assert_eq!(fx.wait_status(Status::Idle).turns, 2);
    assert!(fx.send(Command::Stop, None, None).ok);
    run.finish().unwrap();
}

#[test]
fn stop_rejects_a_later_prompt() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&[]));
    fx.wait_status(Status::Idle);
    assert!(fx.send(Command::Stop, None, None).ok);
    let request = Request {
        command: Command::Prompt,
        images: vec![],
        text: Some("too late".into()),
        expected_turn_id: None,
    };
    if let Ok(response) = control::send(&fx.state_dir, &request, Duration::from_secs(5)) {
        assert!(!response.ok, "a prompt was accepted after stop: {response:?}");
    }
    run.finish().unwrap();
    let state = fx.state();
    assert_eq!((state.turns, state.status), (1, Status::Completed));
}

#[test]
fn a_delayed_interrupt_error_does_not_stop_the_new_turn() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--multi-turn-hold", "--defer-interrupt-error"]));
    fx.wait_status(Status::Active);
    let state_dir = fx.state_dir.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let request = Request {
            command: Command::Interrupt,
            images: vec![],
            text: None,
            expected_turn_id: None,
        };
        let _ = tx.send(control::send(&state_dir, &request, Duration::from_secs(10)));
    });
    fx.wait_status(Status::Idle);
    assert!(fx.send(Command::Prompt, Some("second turn"), None).ok);
    let old = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    assert!(
        !old.ok && old.error.as_deref().unwrap().contains("old turn already completed"),
        "{old:?}"
    );
    let state = fx.state();
    assert_eq!((state.status, state.turn_id.as_deref()), (Status::Active, Some("turn-2")));
    assert!(alive(state.child_pid), "the old interrupt stopped the new turn");
    run.cancel.cancel();
    assert!(run.finish().is_err());
    let state = fx.state();
    assert_eq!(state.status, Status::Interrupted);
    assert!(wait_dead(state.child_pid));
    socket_gone(&state);
}

#[test]
fn interrupt_honors_the_expected_turn() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&["--multi-turn-hold"]));
    fx.wait_status(Status::Active);
    let stale = fx.send(Command::Interrupt, None, Some("old-turn"));
    assert!(
        !stale.ok && stale.error.as_deref().unwrap().contains("interrupt was not sent"),
        "{stale:?}"
    );
    assert!(
        fx.requests_for("turn/interrupt").is_empty(),
        "a stale interrupt reached the provider"
    );
    let state = fx.state();
    assert_eq!((state.status, state.turn_id.as_deref()), (Status::Active, Some("turn-1")));
    assert!(fx.send(Command::Interrupt, None, Some("turn-1")).ok);
    fx.wait_status(Status::Idle);
    assert!(fx.send(Command::Prompt, Some("next turn"), None).ok);
    fx.wait_status(Status::Active);
    assert!(fx.send(Command::Interrupt, None, None).ok);
    fx.wait_status(Status::Idle);
    let interrupts = fx.requests_for("turn/interrupt");
    assert_eq!(interrupts[1]["params"]["turnId"], "turn-2");
    run.cancel.cancel();
    assert!(run.finish().is_err());
}

// ----- the control channel -----

#[cfg(unix)]
fn raw_control(socket: &str, line: &str) -> Value {
    use std::io::{BufRead, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream.write_all(line.as_bytes()).unwrap();
    let mut reply = String::new();
    std::io::BufReader::new(stream).read_line(&mut reply).unwrap();
    serde_json::from_str(&reply).unwrap()
}

#[cfg(unix)]
#[test]
fn the_control_channel_speaks_go_and_rust_clients() {
    let fx = Fixture::new("FIRST SECRET TASK");
    let run = start(fx.idle_config(&[]));
    let idle = fx.wait_status(Status::Idle);
    let status = raw_control(&idle.socket_path, "{\"command\":\"status\"}\n");
    assert_eq!(
        (status["ok"].as_bool(), status["state"]["status"].as_str()),
        (Some(true), Some("idle"))
    );
    let unknown = raw_control(&idle.socket_path, "{\"command\":\"dance\"}\n");
    assert_eq!(unknown["error"], "unknown control command \"dance\"");
    assert!(raw_control(&idle.socket_path, "not json\n")["ok"] == false);
    let empty = raw_control(&idle.socket_path, "{\"command\":\"prompt\"}\n");
    assert_eq!(empty["error"], "prompt text is empty");
    // Go clients stop an idle session with "shutdown".
    let stop = raw_control(&idle.socket_path, "{\"command\":\"shutdown\"}\n");
    assert_eq!(stop["ok"], true, "{stop}");
    run.finish().unwrap();
}

#[test]
fn control_requests_fail_promptly_on_stale_state() {
    let fx = Fixture::new("unused");
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    let stale = serde_json::json!({
        "version": 2, "pid": 99_999_999, "status": "active", "threadId": "thread-stale", "turnId": "turn-stale",
        "stateDir": fx.state_dir, "socketPath": fx.state_dir.join("missing.sock"),
        "startedAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z",
    });
    std::fs::write(fx.state_dir.join("state.json"), stale.to_string()).unwrap();
    let started = Instant::now();
    let request = Request {
        command: Command::Steer,
        images: vec![],
        text: Some("new direction".into()),
        expected_turn_id: None,
    };
    let error = control::send(&fx.state_dir, &request, Duration::from_secs(5)).unwrap_err();
    assert_eq!(error.exit, ruddr_core::Exit::Stale);
    assert!(error.message.contains("is not running"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(fx.state().displayed().status, Status::Stale);
}

// ----- state directories and detaching -----

#[test]
fn a_run_refuses_a_used_state_directory() {
    let fx = Fixture::new("complete");
    start(fx.config(&["--complete-on-start"])).finish().unwrap();
    let before = fx.file("events.jsonl");
    let error = start(fx.config(&["--complete-on-start"])).finish().unwrap_err();
    assert!(
        error.message.contains("already contains a Ruddr run with status completed"),
        "{error}"
    );
    assert_eq!(fx.file("events.jsonl"), before);
}

#[test]
fn an_empty_prompt_fails_before_claiming_the_directory() {
    let fx = Fixture::new("   \n");
    let error = start(fx.config(&[])).finish().unwrap_err();
    assert_eq!(error.message, "prompt file is empty");
    assert!(!fx.state_dir.exists());
}

#[test]
fn a_detached_run_returns_once_the_controller_is_live() {
    let fx = Fixture::new("unused");
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    let dir = fx.state_dir.to_string_lossy().into_owned();
    let command: Vec<std::ffi::OsString> = [
        fake_app_server().to_string_lossy().into_owned(),
        "--detach-helper".into(),
        "active".into(),
        "--detach-dir".into(),
        dir.clone(),
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let startup = ruddr_runner::detach::start_detached_run(&fx.state_dir, &command, Duration::from_secs(10)).unwrap();
    assert_eq!(startup.status, Status::Active);
    assert!(startup.pid > 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let log = fx.state_dir.join(ruddr_runner::detach::LAUNCH_STDERR_FILE);
        assert_eq!(std::fs::metadata(log).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert!(fx.file("helper.args").contains("--detach-dir"));
}

#[test]
fn a_detached_startup_crash_reports_its_stderr() {
    let fx = Fixture::new("unused");
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    let dir = fx.state_dir.to_string_lossy().into_owned();
    let command: Vec<std::ffi::OsString> = [
        fake_app_server().to_string_lossy().into_owned(),
        "--detach-helper".into(),
        "crash".into(),
        "--detach-dir".into(),
        dir,
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let error = ruddr_runner::detach::start_detached_run(&fx.state_dir, &command, Duration::from_secs(10)).unwrap_err();
    assert!(error.message.contains("provider binary not found"), "{error}");
}

#[test]
fn opencode_resume_persists_the_effective_model_and_effort() {
    for (model, effort) in [("", ""), ("new/model", "high")] {
        let fx = Fixture::new("continue the task");
        let effective_model = if model.is_empty() { "stored/model" } else { model };
        let cfg = RunConfig {
            provider: "opencode".into(),
            model: model.into(),
            effort: effort.into(),
            resume_thread_id: "source-thread".into(),
            ..fx.config(&["--complete-on-start", "--resume-model", effective_model, "--resume-effort", "low"])
        };
        start(cfg).finish().unwrap();
        let state = fx.state();
        assert_eq!(state.status, Status::Completed);
        assert_eq!(state.model, effective_model);
        assert_eq!(state.effort.as_deref(), Some(if effort.is_empty() { "low" } else { effort }));
        let request = fx.request("thread/resume");
        assert_eq!(
            request["params"].get("model").and_then(Value::as_str),
            (!model.is_empty()).then_some(model)
        );
        assert_eq!(
            fx.request("turn/start")["params"].get("effort").and_then(Value::as_str),
            (!effort.is_empty()).then_some(effort)
        );
        assert!(!alive(state.child_pid));
        socket_gone(&state);
    }
}
