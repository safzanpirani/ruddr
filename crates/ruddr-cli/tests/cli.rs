//! End-to-end checks of the `ruddr` binary: usage and exit codes, `version`,
//! `skill install`, `thread` against fake app-servers, and `--remote`
//! through a fake ssh. Every test is offline; HOME, the registry, and the
//! update check point into a temporary directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Temp {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!("ruddr-cli-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Temp(dir)
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

/// `ruddr` with a private HOME and registry, and no update check.
fn ruddr(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ruddr"));
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("RUDDR_REGISTRY_DIR", home.join("state").join("runs"))
        .env("RUDDR_NO_UPDATE_CHECK", "1")
        .env_remove("RUDDR_SSH")
        .env_remove("RUDDR_REMOTE_RUDDR")
        .env_remove("RUDDR_REMOTE_SHELL")
        .stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> Output {
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(unix)]
fn write_executable(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn usage_and_exit_codes() {
    let home = Temp::new("usage");
    let none = run(&mut ruddr(home.path()));
    assert_eq!(none.status.code(), Some(2));
    assert!(text(&none.stderr).contains("Usage:"));
    assert!(text(&none.stderr).contains("ruddr: a command is required"));

    let help = run(ruddr(home.path()).arg("--help"));
    assert_eq!(help.status.code(), Some(0));
    assert!(text(&help.stderr).contains("--remote SSH_TARGET COMMAND"));
    assert!(!text(&help.stderr).contains("--rs"));

    assert_eq!(run(ruddr(home.path()).arg("bogus")).status.code(), Some(2));
    assert_eq!(run(ruddr(home.path()).args(["status", "--bogus"])).status.code(), Some(2));
    assert_eq!(run(ruddr(home.path()).arg("status")).status.code(), Some(2));
    let status_help = run(ruddr(home.path()).args(["status", "--help"]));
    assert_eq!(status_help.status.code(), Some(0));
    assert!(text(&status_help.stderr).contains("--state-dir DIR"));
    let missing = run(ruddr(home.path()).args(["status", "--state-dir", home.path().join("nope").to_str().unwrap()]));
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn version_prints_the_cached_update_notice() {
    let home = Temp::new("version");
    let plain = run(ruddr(home.path()).arg("version"));
    assert_eq!(plain.status.code(), Some(0));
    assert_eq!(text(&plain.stdout), format!("ruddr {}\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(text(&plain.stderr), "");

    // A fresh cache skips the network lookup and still reports the release.
    let cache = home.path().join("state").join("update-check.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let check = serde_json::json!({
        "checkedAt": ruddr_core::time::now_rfc3339(), "latest": "99.0.0", "current": env!("CARGO_PKG_VERSION"),
    });
    std::fs::write(&cache, check.to_string()).unwrap();
    let notice = run(ruddr(home.path()).arg("--version").env_remove("RUDDR_NO_UPDATE_CHECK"));
    assert_eq!(notice.status.code(), Some(0));
    assert!(
        text(&notice.stderr).contains("ruddr 99.0.0 is available"),
        "{}",
        text(&notice.stderr)
    );
    // RUDDR_NO_UPDATE_CHECK=1 hides it.
    assert_eq!(text(&run(ruddr(home.path()).arg("version")).stderr), "");
}

#[test]
fn skill_install_uses_dir_flags() {
    let home = Temp::new("skill");
    let (a, b) = (home.path().join("a"), home.path().join("b"));
    let output = run(ruddr(home.path()).args(["skill", "install", "--dir"]).arg(&a).arg("--dir").arg(&b));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    for dir in [&a, &b] {
        let installed = std::fs::read_to_string(dir.join("ruddr-delegate").join("SKILL.md")).unwrap();
        assert!(installed.starts_with("---\nname: ruddr-delegate\n"));
    }
    assert_eq!(text(&output.stdout).lines().count(), 2);
    let show = run(ruddr(home.path()).args(["skill", "show"]));
    assert!(text(&show.stdout).starts_with("---\nname: ruddr-delegate\n"));
    // Without --dir, the default directories under HOME.
    let defaults = run(ruddr(home.path()).args(["skill", "install"]));
    assert_eq!(defaults.status.code(), Some(0));
    assert!(home.path().join(".claude/skills/ruddr-delegate/SKILL.md").exists());
    assert!(home.path().join(".agents/skills/ruddr-delegate/SKILL.md").exists());
    assert!(!home.path().join(".codex").exists());
}

/// A response without a `result` member reports success, so actions such as
/// archive must not fail while decoding it.
#[cfg(unix)]
#[test]
fn thread_accepts_a_void_result() {
    let home = Temp::new("void");
    let server = home.path().join("server");
    write_executable(
        &server,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  [ -n "$id" ] && printf '{"id":"%s"}\n' "$id"
done
"#,
    );
    let output = run(ruddr(home.path()).args(["thread", "archive", "source-thread", "--"]).arg(&server));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "null\n");
}

/// The thread command initializes, refuses a server-initiated request
/// explicitly, sends the expected params, and prints the raw result
/// re-indented with its member order intact.
#[cfg(unix)]
#[test]
fn thread_list_refuses_interactive_requests_and_keeps_raw_results() {
    let home = Temp::new("list");
    let log = home.path().join("requests.log");
    let server = home.path().join("server");
    write_executable(
        &server,
        &format!(
            r#"#!/bin/sh
log='{}'
pwd > "$log.cwd"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*) printf '{{"id":"ruddr-query-1","result":{{"userAgent":"fake"}}}}\n' ;;
    *'"method":"thread/list"'*)
      printf '{{"method":"thread/started","params":{{}}}}\n'
      printf '{{"id":99,"method":"item/tool/requestUserInput","params":{{}}}}\n'
      IFS= read -r reply
      printf '%s\n' "$reply" >> "$log"
      printf '{{"id":"ruddr-query-2","result":{{"data":[{{"id":"t1","preview":"a,b"}}],"nextCursor":"c2","backwardsCursor":null,"amount":1.50}}}}\n'
      ;;
  esac
done
"#,
            log.display()
        ),
    );
    let workdir = home.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let output = run(ruddr(home.path())
        .args(["thread", "list", "--limit", "20", "--cwd-filter", "/w", "--cwd"])
        .arg(&workdir)
        .arg("--")
        .arg(&server));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let want = "{\n  \"data\": [\n    {\n      \"id\": \"t1\",\n      \"preview\": \"a,b\"\n    }\n  ],\n  \"nextCursor\": \"c2\",\n  \"backwardsCursor\": null,\n  \"amount\": 1.50\n}\n";
    assert_eq!(text(&output.stdout), want);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["method"], "initialize");
    assert_eq!(lines[0]["params"]["capabilities"]["experimentalApi"], true);
    assert_eq!(lines[1], serde_json::json!({"method": "initialized", "params": {}}));
    assert_eq!(lines[2]["method"], "thread/list");
    assert_eq!(lines[2]["id"], "ruddr-query-2");
    assert_eq!(lines[2]["params"], serde_json::json!({"cwd": "/w", "limit": 20}));
    assert_eq!(lines[3]["id"], 99);
    assert_eq!(lines[3]["error"]["code"], -32601);
    let cwd = std::fs::read_to_string(format!("{}.cwd", log.display())).unwrap();
    assert_eq!(Path::new(cwd.trim()).canonicalize().unwrap(), workdir.canonicalize().unwrap());
}

