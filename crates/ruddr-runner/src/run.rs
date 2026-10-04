//! One controller run from start to exit: claim the state directory, open
//! the logs, start the child and the control channel, run the handshake and
//! the first turn, keep an idle session alive, and tear everything down in
//! order.

use crate::config::{RunConfig, validate_run_config};
use crate::controller::{Controller, DEFAULT_IDLE_TURN_START_TIMEOUT};
use crate::signals::CancelToken;
use crate::store::StateStore;
use ruddr_core::state::Status;
use ruddr_core::{Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Runs a controller in the foreground until the run ends. Cancelling
/// `cancel` interrupts the turn, ends the provider tree, and persists
/// `interrupted`.
pub fn run_controller(mut cfg: RunConfig, cancel: &CancelToken) -> Result<()> {
    validate_run_config(&mut cfg)?;
    let raw = std::fs::read(&cfg.prompt_file).map_err(|e| Error::failed(format!("read {}: {e}", cfg.prompt_file.display())))?;
    let prompt = String::from_utf8_lossy(&raw).into_owned();
    if prompt.trim().is_empty() {
        return Err(Error::failed("prompt file is empty"));
    }
    let store = StateStore::create(&cfg)?;
    let controller = Controller::new(cfg, store);
    let mut teardown = Teardown {
        controller: &controller,
        watcher: None,
    };

    if let Err(e) = controller.open_logs() {
        controller.fail(&e.message);
        return Err(e);
    }
    if let Err(e) = controller.start_child() {
        controller.fail(&e.message);
        return Err(e);
    }
    teardown.watcher = Some(watch_cancel(&controller, cancel));
    if let Err(e) = crate::control_server::start(&controller) {
        controller.fail(&e.message);
        return Err(e);
    }
    if let Err(e) = initialize(&controller, &prompt) {
        if cancel.is_cancelled() {
            return Err(Error::failed(format!("run interrupted: {e}")));
        }
        controller.fail(&e);
        return Err(Error::failed(e));
    }
    controller.wait_turn();
    if controller.cfg.idle {
        idle_loop(&controller);
    }
    let state = controller.store.snapshot();
    if state.status == Status::Completed {
        return Ok(());
    }
    let result = controller.private_result_error();
    if !result.is_empty() {
        return Err(Error::failed(result));
    }
    Err(Error::failed(
        state.error.unwrap_or_else(|| format!("turn ended with status {}", state.status)),
    ))
}

/// Tears a run down in the order Go's deferred calls did: stop the cancel
/// watcher, stop the child, close the logs, then close the control channel.
struct Teardown<'a> {
    controller: &'a Arc<Controller>,
    watcher: Option<Arc<AtomicBool>>,
}

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        if let Some(finished) = &self.watcher {
            finished.store(true, Ordering::SeqCst);
        }
        self.controller.shutdown_child();
        self.controller.close_logs();
        crate::control_server::close(self.controller);
    }
}

/// Cancels the session when `cancel` fires, until the returned flag is set.
fn watch_cancel(controller: &Arc<Controller>, cancel: &CancelToken) -> Arc<AtomicBool> {
    let finished = Arc::new(AtomicBool::new(false));
    let (flag, token, controller) = (finished.clone(), cancel.clone(), controller.clone());
    let _ = std::thread::Builder::new().name("ruddr-cancel".into()).spawn(move || {
        // The token wakes this thread at once; the timeout only bounds how
        // long it outlives the run.
        while !flag.load(Ordering::SeqCst) {
            if token.wait_timeout(Duration::from_millis(250)) {
                if !flag.load(Ordering::SeqCst) {
                    controller.cancel_session();
                }
                return;
            }
        }
    });
    finished
}

fn initialize(controller: &Controller, prompt: &str) -> std::result::Result<(), String> {
    let client = json!({
        "clientInfo": {"name": "ruddr", "title": "Ruddr", "version": ruddr_core::VERSION},
        "capabilities": {"experimentalApi": true},
    });
    controller
        .call("initialize", client, Duration::from_secs(30))
        .map_err(|e| format!("initialize provider: {e}"))?;
    controller.notify_rpc("initialized", json!({}))?;
    let (thread_id, mode) = acquire_thread(controller)?;
    controller
        .store
        .update(|state| state.thread_id = Some(thread_id.clone()))
        .map_err(|e| format!("persist thread id: {e}"))?;
    controller.trace(format!("[thread] {mode} {thread_id}"));
    let images: Vec<String> = controller.cfg.images.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    controller
        .start_turn(prompt, &images, Duration::from_secs(60))
        .map_err(|e| e.message)
}

