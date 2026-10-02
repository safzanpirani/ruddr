//! Tests for the run commands: selection and discovery, group and single
//! waits, results, peek, and the control commands against fake controllers.
//! Port of group_test.go, wait_test.go, and friction_test.go.

use super::result;
use super::runs::{self, RunRef, Selection, WaitOptions, discover_runs, wait_for_run_state, wait_for_runs};
use super::steering::{self, Action};
use interprocess::local_socket::{ListenerOptions, prelude::*};
use ruddr_core::control::{Command, Request, Response};
use ruddr_core::state::{RunState, Status};
use ruddr_core::{Exit, Result};
use serde_json::{Value, json};
use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(1);
/// A pid no test machine uses, so the controller reads as dead.
const DEAD_PID: i64 = 999_999_999;

struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Temp {
        // A short base keeps socket paths under the Unix limit.
        let dir = std::env::temp_dir().join(format!("rc-{name}-{}", ruddr_core::fsutil::random_hex(3)));
        std::fs::create_dir_all(&dir).unwrap();
        Temp(std::fs::canonicalize(&dir).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn exit_of(result: &Result<()>) -> Exit {
    match result {
        Ok(()) => Exit::Success,
        Err(error) => error.exit,
    }
}

fn message_of(result: &Result<()>) -> String {
    result.as_ref().err().map(|e| e.message.clone()).unwrap_or_default()
}

/// Writes `state.json` for `dir` with `fields` merged over a minimal state.
fn write_state(dir: &Path, fields: Value) {
    std::fs::create_dir_all(dir).unwrap();
    let mut state = json!({
        "version": 2, "provider": "codex", "pid": 1, "status": "active",
        "stateDir": dir.display().to_string(),
        "startedAt": "2026-10-02T09:00:00Z", "updatedAt": "2026-10-02T09:06:12Z",
    });
    for (key, value) in fields.as_object().unwrap() {
        state[key] = value.clone();
    }
    std::fs::write(dir.join("state.json"), serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn always(_: i64) -> bool {
    true
}

fn never(_: i64) -> bool {
    false
}

fn names(refs: &[RunRef]) -> Vec<String> {
    refs.iter().map(|r| r.name.clone()).collect()
}

#[test]
fn discovery_finds_nested_runs_within_depth() {
    let root = Temp::new("discover");
    let r = root.path();
    write_state(&r.join("ui/run"), json!({"status": "completed"}));
    write_state(&r.join("api/run"), json!({"status": "completed"}));
    // A state directory is a leaf: nothing below it is another run.
    write_state(&r.join("api/run/nested"), json!({"status": "completed"}));
    write_state(&r.join("a/b/c/d/run"), json!({"status": "completed"}));
    std::fs::write(r.join("ui/brief.md"), "x").unwrap();
    assert_eq!(names(&discover_runs(r).unwrap()), vec!["api/run", "ui/run"]);
}

#[test]
fn discovery_accepts_a_run_directory_as_root() {
    let root = Temp::new("solo");
    let dir = root.path().join("solo");
    write_state(&dir, json!({"status": "completed"}));
    assert_eq!(names(&discover_runs(&dir).unwrap()), vec!["solo"]);
}

#[test]
fn discovery_does_not_follow_symlinks() {
    #[cfg(unix)]
    {
        let root = Temp::new("links");
        let real = root.path().join("real/run");
        write_state(&real, json!({"status": "completed"}));
        let swarm = root.path().join("swarm");
        std::fs::create_dir_all(&swarm).unwrap();
        std::os::unix::fs::symlink(root.path().join("real"), swarm.join("link")).unwrap();
        assert!(discover_runs(&swarm).unwrap().is_empty());
    }
}

#[test]
fn selection_keeps_single_run_mode() {
    let cases: [(&[&str], &[&str], bool); 5] = [
        (&[], &[], true),
        (&["a"], &[], true),
        (&["a", "b"], &[], false),
        (&[], &["r"], false),
        (&["a"], &["r"], false),
    ];
    for (dirs, roots, single) in cases {
        let selection = Selection {
            state_dirs: strings(dirs),
            roots: strings(roots),
        };
        assert_eq!(selection.single().is_some(), single, "dirs={dirs:?} roots={roots:?}");
    }
}

#[test]
fn resolve_deduplicates_and_rejects_empty_roots() {
    let root = Temp::new("resolve");
    let dir = root.path().join("api");
    write_state(&dir, json!({"status": "completed"}));
    let dir_text = dir.display().to_string();
    let selection = Selection {
        state_dirs: vec![dir_text.clone()],
        roots: vec![root.path().display().to_string()],
    };
    let refs = selection.resolve().unwrap();
    assert_eq!(names(&refs), vec![dir_text]);
    let empty = Temp::new("empty");
    let error = Selection {
        roots: vec![empty.path().display().to_string()],
        ..Default::default()
    }
    .resolve()
    .unwrap_err();
    assert!(error.message.contains("no runs found"), "{}", error.message);
}

#[test]
fn group_wait_reports_every_run_and_fails_on_any_failure() {
    let root = Temp::new("report");
    write_state(
        &root.path().join("api"),
        json!({"pid": 1, "status": "completed", "model": "m", "turns": 1, "tokenUsage": {"totalTokens": 84200}}),
    );
    write_state(
        &root.path().join("ui"),
        json!({"pid": 2, "status": "failed", "error": "turn failed; see trace.log"}),
    );
    let refs = discover_runs(root.path()).unwrap();
    let mut out = Vec::new();
    let result = wait_for_runs(&mut out, &refs, None, WaitOptions::default(), &always, TICK);
    assert_eq!(message_of(&result), "1 of 2 runs did not complete");
    assert_eq!(exit_of(&result), Exit::Failed);
    let table = String::from_utf8(out).unwrap();
    let want = concat!(
        "NAME  STATUS     PROVIDER  MODEL  TURNS  TOKENS  ELAPSED  ERROR\n",
        "api   completed  codex     m      1      84.2K   6m12s    \n",
        "ui    failed     codex     -      -      -       6m12s    turn failed; see trace.log\n",
    );
    assert_eq!(table, want);
}

#[test]
fn group_wait_waits_for_the_slowest_run() {
    let root = Temp::new("slowest");
    let slow = root.path().join("slow");
    write_state(&root.path().join("fast"), json!({"pid": 1, "status": "completed"}));
    write_state(&slow, json!({"pid": 2, "status": "active"}));
    let refs = discover_runs(root.path()).unwrap();
    let polls = Cell::new(0);
    let alive = |_: i64| {
        polls.set(polls.get() + 1);
        if polls.get() == 3 {
            write_state(&slow, json!({"pid": 2, "status": "completed"}));
        }
        true
    };
    wait_for_runs(&mut Vec::new(), &refs, None, WaitOptions::default(), &alive, TICK).unwrap();
    assert!(polls.get() >= 3, "returned after {} polls", polls.get());
}

/// --any skips runs that had already finished, so repeated calls hand back
/// the runs one at a time.
#[test]
fn group_wait_any_waits_for_a_running_run() {
    let root = Temp::new("any");
    let busy = root.path().join("busy");
    write_state(
        &root.path().join("done"),
        json!({"pid": 1, "status": "failed", "error": "old failure"}),
    );
    write_state(&busy, json!({"pid": 2, "status": "active"}));
    write_state(&root.path().join("slow"), json!({"pid": 3, "status": "active"}));
    let refs = discover_runs(root.path()).unwrap();
    let polls = Cell::new(0);
    let alive = |_: i64| {
        polls.set(polls.get() + 1);
        if polls.get() == 4 {
            write_state(&busy, json!({"pid": 2, "status": "completed"}));
        }
        true
    };
    let mut out = Vec::new();
    wait_for_runs(&mut out, &refs, None, WaitOptions { any: true, turn: false }, &alive, TICK).expect("the earlier failure must not count");
    assert!(String::from_utf8(out).unwrap().contains("finished: busy\n"));
}

#[test]
fn group_wait_any_returns_at_once_when_nothing_runs() {
    let root = Temp::new("anynone");
    write_state(&root.path().join("a"), json!({"pid": 1, "status": "completed"}));
    write_state(&root.path().join("b"), json!({"pid": 2, "status": "failed"}));
    let refs = discover_runs(root.path()).unwrap();
    let result = wait_for_runs(&mut Vec::new(), &refs, None, WaitOptions { any: true, turn: false }, &always, TICK);
    assert_eq!(message_of(&result), "1 of 2 runs did not complete");
}

#[test]
fn group_wait_turn_judges_idle_sessions_by_their_last_turn() {
    let root = Temp::new("turn");
    write_state(
        &root.path().join("ok"),
        json!({"pid": 1, "status": "idle", "idle": true, "lastTurnStatus": "completed"}),
    );
    write_state(
        &root.path().join("bad"),
        json!({"pid": 2, "status": "idle", "idle": true, "lastTurnStatus": "failed"}),
    );
    let refs = discover_runs(root.path()).unwrap();
    let deadline = Some(Instant::now() + Duration::from_millis(20));
    let result = wait_for_runs(&mut Vec::new(), &refs, deadline, WaitOptions::default(), &always, TICK);
    assert!(message_of(&result).contains("timed out"), "a plain wait keeps waiting through idle");
    let mut out = Vec::new();
    let result = wait_for_runs(&mut out, &refs, None, WaitOptions { any: false, turn: true }, &always, TICK);
    assert_eq!(message_of(&result), "1 of 2 runs did not complete");
    assert!(String::from_utf8(out).unwrap().contains("last turn failed"));
}

#[test]
fn single_wait_turn_reports_the_last_turn() {
    let root = Temp::new("single");
    let dir = root.path();
    write_state(
        dir,
        json!({"pid": 1, "status": "idle", "idle": true, "lastTurnStatus": "completed"}),
    );
    let mut out = Vec::new();
    wait_for_run_state(&mut out, dir, None, true, &always, TICK).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "idle (last turn completed)\n");
    write_state(
        dir,
        json!({"pid": 1, "status": "idle", "idle": true, "lastTurnStatus": "interrupted"}),
    );
    let result = wait_for_run_state(&mut Vec::new(), dir, None, true, &always, TICK);
    assert!(message_of(&result).contains("interrupted"));
}

/// The controller persists its terminal state before exiting, so a wait that
/// sees a live-looking state and then a dead pid must re-read rather than
/// report a completed run as stale.
#[test]
fn single_wait_rereads_state_when_the_controller_exits_after_completing() {
    let root = Temp::new("reread");
    let dir = root.path();
    write_state(dir, json!({"pid": 4242, "status": "active"}));
    let alive = |_: i64| {
        write_state(dir, json!({"pid": 4242, "status": "completed"}));
        false
    };
    let mut out = Vec::new();
    wait_for_run_state(&mut out, dir, None, false, &alive, TICK).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "completed\n");
}

#[test]
fn single_wait_reports_stale_and_failed_states() {
    let root = Temp::new("stale");
    let dir = root.path();
    write_state(dir, json!({"pid": 4242, "status": "active"}));
    let result = wait_for_run_state(&mut Vec::new(), dir, None, false, &never, TICK);
    assert_eq!(exit_of(&result), Exit::Stale);
    assert!(message_of(&result).contains("stale"));

    let alive = |_: i64| {
        write_state(dir, json!({"pid": 4242, "status": "failed", "error": "turn failed; see trace.log"}));
        false
    };
    write_state(dir, json!({"pid": 4242, "status": "active"}));
    let result = wait_for_run_state(&mut Vec::new(), dir, None, false, &alive, TICK);
    assert_eq!(exit_of(&result), Exit::Failed);
    assert!(message_of(&result).contains("turn failed"));
}

#[test]
fn wait_exit_codes_separate_timeout_stale_and_failure() {
    let root = Temp::new("codes");
    let dir = root.path().join("single");
    write_state(&dir, json!({"pid": 1, "status": "active"}));
    let soon = || Some(Instant::now() + Duration::from_millis(5));
    assert_eq!(
        exit_of(&wait_for_run_state(&mut Vec::new(), &dir, soon(), false, &always, TICK)),
        Exit::Running
    );
    assert_eq!(
        exit_of(&wait_for_run_state(&mut Vec::new(), &dir, None, false, &never, TICK)),
        Exit::Stale
    );
    write_state(&dir, json!({"pid": 1, "status": "failed", "error": "turn failed"}));
    assert_eq!(
        exit_of(&wait_for_run_state(&mut Vec::new(), &dir, None, false, &always, TICK)),
        Exit::Failed
    );

    let group = root.path().join("group");
    write_state(&group.join("busy"), json!({"pid": 1, "status": "active"}));
    let refs = discover_runs(&group).unwrap();
    let result = wait_for_runs(&mut Vec::new(), &refs, soon(), WaitOptions::default(), &always, TICK);
    assert_eq!(exit_of(&result), Exit::Running);
    assert!(message_of(&result).contains("timed out: 1 of 1 runs still running"));
    write_state(&group.join("failed"), json!({"pid": 2, "status": "failed"}));
    let mut out = Vec::new();
    let result = wait_for_runs(&mut out, &refs, None, WaitOptions::default(), &never, TICK);
    assert_eq!(exit_of(&result), Exit::Stale);
    assert!(String::from_utf8(out).unwrap().contains("stale"));
}

#[test]
fn command_line_wait_rejects_bare_integer_timeouts() {
    let result = runs::wait(&mut Vec::new(), strings(&["--state-dir", "x", "--timeout", "30"]));
    assert_eq!(exit_of(&result), Exit::Usage);
}

#[test]
fn status_usage_errors_exit_two() {
    assert_eq!(exit_of(&runs::status(&mut Vec::new(), strings(&["--bogus"]))), Exit::Usage);
    assert_eq!(exit_of(&runs::status(&mut Vec::new(), Vec::new())), Exit::Usage);
    assert_eq!(exit_of(&runs::status(&mut Vec::new(), strings(&["-state-dir", "x"]))), Exit::Usage);
    assert_eq!(exit_of(&runs::status(&mut Vec::new(), strings(&["--help"]))), Exit::Success);
}

#[test]
fn single_status_prints_a_line_and_stale_state() {
    let root = Temp::new("status");
    let dir = root.path();
    write_state(
        dir,
        json!({"pid": DEAD_PID, "status": "active", "threadId": "th", "turnId": "tu", "steers": 2}),
    );
    let mut out = Vec::new();
    runs::status(&mut out, strings(&["--state-dir", &dir.display().to_string()])).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        text,
        format!(
            "stale provider=codex thread=th turn=tu pid={DEAD_PID} steers=2\nerror: Ruddr pid {DEAD_PID} is not running; persisted state is stale\n"
        )
    );
    let mut out = Vec::new();
    runs::status(&mut out, strings(&["--state-dir", &dir.display().to_string(), "--json"])).unwrap();
    let state: RunState = serde_json::from_slice(&out).unwrap();
    assert_eq!(state.status, Status::Stale);
}

#[test]
fn group_status_lists_unreadable_runs_without_failing() {
    let root = Temp::new("unreadable");
    write_state(&root.path().join("good"), json!({"pid": 1, "status": "completed"}));
    let bad = root.path().join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("state.json"), "{not json").unwrap();
    let mut out = Vec::new();
    runs::status(&mut out, strings(&["--root", &root.path().display().to_string(), "--json"])).unwrap();
    let states: Vec<Value> = serde_json::from_slice(&out).unwrap();
    assert_eq!(states[0]["status"], "unreadable");
    assert_eq!(states[1]["status"], "completed");
}