#[cfg(unix)]
#[test]
fn thread_reports_server_errors_and_early_exit() {
    let home = Temp::new("errors");
    let failing = home.path().join("failing");
    write_executable(
        &failing,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"id":"%s","result":{}}\n' "$id" ;;
    *) [ -n "$id" ] && printf '{"id":"%s","error":{"code":-32600,"message":"thread not found"}}\n' "$id" ;;
  esac
done
"#,
    );
    let output = run(ruddr(home.path()).args(["thread", "read", "T", "--"]).arg(&failing));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains("thread not found (-32600)"),
        "{}",
        text(&output.stderr)
    );

    let quitting = home.path().join("quitting");
    write_executable(&quitting, "#!/bin/sh\nexit 0\n");
    let output = run(ruddr(home.path()).args(["thread", "read", "T", "--"]).arg(&quitting));
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("initialize app-server"), "{stderr}");

    assert_eq!(
        run(ruddr(home.path()).args(["thread", "fork", "T", "--before-turn", "a", "--through-turn", "b"]))
            .status
            .code(),
        Some(1)
    );
    assert_eq!(run(ruddr(home.path()).args(["thread", "read", "T", "--"])).status.code(), Some(1));
}

/// The fake ssh runs the rendered command through sh, so this covers quoting,
/// stdin forwarding, output passthrough, and the remote exit status together.
#[cfg(unix)]
#[test]
fn remote_runs_through_ssh_and_propagates_the_exit_status() {
    let home = Temp::new("remote");
    let record = home.path().join("record");
    let fake_ruddr = home.path().join("fake-ruddr");
    write_executable(
        &fake_ruddr,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}.args'\ncat > '{0}.stdin'\necho remote-output\nexit 3\n",
            record.display()
        ),
    );
    let fake_ssh = home.path().join("ssh");
    write_executable(
        &fake_ssh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" \"$2\" \"$3\" > '{}.ssh'\nexec sh -c \"$4\"\n",
            record.display()
        ),
    );
    let message = home.path().join("message.md");
    std::fs::write(&message, "don't touch main.go").unwrap();
    let remote = |command: &mut Command| {
        command
            .env("RUDDR_SSH", &fake_ssh)
            .env("RUDDR_REMOTE_RUDDR", &fake_ruddr)
            .env("RUDDR_REMOTE_SHELL", "posix");
    };
    let read = |suffix: &str| std::fs::read_to_string(format!("{}.{suffix}", record.display())).unwrap();

    let mut command = ruddr(home.path());
    remote(&mut command);
    let output = run(command
        .args(["--remote", "ampere", "steer", "--state-dir", "run dir", "--message-file"])
        .arg(&message));
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "remote-output\n");
    assert_eq!(text(&output.stderr), "", "the remote status passes through without a second error");
    assert_eq!(read("ssh"), "-T\n--\nampere\n");
    assert_eq!(read("args"), "steer\n--state-dir\nrun dir\n--message-file\n-\n");
    assert_eq!(read("stdin"), "don't touch main.go");

    // --remote=TARGET works too, and run gains --detach and reads its prompt
    // from local stdin.
    let mut command = ruddr(home.path());
    remote(&mut command);
    let mut child = command
        .args(["--remote=user@host", "run", "--cwd", "~/proj", "--prompt-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"fix the bug\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(read("ssh"), "-T\n--\nuser@host\n");
    assert_eq!(
        read("args"),
        format!("run\n--detach\n--cwd\n{}/proj\n--prompt-file\n-\n", home.path().display())
    );
    assert_eq!(read("stdin"), "fix the bug\n");

    // Scalar flags use the last value. Only that local payload travels over
    // stdin, and a false detach flag cannot leave the remote run attached.
    let mut command = ruddr(home.path());
    remote(&mut command);
    let output = run(command
        .args([
            "--remote",
            "ampere",
            "run",
            "--cwd",
            "/remote/project",
            "--prompt-file",
            "/missing/ignored.md",
            "--detach=false",
        ])
        .arg(format!("--prompt-file={}", message.display()))
        .args(["--", "provider", "--detach=false"]));
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert_eq!(read("stdin"), "don't touch main.go");
    assert_eq!(
        read("args"),
        "run\n--cwd\n/remote/project\n--prompt-file\n/missing/ignored.md\n--detach=false\n--prompt-file=-\n--detach\n--\nprovider\n--detach=false\n"
    );
}

#[cfg(unix)]
#[test]
fn remote_args_after_tilde_expand_on_the_remote_side() {
    let home = Temp::new("tilde");
    let record = home.path().join("args");
    let fake_ruddr = home.path().join("fake-ruddr");
    write_executable(&fake_ruddr, &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", record.display()));
    let fake_ssh = home.path().join("ssh");
    write_executable(&fake_ssh, "#!/bin/sh\nexec sh -c \"$4\"\n");
    let output = run(ruddr(home.path())
        .env("RUDDR_SSH", &fake_ssh)
        .env("RUDDR_REMOTE_RUDDR", &fake_ruddr)
        .env("RUDDR_REMOTE_SHELL", "posix")
        .args(["--remote", "ampere", "status", "--state-dir", "~/runs/it's"]));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!("status\n--state-dir\n{}/runs/it's\n", home.path().display())
    );
}

#[test]
fn remote_rejects_bad_targets_and_missing_flags() {
    let home = Temp::new("badremote");
    let missing = run(ruddr(home.path()).arg("--remote"));
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing.stderr).contains("requires an SSH target"));
    let evil = run(ruddr(home.path()).args(["--remote", "-oProxyCommand=evil", "status"]));
    assert!(text(&evil.stderr).contains("invalid --remote SSH target"));
    let empty = run(ruddr(home.path()).arg("--remote="));
    assert!(text(&empty.stderr).contains("invalid --remote SSH target"));
    let no_cwd = run(ruddr(home.path())
        .env("RUDDR_REMOTE_SHELL", "posix")
        .args(["--remote", "h", "run", "--prompt-file", "x"]));
    assert!(text(&no_cwd.stderr).contains("--cwd is required"));
    let bad_shell = run(ruddr(home.path())
        .env("RUDDR_REMOTE_SHELL", "fish")
        .args(["--remote", "h", "status"]));
    assert!(text(&bad_shell.stderr).contains("must be posix or powershell"));
}

