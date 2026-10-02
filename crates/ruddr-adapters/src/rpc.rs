//! The request/response core shared by the Pi and Droid clients: a provider
//! CLI that speaks JSON lines on stdio, with requests matched to responses by
//! ID. Port of the common half of `SubprocessPiClient` and
//! `SubprocessDroidClient`.
//!
//! Every call and every write has a deadline. A call that times out, a write
//! that fails, unreadable output, or EOF fails the whole process: it is
//! terminated, every pending call fails with the same message, and the
//! adapter gets a failure event (unless the client is closing).

use crate::child::ChildProcess;
use crate::protocol::{LineReader, MAX_LINE_BYTES, lock};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Called with each provider event, on the client's reader thread. It must
/// not block; adapters hand the event to their own worker thread.
pub type EventFn = Arc<dyn Fn(Map<String, Value>) + Send + Sync>;

/// What one line of provider output is.
pub enum Incoming {
    /// The answer to a call: the response ID and its outcome.
    Response(String, Result<Map<String, Value>, String>),
    /// A server-initiated request; the value is the reply to write back.
    Reply(Value),
    /// A provider event for the adapter.
    Event(Map<String, Value>),
    Skip,
}

/// The wording of a provider's errors.
pub struct Labels {
    /// "Pi RPC" or "Droid": prefixes write timeouts and output failures.
    pub prefix: &'static str,
    /// "Pi RPC process" or "Droid process".
    pub process: &'static str,
    /// "Pi RPC client" or "Droid client".
    pub client: &'static str,
    /// What a non-object line is called in the parse error.
    pub message: &'static str,
}

type Pending = HashMap<String, SyncSender<Result<Map<String, Value>, String>>>;

pub struct RpcProcess {
    labels: Labels,
    rpc_timeout: Duration,
    child: Mutex<Option<Arc<ChildProcess>>>,
    pending: Mutex<Pending>,
    closing: AtomicBool,
    on_event: EventFn,
    failure_event: fn(&str) -> Map<String, Value>,
}

impl RpcProcess {
    pub fn spawn(
        command: Command,
        labels: Labels,
        rpc_timeout: Duration,
        classify: fn(&Map<String, Value>) -> Incoming,
        on_event: EventFn,
        failure_event: fn(&str) -> Map<String, Value>,
    ) -> std::io::Result<Arc<RpcProcess>> {
        let (child, stdout) = ChildProcess::spawn(command)?;
        let process = Arc::new(RpcProcess {
            labels,
            rpc_timeout,
            child: Mutex::new(Some(child)),
            pending: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            on_event,
            failure_event,
        });
        let reader = process.clone();
        thread::spawn(move || reader.read(stdout, classify));
        Ok(process)
    }

    /// Sends `message` and waits for the response with ID `id`.
    /// `timeout_message` is the error when no response arrives in time.
    pub fn call(&self, id: &str, message: &Value, timeout: Duration, timeout_message: String) -> Result<Map<String, Value>, String> {
        let child = lock(&self.child)
            .clone()
            .ok_or_else(|| format!("{} is not running", self.labels.process))?;
        let (sender, receiver) = mpsc::sync_channel(1);
        lock(&self.pending).insert(id.to_string(), sender);
        if let Err(error) = self.write(&child, message, timeout) {
            lock(&self.pending).remove(id);
            self.fail(&child, &error);
            return Err(error);
        }
        match receiver.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                if lock(&self.pending).remove(id).is_none() {
                    // The response won the race with the deadline.
                    return receiver.try_recv().unwrap_or(Err(timeout_message));
                }
                self.fail(&child, &timeout_message);
                Err(timeout_message)
            }
            Err(RecvTimeoutError::Disconnected) => Err(format!("{} is not running", self.labels.process)),
        }
    }

    fn write(&self, child: &ChildProcess, message: &Value, timeout: Duration) -> Result<(), String> {
        let line = serde_json::to_vec(message).unwrap_or_default();
        child.write(line, timeout).map_err(|error| {
            if error.starts_with("write timed out") {
                format!("{} write timed out after {}ms", self.labels.prefix, timeout.as_millis())
            } else if lock(&self.child)
                .as_ref()
                .is_some_and(|current| std::ptr::eq(current.as_ref(), child))
            {
                format!("{} {error}", self.labels.prefix)
            } else {
                format!("{} is not running", self.labels.process)
            }
        })
    }

    fn read(self: Arc<Self>, stdout: std::process::ChildStdout, classify: fn(&Map<String, Value>) -> Incoming) {
        let Some(child) = lock(&self.child).clone() else { return };
        let mut lines = LineReader::new(stdout, MAX_LINE_BYTES);
        let failure = loop {
            let line = match lines.next_line() {
                Ok(Some(line)) => line,
                Ok(None) => break format!("{} output closed", self.labels.prefix),
                Err(error) => break error.to_string(),
            };
            if line.trim().is_empty() {
                continue;
            }
            let message = match serde_json::from_str::<Value>(&line) {
                Ok(Value::Object(message)) => message,
                Ok(_) => break format!("{} must be an object", self.labels.message),
                Err(error) => break error.to_string(),
            };
            match classify(&message) {
                Incoming::Response(id, outcome) => {
                    if let Some(sender) = lock(&self.pending).remove(&id) {
                        let _ = sender.send(outcome);
                    }
                }
                Incoming::Reply(reply) => {
                    // Answered off the reader thread so a stuck stdin never
                    // stops responses from being read.
                    let (process, child) = (self.clone(), child.clone());
                    thread::spawn(move || {
                        if let Err(error) = process.write(&child, &reply, process.rpc_timeout) {
                            process.fail(&child, &error);
                        }
                    });
                }
                Incoming::Event(event) => (self.on_event)(event),
                Incoming::Skip => {}
            }
        };
        self.fail(&child, &failure);
    }

    fn fail(&self, child: &Arc<ChildProcess>, message: &str) {
        {
            let mut current = lock(&self.child);
            if !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, child)) {
                return;
            }
            current.take();
        }
        self.reject_all(message);
        child.shut_down(Duration::ZERO);
        if !self.closing.load(Ordering::SeqCst) {
            (self.on_event)((self.failure_event)(message));
        }
    }

    fn reject_all(&self, message: &str) {
        for (_, sender) in lock(&self.pending).drain() {
            let _ = sender.send(Err(message.to_string()));
        }
    }

    /// Fails pending calls, closes stdin, waits `grace` for a clean exit,
    /// then terminates the process.
    pub fn close(&self, grace: Duration) {
        self.closing.store(true, Ordering::SeqCst);
        let child = lock(&self.child).take();
        self.reject_all(&format!("{} is closing", self.labels.client));
        if let Some(child) = child {
            child.shut_down(grace);
        }
    }
}