fn agent_message(text: &str) -> Value {
    json!({"method": "item/completed", "params": {"item": {"type": "agentMessage", "text": text}}})
}

fn write_events(path: &Path, events: &[Value]) {
    let mut text = String::new();
    for event in events {
        text.push_str(&serde_json::to_string(event).unwrap());
        text.push('\n');
    }
    std::fs::write(path, text).unwrap();
}

#[test]
fn last_agent_message_reads_only_the_latest_turn() {
    let root = Temp::new("events");
    let path = root.path().join("events.jsonl");
    let started = json!({"method": "turn/started", "params": {"turn": {"id": "t"}}});
    write_events(
        &path,
        &[
            started.clone(),
            agent_message("first answer"),
            started.clone(),
            agent_message("looking at it"),
            agent_message("final\n\nanswer"),
        ],
    );
    assert_eq!(result::last_agent_message(&path.display().to_string()).unwrap(), "final\n\nanswer");
    write_events(&path, &[started.clone(), agent_message("first answer"), started]);
    assert_eq!(result::last_agent_message(&path.display().to_string()).unwrap(), "");
}

#[test]
fn result_reports_answers_and_failures() {
    let root = Temp::new("result");
    let done = root.path().join("done");
    let events = done.join("events.jsonl");
    write_state(
        &done,
        json!({"pid": 1, "status": "completed", "eventsPath": events.display().to_string()}),
    );
    write_events(&events, &[agent_message("all fixed")]);
    write_state(
        &root.path().join("failed"),
        json!({"pid": 2, "status": "failed", "error": "turn failed; see trace.log"}),
    );
    let root_arg = root.path().display().to_string();

    let mut out = Vec::new();
    let outcome = result::run(&mut out, strings(&["--root", &root_arg]), &always);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "== done: completed ==\nall fixed\n\n== failed: failed ==\nerror: turn failed; see trace.log\n"
    );
    assert_eq!(message_of(&outcome), "1 of 2 runs did not complete");
    assert_eq!(exit_of(&outcome), Exit::Failed);

    let mut out = Vec::new();
    result::run(&mut out, strings(&["--state-dir", &done.display().to_string()]), &always).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "all fixed\n");

    let mut out = Vec::new();
    let outcome = result::run(&mut out, strings(&["--root", &root_arg, "--json"]), &always);
    assert!(outcome.is_err());
    let results: Vec<result::RunResult> = serde_json::from_slice(&out).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].message, "all fixed");
    assert!(!results[1].error.is_empty());
    assert_eq!(exit_of(&result::run(&mut Vec::new(), Vec::new(), &always)), Exit::Usage);
}