fn wait_state(dir: &Path, status: &str, pid: u32) {
    std::fs::create_dir_all(dir).unwrap();
    let state = serde_json::json!({
        "version": 2, "pid": pid, "status": status, "stateDir": dir,
        // Missing start time keeps table bytes stable across sequential waits.
        "startedAt": "", "updatedAt": "2026-10-02T09:01:00Z",
        "completedAt": "2026-10-02T09:01:00Z", "turns": 2,
    });
    ruddr_core::fsutil::write_private_atomic(&dir.join("state.json"), state.to_string().as_bytes()).unwrap();
}

#[test]
fn wait_progress_preserves_stdout_and_exit_codes() {
    let home = Temp::new("wait-progress");
    let dir = home.path().join("run");
    for group in [false, true] {
        for (status, pid, expected) in [
            ("active", std::process::id(), 3),
            ("completed", 999_999_999, 0),
            ("failed", 999_999_999, 1),
            ("active", 999_999_999, 4),
        ] {
            wait_state(&dir, status, pid);
            std::fs::write(dir.join("trace.log"), "first\nlatest observation\n").unwrap();
            let selection = if group { "--root" } else { "--state-dir" };
            let plain = run(ruddr(home.path()).args(["wait", selection]).arg(&dir).args(["--timeout", "60ms"]));
            let progress = run(ruddr(home.path())
                .args(["wait", selection])
                .arg(&dir)
                .args(["--timeout", "60ms", "--progress=20ms"]));
            assert_eq!(plain.status.code(), Some(expected));
            assert_eq!(progress.status.code(), plain.status.code());
            assert_eq!(progress.stdout, plain.stdout);
            if expected == 3 {
                let stderr = text(&progress.stderr);
                assert!(stderr.lines().filter(|line| line.starts_with("progress:")).count() >= 2, "{stderr}");
                assert!(stderr.contains("status=active turns=2 elapsed="), "{stderr}");
                assert!(stderr.contains(" activity="), "{stderr}");
                assert!(stderr.contains("trace=latest observation"), "{stderr}");
            }
            let errors: String = text(&progress.stderr)
                .lines()
                .filter(|line| !line.starts_with("progress:"))
                .map(|line| format!("{line}\n"))
                .collect();
            assert_eq!(errors, text(&plain.stderr));
        }
    }
    for value in ["5", "0s", "-1m", "bad"] {
        let output = run(ruddr(home.path())
            .args(["wait", "--state-dir"])
            .arg(&dir)
            .args(["--progress", value]));
        assert_eq!(output.status.code(), Some(2), "{value}: {}", text(&output.stderr));
    }
    let help = run(ruddr(home.path()).args(["wait", "--help"]));
    assert!(text(&help.stderr).contains("--progress DURATION"));
}

