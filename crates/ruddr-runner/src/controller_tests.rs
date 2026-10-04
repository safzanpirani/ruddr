//! Unit tests for controller internals that need seams the lifecycle tests
//! cannot reach: blocked stdin writes, the write gate, accept retries,
//! duplicate responses, ID-preserving rejections, interrupt claims, and
//! output ordering.

use crate::config::RunConfig;
use crate::controller::{Controller, read_line_limited, rpc_id};
use crate::store::StateStore;
use crate::test_support::TempDir;
use ruddr_core::state::{self, Status};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn controller(idle: bool) -> (TempDir, Arc<Controller>) {
    let dir = TempDir::new("ctl");
    let cfg = RunConfig {
        state_dir: dir.join("run"),
        idle,
        model: "test-model".into(),
        sandbox: "read-only".into(),
        ..Default::default()
    };
    let store = StateStore::create(&cfg).unwrap();
    (dir, Controller::new(cfg, store))
}

/// A writer that blocks until its sender is dropped.
struct Blocking(mpsc::Receiver<()>);

impl Write for Blocking {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        let _ = self.0.recv();
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer that records everything written to it.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<u8>>>);

impl Recorder {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready() {
        assert!(Instant::now() < deadline, "condition never held");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_blocked_stdin_write_respects_the_rpc_timeout() {
    let (_dir, c) = controller(false);
    let (release, blocked) = mpsc::channel();
    c.attach_stdin(Box::new(Blocking(blocked)));
    let started = Instant::now();
    let error = c.call("thread/list", json!({}), Duration::from_millis(50)).unwrap_err();
    assert!(error.to_string().contains("write timed out"), "{error}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the blocked write ignored the timeout"
    );
    // Later writes fail fast instead of interleaving with the stuck frame.
    let again = c.write_line(b"{}\n".to_vec(), Duration::from_secs(1)).unwrap_err();
    assert!(again.contains("session ended"), "{again}");
    assert_eq!(state::read_state(&c.cfg.state_dir).unwrap().status, Status::Failed);
    assert!(c.stop_child.load(Ordering::SeqCst));
    drop(release);
}

#[cfg(unix)]
#[test]
fn a_blocked_child_pipe_fails_the_run_and_cleans_up_the_child_and_socket() {
    let dir = TempDir::new("blocked-pipe");
    let cfg = RunConfig {
        state_dir: dir.join("run"),
        cwd: dir.to_path_buf(),
        child_command: vec!["sleep".into(), "30".into()],
        ..Default::default()
    };
    let store = StateStore::create(&cfg).unwrap();
    let c = Controller::new(cfg, store);
    c.open_logs().unwrap();
    c.start_child().unwrap();
    crate::control_server::start(&c).unwrap();
    let pid = c.child_pid();
    c.store.update(|s| s.status = Status::Active).unwrap();
    assert!(c.write_line(vec![b'x'; 2 * 1024 * 1024], Duration::from_millis(50)).is_err());
    c.shutdown_child();
    c.close_logs();
    crate::control_server::close(&c);
    assert_eq!(state::read_state(&c.cfg.state_dir).unwrap().status, Status::Failed);
    assert!(!ruddr_core::process::alive(pid as i64));
    assert!(!std::path::Path::new(&c.store.snapshot().socket_path).exists());
}

#[test]
fn waiting_for_the_write_gate_counts_against_the_deadline() {
    let (_dir, c) = controller(false);
    let recorder = Recorder::default();
    c.attach_stdin(Box::new(recorder.clone()));
    c.lock().write_busy = true;
    let error = c.call("turn/steer", json!({}), Duration::from_millis(20)).unwrap_err();
    assert!(error.to_string().contains("write timed out"), "{error}");
    assert_eq!(c.pending_len(), 0, "a timed out call left a pending response");
    assert!(recorder.text().is_empty());
    // The writer that held the gate still owns a working stdin.
    c.lock().write_busy = false;
    c.notify();
    c.write_line(b"{\"after\":true}\n".to_vec(), Duration::from_secs(1)).unwrap();
    assert_eq!(recorder.text(), "{\"after\":true}\n");
}

#[test]
fn calls_correlate_responses_and_surface_rpc_errors() {
    let (_dir, c) = controller(false);
    c.open_logs().unwrap();
    let recorder = Recorder::default();
    c.attach_stdin(Box::new(recorder.clone()));
    let caller = c.clone();
    let call = std::thread::spawn(move || caller.call("thread/list", json!({"limit": 1}), Duration::from_secs(5)));
    wait_until(|| recorder.text().contains("thread/list"));
    let sent: Value = serde_json::from_str(recorder.text().trim()).unwrap();
    assert_eq!(sent, json!({"id": "ruddr-1", "method": "thread/list", "params": {"limit": 1}}));
    c.deliver(r#"{"id":"ruddr-1","error":{"code":-32602,"message":"bad params"}}"#);
    let error = call.join().unwrap().unwrap_err();
    assert_eq!(error.to_string(), "bad params (-32602)");
}

#[test]
fn duplicate_responses_never_block_the_reader() {
    let (_dir, c) = controller(false);
    c.open_logs().unwrap();
    let mailbox = c.register_pending("ruddr-1");
    let input = "{\"id\":\"ruddr-1\",\"result\":{}}\n".repeat(3)
        + "{\"method\":\"item/completed\",\"params\":{\"item\":{\"type\":\"agentMessage\",\"text\":\"AFTER DUPLICATES\"}}}\n"
        + "{\"method\":\"turn/completed\",\"params\":{\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\"}}}\n";
    let reader = c.clone();
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        reader.read_child(io::Cursor::new(input.into_bytes()));
        let _ = done.send(());
    });
    finished
        .recv_timeout(Duration::from_secs(2))
        .expect("duplicate responses blocked later provider output");
    assert!(mailbox.try_recv().is_ok());
    let state = state::read_state(std::path::Path::new(&c.store.snapshot().state_dir)).unwrap();
    assert_eq!(state.status, Status::Completed);
    assert_eq!(std::fs::read_to_string(&state.output_path).unwrap(), "AFTER DUPLICATES\n");
    let events = std::fs::read_to_string(&state.events_path).unwrap();
    assert_eq!(events.lines().count(), 5, "every provider line reaches events.jsonl");
}

#[cfg(unix)]
#[test]
fn an_event_write_failure_fails_the_run_and_cleans_up_the_child_and_socket() {
    let dir = TempDir::new("event-write");
    let cfg = RunConfig {
        state_dir: dir.join("run"),
        cwd: dir.to_path_buf(),
        child_command: vec!["sleep".into(), "30".into()],
        ..Default::default()
    };
    let store = StateStore::create(&cfg).unwrap();
    let c = Controller::new(cfg, store);
    c.open_logs().unwrap();
    c.start_child().unwrap();
    crate::control_server::start(&c).unwrap();
    let pid = c.child_pid();
    let mailbox = c.register_pending("ruddr-test");
    c.set_events_file(std::fs::File::open(&c.store.snapshot().events_path).unwrap());
    c.deliver(r#"{"method":"turn/completed","params":{"turn":{"id":"turn-test","status":"completed"}}}"#);
    assert!(c.private_result_error().contains("persist provider event"));
    assert!(mailbox.recv_timeout(Duration::from_secs(1)).is_err());
    assert!(c.stop_child.load(Ordering::SeqCst));
    c.shutdown_child();
    c.close_logs();
    crate::control_server::close(&c);
    let persisted = state::read_state(&c.cfg.state_dir).unwrap();
    assert_eq!(persisted.status, Status::Failed);
    assert!(persisted.completed_at.is_some());
    assert_eq!(
        persisted.error.as_deref(),
        Some("turn failed; see trace.log and provider.stderr.log")
    );
    assert!(!ruddr_core::process::alive(pid as i64));
    assert!(!std::path::Path::new(&persisted.socket_path).exists());
}

#[test]
fn interactive_request_rejections_keep_the_exact_id() {
    for id in [
        json!("approval-1"),
        json!(9_007_199_254_740_993u64),
        json!(9_223_372_036_854_775_807u64),
    ] {
        let (_dir, c) = controller(false);
        let recorder = Recorder::default();
        c.attach_stdin(Box::new(recorder.clone()));
        c.reject_server_request(&id, "item/commandExecution/requestApproval");
        let reply: Value = serde_json::from_str(recorder.text().trim()).unwrap();
        assert_eq!(reply["id"], id);
        assert_eq!(reply["error"]["code"], -32601);
        assert!(recorder.text().contains(&format!("\"id\":{id}")), "{}", recorder.text());
    }
}

#[test]
fn an_old_interrupt_timeout_cannot_claim_a_later_turn() {
    let (_dir, c) = controller(true);
    c.store
        .update(|s| {
            s.status = Status::Active;
            s.turn_id = Some("turn-2".into());
        })
        .unwrap();
    c.lock().turn_gen = 2;
    assert!(!c.end_session_if_turn_open(1, Status::Failed, "old interrupt timeout"));
    assert_eq!(c.store.snapshot().status, Status::Active);
    assert!(!c.stop_child.load(Ordering::SeqCst));
    assert!(c.end_session_if_turn_open(2, Status::Failed, "current interrupt timeout"));
    let persisted = state::read_state(std::path::Path::new(&c.store.snapshot().state_dir)).unwrap();
    assert_eq!(persisted.status, Status::Failed);
    assert!(c.stop_child.load(Ordering::SeqCst));
    assert_eq!(c.private_result_error(), "current interrupt timeout");
    assert!(c.lock().session_closed);
}

#[test]
fn a_rejected_interrupt_writes_nothing_to_the_provider() {
    let (_dir, c) = controller(false);
    c.store
        .update(|s| {
            s.status = Status::Active;
            s.thread_id = Some("thread-1".into());
            s.turn_id = Some("turn-2".into());
        })
        .unwrap();
    let recorder = Recorder::default();
    c.attach_stdin(Box::new(recorder.clone()));
    let error = crate::control_server::interrupt(&c, "turn-1").unwrap_err();
    assert!(error.contains("interrupt was not sent"), "{error}");
    let steer = crate::control_server::steer(&c, "go", &[], "turn-1").unwrap_err();
    assert!(steer.contains("steer was not sent"), "{steer}");
    assert!(recorder.text().is_empty(), "{}", recorder.text());
}

#[test]
fn agent_output_keeps_order_and_turn_boundaries() {
    let (_dir, c) = controller(true);
    c.open_logs().unwrap();
    c.store.update(|s| s.turns = 2).unwrap();
    c.lock().turn_count = 2;
    c.append_output_separator(); // No leading rule before any output.
    c.record_agent_message("first").unwrap();
    c.record_agent_message("---").unwrap(); // A message may itself be a rule.
    c.append_output_separator();
    c.rollback_rejected_turn(2).unwrap();
    c.append_output_separator();
    c.append_output_separator(); // An accepted turn with no messages keeps its rule.
    c.record_agent_message("last\nline").unwrap();
    let path = c.store.snapshot().output_path;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "first\n\n---\n\n---\n\n---\n\nlast\nline\n"
    );
    assert_eq!(c.store.snapshot().turns, 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn an_output_failure_keeps_earlier_messages_and_the_pending_rule() {
    let (_dir, c) = controller(false);
    c.open_logs().unwrap();
    let path = std::path::PathBuf::from(c.store.snapshot().output_path);
    c.record_agent_message("first").unwrap();
    c.append_output_separator();
    let saved = path.with_extension("saved");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(c.record_agent_message("last").is_err());
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&saved, &path).unwrap();
    c.record_agent_message("last").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n\n---\n\nlast\n");
}

#[test]
fn a_log_open_failure_still_persists_a_terminal_state() {
    let (_dir, c) = controller(false);
    std::fs::create_dir(c.store.snapshot().events_path).unwrap();
    let error = c.open_logs().unwrap_err();
    c.fail(&error.message);
    let persisted = state::read_state(std::path::Path::new(&c.store.snapshot().state_dir)).unwrap();
    assert_eq!(persisted.status, Status::Failed);
    assert!(persisted.completed_at.is_some());
    assert_eq!(
        persisted.error.as_deref(),
        Some("turn failed; see trace.log and provider.stderr.log")
    );
}

#[test]
fn logs_refuse_existing_files() {
    let (_dir, c) = controller(false);
    let events = c.store.snapshot().events_path;
    std::fs::write(&events, "preserve me\n").unwrap();
    assert!(c.open_logs().is_err());
    assert_eq!(std::fs::read_to_string(events).unwrap(), "preserve me\n");
}

#[test]
fn an_idle_turn_never_publishes_a_terminal_session_state() {
    for status in [Status::Completed, Status::Failed, Status::Interrupted] {
        let (_dir, c) = controller(true);
        assert!(c.finish_turn(status, ""));
        let between = state::read_state(std::path::Path::new(&c.store.snapshot().state_dir)).unwrap();
        assert_eq!((between.status, between.completed_at.as_deref()), (Status::Idle, None), "{status}");
        assert_eq!(between.last_turn, Some(status));
        assert_eq!(c.interrupt_settlement_result().is_err(), status == Status::Failed, "{status}");
        c.persist_final_idle_exit();
        let last = state::read_state(std::path::Path::new(&c.store.snapshot().state_dir)).unwrap();
        assert_eq!(last.status, status);
        assert!(last.completed_at.is_some());
    }
}

#[test]
fn a_session_ending_status_overrides_the_last_turn() {
    let (_dir, c) = controller(true);
    assert!(c.finish_turn(Status::Completed, ""));
    c.end_session(Status::Interrupted, "");
    assert_eq!(c.store.snapshot().status, Status::Interrupted);
}

#[test]
fn each_trace_record_stays_on_one_line() {
    let (dir, c) = controller(false);
    let path = dir.join("trace.log");
    c.set_trace_file(std::fs::File::create(&path).unwrap());
    c.trace("[warn] provider failed\n2026-01-01T00:00:00Z [say] forged agent message");
    c.trace("[turn] completed");
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 2, "{raw}");
    assert!(
        lines[0].contains("[warn] provider failed 2026-01-01T00:00:00Z [say] forged"),
        "{raw}"
    );
}

#[cfg(unix)]
#[test]
fn closing_never_removes_a_socket_path_the_run_did_not_bind() {
    let (_dir, c) = controller(false);
    let socket = c.store.snapshot().socket_path;
    std::fs::write(&socket, "not a socket").unwrap();
    crate::control_server::close(&c);
    assert!(std::path::Path::new(&socket).exists());
}

#[test]
fn socket_parents_must_be_owner_only() {
    let dir = std::path::Path::new("state");
    crate::control_server::check_socket_parent_mode(dir, 0o40700).unwrap();
    let error = crate::control_server::check_socket_parent_mode(dir, 0o40755).unwrap_err();
    assert!(error.message.contains("must be owner-only, mode is 755"), "{error}");
}

#[cfg(unix)]
#[test]
fn the_accept_loop_retries_temporary_failures() {
    use crate::control_server::{Acceptor, Connection};
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    struct Sequence(Vec<io::Result<Box<dyn Connection>>>);
    impl Acceptor for Sequence {
        fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
            if self.0.is_empty() {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            self.0.remove(0)
        }
    }

    let (dir, c) = controller(false);
    c.set_trace_file(std::fs::File::create(dir.join("trace.log")).unwrap());
    let (server, mut client) = UnixStream::pair().unwrap();
    let acceptor = Sequence(vec![Err(io::Error::from(io::ErrorKind::ConnectionAborted)), Ok(Box::new(server))]);
    let closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread = crate::control_server::spawn_accept_loop(&c, Box::new(acceptor), closing.clone()).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    client.write_all(b"{\"command\":\"status\"}\n").unwrap();
    let mut reply = String::new();
    BufReader::new(&client)
        .read_line(&mut reply)
        .expect("the control loop did not recover after a temporary accept failure");
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["state"]["status"], "starting");
    closing.store(true, Ordering::SeqCst);
    thread.join().unwrap();
    assert!(
        std::fs::read_to_string(dir.join("trace.log"))
            .unwrap()
            .contains("[warn] temporary control accept error")
    );
}

