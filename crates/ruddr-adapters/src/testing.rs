//! Fake provider CLIs for tests. Each fake is this test binary run again
//! through a small shell wrapper: the wrapper records its argv, sets
//! `RUDDR_ADAPTERS_FAKE`, and runs only `testing::fake_provider_entry`. The
//! test harness prints to stdout, so the wrapper sends that to /dev/null and
//! hands the fake its real stdout as file descriptor 3. Unix only.

#![cfg(unix)]

use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FAKE_ENV: &str = "RUDDR_ADAPTERS_FAKE";
const FAKE_DIR_ENV: &str = "RUDDR_ADAPTERS_FAKE_DIR";

/// A scratch directory holding one fake's wrapper script and its records.
pub struct FakeDir {
    pub dir: PathBuf,
}

impl FakeDir {
    pub fn new() -> FakeDir {
        let dir = std::env::temp_dir().join(format!("ruddr-adapters-{}", ruddr_core::fsutil::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        FakeDir { dir }
    }

    /// Writes the wrapper for a fake of `kind` and returns its path.
    pub fn script(&self, kind: &str) -> String {
        let exe = std::env::current_exe().unwrap();
        let path = self.dir.join(format!("fake-{kind}"));
        let dir = self.dir.display();
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{dir}/argv'\nexport {FAKE_ENV}='{kind}'\nexport {FAKE_DIR_ENV}='{dir}'\n\
             exec '{}' --exact testing::fake_provider_entry --nocapture --test-threads=1 -q 3>&1 1>/dev/null\n",
            exe.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.path(name), contents).unwrap();
    }

    pub fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    pub fn argv(&self) -> Vec<String> {
        self.read("argv").lines().map(str::to_string).collect()
    }

    /// The JSON lines a fake recorded in `name`.
    pub fn records(&self, name: &str) -> Vec<Value> {
        self.read(name).lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    /// Waits up to 15 seconds for the recorded lines to satisfy `predicate`.
    pub fn wait_for(&self, name: &str, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let records = self.records(name);
            if predicate(&records) {
                return records;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {name}: {records:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn fake_provider_entry() {
    let Ok(kind) = std::env::var(FAKE_ENV) else { return };
    let dir = PathBuf::from(std::env::var(FAKE_DIR_ENV).unwrap());
    // SAFETY: the wrapper script opens descriptor 3 as this process's stdout.
    let out = Arc::new(Mutex::new(unsafe { File::from_raw_fd(3) }));
    let code = match kind.as_str() {
        "claude" => fake_claude(&dir, &out),
        other => panic!("unknown fake {other}"),
    };
    std::process::exit(code);
}

fn write(out: &Mutex<File>, message: &Value) {
    let mut file = out.lock().unwrap();
    let _ = writeln!(file, "{message}");
    let _ = file.flush();
}

fn append(path: &Path, line: &str) {
    let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn stdin_lines() -> impl Iterator<Item = Value> {
    BufReader::new(std::io::stdin())
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
}

/// A stand-in for `claude -p --input-format stream-json`. It records its
/// environment and every stdin line, answers `initialize`, and on the first
/// user message replays `replay.jsonl` from its directory. A prompt of
/// "hang" replays nothing. `exit_code` in the directory sets the exit status.
fn fake_claude(dir: &Path, out: &Mutex<File>) -> i32 {
    let env: Vec<String> = ["CLAUDE_CODE_ENTRYPOINT", "CLAUDE_AGENT_SDK_VERSION", "NODE_OPTIONS", "DEBUG"]
        .iter()
        .map(|name| format!("{name}={}", std::env::var(name).unwrap_or_else(|_| "<unset>".into())))
        .collect();
    std::fs::write(dir.join("env"), env.join("\n")).unwrap();
    std::fs::write(dir.join("cwd"), std::env::current_dir().unwrap().display().to_string()).unwrap();
    let mut replayed = false;
    for message in stdin_lines() {
        append(&dir.join("stdin.jsonl"), &message.to_string());
        if message["type"] == "control_request" && message["request"]["subtype"] == "initialize" {
            write(
                out,
                &json!({ "type": "control_response", "response": {
                    "subtype": "success", "request_id": message["request_id"], "response": { "commands": [], "models": [] },
                } }),
            );
        }
        if message["type"] == "user" && !replayed {
            replayed = true;
            if message["message"]["content"][0]["text"] == "hang" {
                continue;
            }
            let replay = std::fs::read_to_string(dir.join("replay.jsonl")).unwrap_or_default();
            for line in replay.lines().filter(|line| !line.trim().is_empty()) {
                let mut file = out.lock().unwrap();
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }
    std::fs::read_to_string(dir.join("exit_code"))
        .ok()
        .and_then(|code| code.trim().parse().ok())
        .unwrap_or(0)
}