/// The thread/start, thread/resume, or thread/fork parameters for a run.
pub fn thread_request(cfg: &RunConfig) -> (&'static str, &'static str, Value) {
    let mut params = json!({
        "cwd": cfg.cwd.to_string_lossy(),
        "approvalPolicy": cfg.approval_policy,
        "sandbox": cfg.sandbox,
        "provider": cfg.provider,
    });
    if !cfg.model.is_empty() {
        params["model"] = json!(cfg.model);
    }
    if cfg.provider == "claude" {
        params["persistSession"] = json!(!cfg.ephemeral);
        if !cfg.claude_path.is_empty() {
            params["claudePath"] = json!(cfg.claude_path);
        }
    }
    if !cfg.provider_path.is_empty() {
        params["providerPath"] = json!(cfg.provider_path);
    }
    if !cfg.resume_thread_id.is_empty() {
        // Resume continues the source thread and sends no start-only fields.
        params["threadId"] = json!(cfg.resume_thread_id);
        params["excludeTurns"] = json!(true);
        return ("thread/resume", "resumed", params);
    }
    params["ephemeral"] = json!(cfg.ephemeral);
    if !cfg.fork_thread_id.is_empty() {
        params["threadId"] = json!(cfg.fork_thread_id);
        params["excludeTurns"] = json!(true);
        if !cfg.fork_before_turn_id.is_empty() {
            params["beforeTurnId"] = json!(cfg.fork_before_turn_id);
        }
        if !cfg.fork_through_turn_id.is_empty() {
            params["lastTurnId"] = json!(cfg.fork_through_turn_id);
        }
        return ("thread/fork", "forked", params);
    }
    params["serviceName"] = json!("ruddr");
    ("thread/start", "started", params)
}

fn acquire_thread(controller: &Controller) -> std::result::Result<(String, &'static str), String> {
    let (method, mode, params) = thread_request(&controller.cfg);
    let result = controller
        .call(method, params, Duration::from_secs(60))
        .map_err(|e| format!("{method}: {e}"))?;
    let thread_id = crate::controller::str_at(&result, &["thread", "id"]);
    if thread_id.is_empty() {
        return Err(format!("{method} returned no thread id"));
    }
    if method == "thread/fork" && thread_id == controller.cfg.fork_thread_id {
        return Err("thread/fork returned the source thread id".into());
    }
    if controller.cfg.provider == "opencode" {
        let model = result.get("model").and_then(Value::as_str).unwrap_or_default();
        let effort = result.get("reasoningEffort").and_then(Value::as_str);
        controller
            .store
            .update(|state| {
                state.model = model.to_string();
                if controller.cfg.effort.is_empty() {
                    state.effort = effort.map(str::to_string);
                }
            })
            .map_err(|e| format!("persist OpenCode model: {e}"))?;
    }
    Ok((thread_id.to_string(), mode))
}

enum IdleEvent {
    Prompt(crate::controller::PromptRequest),
    Stop,
    SessionClosed,
    Timeout,
}