#[test]
fn wait_progress_reports_status_changes_before_the_interval() {
    use std::io::{BufRead, BufReader};
    for group in [false, true] {
        let home = Temp::new("wait-changes");
        let dir = home.path().join("run");
        wait_state(&dir, "starting", std::process::id());
        // An events-only run still reports activity; a trace need not exist.
        std::fs::write(dir.join("events.jsonl"), "{}\n").unwrap();
        let mut child = ruddr(home.path())
            .args(["wait", if group { "--root" } else { "--state-dir" }])
            .arg(&dir)
            .args(["--progress", "1h", "--timeout", "5s", "--turn"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        for status in ["starting", "active", "idle"] {
            let mut line = String::new();
            stderr.read_line(&mut line).unwrap();
            assert!(line.contains(&format!("status={status}")), "{line}");
            assert!(!line.contains("activity=unknown"), "{line}");
            assert!(line.contains("trace=unavailable"), "{line}");
            if status == "starting" {
                wait_state(&dir, "active", std::process::id());
            }
            if status == "active" {
                wait_state(&dir, "idle", std::process::id());
            }
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        let plain = run(ruddr(home.path())
            .args(["wait", if group { "--root" } else { "--state-dir" }])
            .arg(&dir)
            .arg("--turn"));
        assert_eq!(output.stdout, plain.stdout);
    }
}

#[test]
fn prune_dry_run_apply_json_and_bad_usage() {
    let home = Temp::new("prune");
    let registry = home.path().join("state/runs");
    let present = home.path().join("present");
    let absent = home.path().join("absent");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::create_dir(&present).unwrap();
    std::fs::write(present.join("trace.log"), "preserve me").unwrap();
    std::fs::write(registry.join("present.run"), present.to_str().unwrap()).unwrap();
    std::fs::write(registry.join("absent.run"), absent.to_str().unwrap()).unwrap();
    std::fs::write(registry.join("bad.run"), "").unwrap();
    let dry = run(ruddr(home.path()).arg("prune"));
    assert_eq!(dry.status.code(), Some(0));
    assert!(text(&dry.stdout).contains("would remove 1 registry entries; kept 1; unreadable 1"));
    assert!(registry.join("absent.run").exists());
    assert!(!registry.join(".registry.lock").exists(), "dry run must not write");
    let applied = run(ruddr(home.path()).args(["prune", "--apply", "--json"]));
    assert_eq!(applied.status.code(), Some(0), "{}", text(&applied.stderr));
    let report: serde_json::Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(report["applied"], true);
    assert_eq!(report["count"], 1);
    assert_eq!(report["kept"], 1);
    assert_eq!(report["unreadable"].as_array().unwrap().len(), 1);
    assert!(!registry.join("absent.run").exists());
    assert!(registry.join("present.run").exists());
    assert!(registry.join("bad.run").exists());
    assert_eq!(std::fs::read_to_string(present.join("trace.log")).unwrap(), "preserve me");
    for args in [["prune", "--bogus"], ["prune", "unexpected"]] {
        assert_eq!(run(ruddr(home.path()).args(args)).status.code(), Some(2));
    }
    let help = run(ruddr(home.path()).args(["prune", "--help"]));
    assert_eq!(help.status.code(), Some(0));
    assert!(text(&help.stderr).contains("--apply"));
}