#[test]
fn result_exit_codes_rank_running_over_stale_over_failed() {
    let root = Temp::new("rescodes");
    let root_arg = root.path().display().to_string();
    write_state(
        &root.path().join("failed"),
        json!({"pid": 1, "status": "failed", "error": "turn failed"}),
    );
    assert_eq!(
        exit_of(&result::run(&mut Vec::new(), strings(&["--root", &root_arg]), &always)),
        Exit::Failed
    );
    write_state(&root.path().join("dead"), json!({"pid": 2, "status": "active"}));
    let dead_only = |pid: i64| pid != 2;
    assert_eq!(
        exit_of(&result::run(&mut Vec::new(), strings(&["--root", &root_arg]), &dead_only)),
        Exit::Stale
    );
    write_state(&root.path().join("busy"), json!({"pid": 3, "status": "active"}));
    assert_eq!(
        exit_of(&result::run(&mut Vec::new(), strings(&["--root", &root_arg]), &dead_only)),
        Exit::Running
    );
}

#[test]
fn group_peek_prints_each_runs_trace() {
    let root = Temp::new("peek");
    for name in ["a", "b"] {
        let dir = root.path().join(name);
        let trace = dir.join("trace.log");
        write_state(
            &dir,
            json!({"pid": 1, "status": "completed", "tracePath": trace.display().to_string()}),
        );
        std::fs::write(&trace, format!("one\ntwo\nthree {name}\n")).unwrap();
    }
    let refs = discover_runs(root.path()).unwrap();
    let mut out = Vec::new();
    runs::group_peek(&mut out, &refs, 2, &always).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "== a: completed ==\ntwo\nthree a\n\n== b: completed ==\ntwo\nthree b\n"
    );

    let mut out = Vec::new();
    let dir = root.path().join("a").display().to_string();
    runs::peek(&mut out, strings(&["--state-dir", &dir, "-n", "1"])).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "three a\n");
}