/// Keeps an idle session alive between turns, taking prompts and `stop`
/// from the control channel until the session ends.
fn idle_loop(controller: &Controller) {
    loop {
        {
            let lifecycle = controller.lock();
            if lifecycle.session_closed || lifecycle.session_ended {
                drop(lifecycle);
                controller.ensure_terminal_exit();
                return;
            }
            if lifecycle.stop_requested {
                drop(lifecycle);
                controller.trace("[shutdown] requested while idle");
                controller.persist_final_idle_exit();
                return;
            }
            let result = controller.store.update(|state| {
                state.status = Status::Idle;
                state.error = None;
                state.completed_at = None;
            });
            if let Err(e) = result {
                drop(lifecycle);
                controller.fail(&format!("persist idle state: {e}"));
                return;
            }
        }
        controller.trace("[idle] waiting for prompt");
        let timeout = controller.cfg.idle_timeout;
        let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
        let event = {
            let mut lifecycle = controller.lock();
            lifecycle.idle_waiting = true;
            let mut event = None;
            let (mut lifecycle, _) = controller.wait_for(lifecycle, deadline, |l| {
                event = if let Some(request) = l.prompt_slot.take() {
                    Some(IdleEvent::Prompt(request))
                } else if l.stop_requested {
                    Some(IdleEvent::Stop)
                } else if l.session_closed {
                    Some(IdleEvent::SessionClosed)
                } else {
                    None
                };
                event.is_some()
            });
            lifecycle.idle_waiting = false;
            event.unwrap_or(IdleEvent::Timeout)
        };
        match event {
            IdleEvent::Prompt(request) => {
                if !start_prompted_turn(controller, request) {
                    return;
                }
            }
            IdleEvent::Stop => {
                controller.trace("[shutdown] requested while idle");
                controller.persist_final_idle_exit();
                return;
            }
            IdleEvent::Timeout => {
                controller.trace(format!("[idle] timeout after {}", ruddr_core::duration::format(timeout)));
                controller.persist_final_idle_exit();
                return;
            }
            IdleEvent::SessionClosed => {
                controller.ensure_terminal_exit();
                return;
            }
        }
    }
}

/// Starts the turn for a prompt from the control channel and answers it.
/// Returns false when the session must end.
fn start_prompted_turn(controller: &Controller, request: crate::controller::PromptRequest) -> bool {
    let reply = |result: std::result::Result<(), String>| {
        controller.lock().prompt_replies.insert(request.id, result);
        controller.notify();
    };
    let mut accepted = false;
    let update = {
        let lifecycle = controller.lock();
        let open = !lifecycle.session_ended && !lifecycle.stop_requested;
        controller.store.update(|state| {
            if open && state.status == Status::Idle && state.turns == request.observed_turns {
                state.status = Status::Starting;
                state.turn_id = None;
                accepted = true;
            }
        })
    };
    let mut result = match update {
        Err(e) => Err(crate::controller::TurnStartError {
            ambiguous: false,
            message: e.message,
        }),
        Ok(()) if !accepted => Err(crate::controller::TurnStartError {
            ambiguous: false,
            message: "session left the observed idle turn before the prompt was accepted".into(),
        }),
        Ok(()) => Ok(()),
    };
    if result.is_ok() {
        let timeout = controller.cfg.idle_turn_start_timeout.unwrap_or(DEFAULT_IDLE_TURN_START_TIMEOUT);
        result = controller.start_turn(&request.text, &request.images, timeout);
    }
    let Err(error) = result else {
        reply(Ok(()));
        controller.wait_turn();
        return true;
    };
    controller.trace(format!("[error] prompt turn failed to start: {}", error.message));
    if error.ambiguous {
        controller.stop_child.store(true, Ordering::SeqCst);
        controller.fail(&error.message);
        controller.terminate(false);
        reply(Err(error.message));
        return false;
    }
    if accepted {
        let restore = {
            let lifecycle = controller.lock();
            let open = !lifecycle.session_ended;
            controller.store.update(|state| {
                if open && state.status == Status::Starting {
                    state.status = Status::Idle;
                }
            })
        };
        if let Err(e) = restore {
            // The prompt's caller sees the session end instead of a reply.
            controller.fail(&format!("restore idle state: {e}"));
            return false;
        }
    }
    reply(Err(error.message));
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn teardown_stops_a_child_even_when_initialization_never_finished() {
        let dir = TempDir::new("setup-teardown");
        let cfg = RunConfig {
            state_dir: dir.join("run"),
            cwd: dir.to_path_buf(),
            child_command: vec!["sleep".into(), "30".into()],
            ..Default::default()
        };
        let store = StateStore::create(&cfg).unwrap();
        let controller = Controller::new(cfg, store);
        controller.open_logs().unwrap();
        controller.start_child().unwrap();
        let pid = controller.child_pid();
        controller.fail("setup did not finish");
        drop(Teardown {
            controller: &controller,
            watcher: None,
        });
        assert!(!ruddr_core::process::alive(pid as i64));
        assert_eq!(
            ruddr_core::state::read_state(&controller.cfg.state_dir).unwrap().status,
            Status::Failed
        );
    }
}