#[test]
fn rpc_ids_accept_strings_and_integers_only() {
    for (raw, want) in [
        (json!("ruddr-1"), Some("ruddr-1")),
        (json!(42), Some("42")),
        (json!(-7), Some("-7")),
        (json!(""), None),
        (json!(1.5), None),
        (Value::Null, None),
        (json!(u64::MAX), None),
    ] {
        assert_eq!(rpc_id(Some(&raw)).as_deref(), want, "{raw}");
    }
    assert_eq!(rpc_id(None), None);
}

#[test]
fn provider_lines_are_bounded() {
    let mut input = io::Cursor::new(b"abc\r\nlast".to_vec());
    assert_eq!(read_line_limited(&mut input, 5).unwrap().unwrap(), b"abc");
    assert_eq!(read_line_limited(&mut input, 5).unwrap().unwrap(), b"last");
    assert!(read_line_limited(&mut input, 5).unwrap().is_none());
    let mut long = io::Cursor::new(b"abcdefgh\n".to_vec());
    assert!(read_line_limited(&mut long, 5).is_err());
}

#[test]
fn thread_requests_carry_adapter_fields() {
    let claude = RunConfig {
        provider: "claude".into(),
        claude_path: "/opt/claude".into(),
        ephemeral: true,
        resume_thread_id: "source".into(),
        ..Default::default()
    };
    let (method, mode, params) = crate::run::thread_request(&claude);
    assert_eq!((method, mode), ("thread/resume", "resumed"));
    assert_eq!(
        (params["persistSession"].as_bool(), params["claudePath"].as_str()),
        (Some(false), Some("/opt/claude"))
    );
    assert!(params.get("ephemeral").is_none() && params.get("serviceName").is_none() && params.get("model").is_none());
    let droid = RunConfig {
        provider: "droid".into(),
        provider_path: "/opt/droid".into(),
        model: "glm-5.3-flash".into(),
        ..Default::default()
    };
    let (method, _, params) = crate::run::thread_request(&droid);
    assert_eq!(method, "thread/start");
    assert_eq!(params["providerPath"], "/opt/droid");
    assert!(params.get("persistSession").is_none());
}