#[test]
fn tail_reads_past_the_first_window_when_asked() {
    let root = Temp::new("tail");
    let path = root.path().join("trace.log");
    let line = "x".repeat(1000);
    let text: String = (0..600).map(|i| format!("{i} {line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let lines = runs::tail_lines(&path, 500).unwrap();
    assert_eq!(lines.len(), 500);
    assert!(lines[0].starts_with("100 "));
}

#[test]
fn group_control_skips_runs_in_other_states() {
    let root = Temp::new("skip");
    write_state(&root.path().join("done"), json!({"pid": 1, "status": "completed"}));
    let refs = discover_runs(root.path()).unwrap();
    let mut out = Vec::new();
    steering::broadcast(&mut out, &refs, Action::Stop, Duration::from_secs(1), &always).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "done: skipped (status=completed)\nno idle runs to stop\n"
    );
}

#[test]
fn interrupt_rejects_an_expected_turn_for_several_runs() {
    let root = Temp::new("several");
    let result = steering::interrupt(
        &mut Vec::new(),
        strings(&["--root", &root.path().display().to_string(), "--expected-turn-id", "turn-1"]),
    );
    assert!(message_of(&result).contains("applies to one run"));
}

/// A controller that answers each control request with `reply` and records
/// every request it sees.
struct FakeController {
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeController {
    fn start(dir: &Path, state: Value, reply: impl Fn(&Request, &RunState) -> Response + Send + 'static) -> FakeController {
        // Controllers listen on a Unix socket in the state directory, or on a
        // named pipe on Windows.
        #[cfg(unix)]
        let socket = dir.join(".ruddr.sock").display().to_string();
        #[cfg(windows)]
        let socket = format!(r"\\.\pipe\ruddr-test-{}", ruddr_core::fsutil::random_hex(6));
        let mut fields = state;
        fields["pid"] = json!(std::process::id());
        fields["socketPath"] = json!(socket);
        write_state(dir, fields);
        let name = ruddr_core::control::socket_name(&socket).unwrap();
        let listener = ListenerOptions::new().name(name).create_sync().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(connection) = connection else { return };
                let mut reader = BufReader::new(connection);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let request: Request = serde_json::from_str(line.trim()).unwrap();
                let state = ruddr_core::state::read_state(&dir).unwrap();
                let response = reply(&request, &state);
                seen.lock().unwrap().push(request);
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                let _ = reader.get_mut().write_all(&bytes);
            }
        });
        FakeController { requests }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

fn accept(_: &Request, state: &RunState) -> Response {
    Response {
        ok: true,
        error: None,
        state: Some(state.clone()),
    }
}

#[test]
fn steer_sends_the_observed_turn_and_rejects_a_changed_one() {
    let root = Temp::new("steer");
    let dir = root.path();
    let controller = FakeController::start(dir, json!({"status": "active", "threadId": "th", "turnId": "turn-1"}), accept);
    let dir_arg = dir.display().to_string();
    let mut out = Vec::new();
    steering::steer(
        &mut out,
        strings(&["--state-dir", &dir_arg, "focus", "on", "parser.go"]),
        &mut std::io::empty(),
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "steered turn turn-1\n");

    let mut out = Vec::new();
    let result = steering::steer(
        &mut out,
        strings(&["--state-dir", &dir_arg, "--expected-turn-id", "turn-0", "late"]),
        &mut std::io::empty(),
    );
    assert!(message_of(&result).contains("active turn changed from turn-0 to turn-1; steer was not sent"));

    let mut out = Vec::new();
    steering::steer(
        &mut out,
        strings(&["--state-dir", &dir_arg, "--message-file", "-"]),
        &mut &b"  from stdin\n"[..],
    )
    .unwrap();

    let requests = controller.requests();
    assert_eq!(requests.len(), 2, "the rejected steer never reached the controller");
    assert_eq!(
        requests[0],
        Request {
            command: Command::Steer,
            text: Some("focus on parser.go".into()),
            expected_turn_id: Some("turn-1".into())
        }
    );
    assert_eq!(requests[1].text.as_deref(), Some("from stdin"));
    assert_eq!(
        exit_of(&steering::steer(
            &mut Vec::new(),
            strings(&["--state-dir", &dir_arg]),
            &mut std::io::empty()
        )),
        Exit::Usage
    );
}

#[test]
fn steer_and_prompt_never_swap_routes() {
    let root = Temp::new("routes");
    let idle = root.path().join("idle");
    let active = root.path().join("active");
    let idle_controller = FakeController::start(&idle, json!({"status": "idle", "idle": true}), accept);
    let active_controller = FakeController::start(&active, json!({"status": "active", "turnId": "t"}), accept);
    let steer_idle = steering::steer(
        &mut Vec::new(),
        strings(&["--state-dir", &idle.display().to_string(), "x"]),
        &mut std::io::empty(),
    );
    assert_eq!(message_of(&steer_idle), "turn is not steerable: status=idle");
    let prompt_active = steering::prompt(
        &mut Vec::new(),
        strings(&["--state-dir", &active.display().to_string(), "x"]),
        &mut std::io::empty(),
    );
    assert_eq!(message_of(&prompt_active), "a turn is active; use steer");
    assert!(idle_controller.requests().is_empty() && active_controller.requests().is_empty());

    let mut out = Vec::new();
    steering::prompt(
        &mut out,
        strings(&["--state-dir", &idle.display().to_string(), "next", "task"]),
        &mut std::io::empty(),
    )
    .unwrap();
    assert_eq!(
        idle_controller.requests()[0],
        Request {
            command: Command::Prompt,
            text: Some("next task".into()),
            expected_turn_id: None
        }
    );
}

#[test]
fn a_rejected_request_fails_with_the_controller_error() {
    let root = Temp::new("reject");
    let dir = root.path();
    let _controller = FakeController::start(dir, json!({"status": "active", "turnId": "t"}), |_, state| Response {
        ok: false,
        error: Some("turn is not steerable: status=idle".into()),
        state: Some(state.clone()),
    });
    let result = steering::steer(
        &mut Vec::new(),
        strings(&["--state-dir", &dir.display().to_string(), "x"]),
        &mut std::io::empty(),
    );
    assert_eq!(exit_of(&result), Exit::Failed);
    assert_eq!(message_of(&result), "turn is not steerable: status=idle");
}

#[test]
fn control_commands_on_a_dead_controller_exit_four() {
    let root = Temp::new("dead");
    let dir = root.path();
    write_state(
        dir,
        json!({"pid": DEAD_PID, "status": "active", "turnId": "t", "socketPath": dir.join(".ruddr.sock").display().to_string()}),
    );
    let dir_arg = dir.display().to_string();
    let started = Instant::now();
    let steer = steering::steer(&mut Vec::new(), strings(&["--state-dir", &dir_arg, "x"]), &mut std::io::empty());
    let prompt = steering::prompt(&mut Vec::new(), strings(&["--state-dir", &dir_arg, "x"]), &mut std::io::empty());
    let stop = steering::stop(&mut Vec::new(), strings(&["--state-dir", &dir_arg]));
    let interrupt = steering::interrupt(&mut Vec::new(), strings(&["--state-dir", &dir_arg]));
    for result in [&steer, &prompt, &stop, &interrupt] {
        assert_eq!(exit_of(result), Exit::Stale, "{}", message_of(result));
    }
    assert!(started.elapsed() < Duration::from_secs(2), "stale commands must fail promptly");
}

#[test]
fn interrupt_captures_the_current_turn() {
    let root = Temp::new("interrupt");
    let dir = root.path();
    let controller = FakeController::start(dir, json!({"status": "active", "turnId": "turn-7"}), accept);
    let mut out = Vec::new();
    steering::interrupt(&mut out, strings(&["--state-dir", &dir.display().to_string()])).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "interrupt requested for turn turn-7\n");
    assert_eq!(
        controller.requests()[0],
        Request {
            command: Command::Interrupt,
            text: None,
            expected_turn_id: Some("turn-7".into())
        }
    );
}

/// Two live idle sessions: status lists both, wait --turn returns, result
/// prints both answers, and stop reaches both controllers.
#[test]
fn group_commands_drive_several_live_runs() {
    let root = Temp::new("live");
    let mut controllers = Vec::new();
    let mut dirs = Vec::new();
    for name in ["first", "second"] {
        let dir = root.path().join(name);
        let events = dir.join("events.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        write_events(&events, &[agent_message(&format!("TURN 1 {name}"))]);
        let state = json!({"status": "idle", "idle": true, "lastTurnStatus": "completed", "eventsPath": events.display().to_string()});
        controllers.push(FakeController::start(&dir, state, accept));
        dirs.extend(["--state-dir".to_string(), dir.display().to_string()]);
    }

    let mut out = Vec::new();
    runs::status(&mut out, [vec!["--json".to_string()], dirs.clone()].concat()).unwrap();
    let states: Vec<RunState> = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        states.iter().map(|s| s.status).collect::<Vec<_>>(),
        vec![Status::Idle, Status::Idle]
    );
    assert_eq!(states[0].last_turn, Some(Status::Completed));

    let mut out = Vec::new();
    runs::wait(&mut out, [strings(&["--turn", "--timeout", "5s"]), dirs.clone()].concat()).unwrap();
    assert_eq!(String::from_utf8(out).unwrap().matches("idle").count(), 2);

    let mut out = Vec::new();
    result::run(&mut out, dirs.clone(), &runs::process_alive).unwrap();
    assert_eq!(String::from_utf8(out).unwrap().matches("TURN 1").count(), 2);

    let mut out = Vec::new();
    steering::stop(&mut out, dirs.clone()).unwrap();
    assert_eq!(String::from_utf8(out).unwrap().matches("shutdown requested").count(), 2);
    for controller in &controllers {
        assert_eq!(
            controller.requests(),
            vec![Request {
                command: Command::Stop,
                text: None,
                expected_turn_id: None
            }]
        );
    }

    let mut out = Vec::new();
    steering::interrupt(&mut out, dirs).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap().lines().last(),
        Some("no active runs to interrupt"),
        "idle sessions are skipped"
    );
}
