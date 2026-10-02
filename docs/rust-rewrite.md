# Ruddr 0.6.0: the Rust rewrite

Ruddr 0.6.0 replaces the Go control plane and every Bun runtime piece (TUI,
web server, provider adapters) with one Rust binary, `ruddr`. Nothing needs
Bun or Go at runtime. The browser client stays TypeScript because browsers
run JavaScript; it is bundled once at development time and embedded in the
binary.

## Status

The port is complete. Ruddr 0.6.0 ships the Rust workspace described here,
and the Go and TypeScript implementations are gone from the tree; Git history
keeps them. `web/client` is the only TypeScript left. The README lists the
user-visible behavior changes under "Changes in 0.6.0". The rest of this
document is the design record of the port. Its ownership table, build
instructions, and report checklist describe how the port was run.

The Go and TypeScript sources stayed in the tree until the port landed, as the
reference specification. Behavior and tests were ported from them.

## Workspace

```
Cargo.toml                 workspace; the only place third-party versions live
crates/ruddr-core          shared contracts and pure helpers (integrator owns)
crates/ruddr-runner        `ruddr run`: the controller
crates/ruddr-adapters      hidden `ruddr app-server --provider NAME`
crates/ruddr-cli           the `ruddr` binary: dispatch and every other command
crates/ruddr-tui           `ruddr tui`
crates/ruddr-web           `ruddr web`
```

`crates/ruddr-cli/src/main.rs` routes the first argument to its owner. Each
owner parses its own flags and returns `ruddr_core::Result<()>`. The error
carries the exit code.

## Contracts that do not change

Agents, skills, the web client, and runs started by older releases depend on
these. Keep them byte-compatible unless this document says otherwise.

- **Commands and flags.** Every command, subcommand, and long flag in
  `ruddr --help`, `ruddr run --help`, and the Go usage text keeps its name and
  meaning. Flags are GNU style: `--flag value` and `--flag=value`. The Go
  single-dash form (`-flag`) is dropped.
- **Exit codes.** 0 success, 1 failed, 2 usage, 3 still running, 4 stale.
- **Run directory files.** `state.json` (see `ruddr_core::state::RunState`),
  `events.jsonl`, `trace.log`, `output.md`, `provider.stderr.log`,
  `launch.stderr.log`, `prompt.md`, `.ruddr.claim`. Same names, same formats,
  same `0700` directories and `0600` files. `state.json` stays redacted: IDs,
  paths, lifecycle metadata, counts, timestamps, PID, generic errors only.
- **Default run locations.** `CWD/.scratch/ruddr/<YYYYMMDD-HHMMSS>-<hex>` for
  `run` without `--state-dir`; `CWD/.scratch/ruddr-tui/...` for runs started by
  the TUI and web dashboard. Both parents ignore themselves in Git
  (`ruddr_core::paths::ensure_ignored_runs_dir`).
- **The global registry** (`ruddr_core::registry`) and config files under
  `~/.config/ruddr` (`models.json`, `tui.json`, `web-token`).
- **Environment variables**, including the `RUDDER_*` and `CODEX_RUDDER_*`
  aliases from earlier releases.
- **Release asset names** `ruddr-<goos>-<goarch>[.exe]` (darwin/linux/windows,
  amd64/arm64) plus `checksums.txt`, so the npm launcher and `ruddr update`
  keep working.

## Invariants (from AGENTS.md; all still apply)

- Never read or persist provider OAuth tokens, broker secrets, bearer tokens,
  refresh tokens, or auth files. Authentication belongs to the child command.
- Every `turn/steer` carries both `threadId` and `expectedTurnId`. A rejected
  steer never becomes a new turn. Prompt routing never converts one route into
  another.
- Signal cancellation, watchdog expiry, and interrupt terminate the whole
  provider process tree, close logs, remove the socket, and persist a terminal
  state.
- Bound child stdin writes and RPC calls. Never hold a write lock across an
  unbounded operation.
- Preserve every completed `agentMessage` in `output.md`, in arrival order.
- A dead controller with non-terminal state reads as `stale`; wait and control
  commands fail promptly with exit 4.
- Thread semantics: fresh runs `thread/start`; `--resume-thread` uses
  `thread/resume` with `excludeTurns: true` and no start-only fields;
  `--fork-thread` uses `thread/fork` and must return a new ID;
  `--fork-before-turn` maps to `beforeTurnId`, `--fork-through-turn` to
  `lastTurnId`; resume and fork are exclusive; the two fork selectors are
  exclusive and need `--fork-thread`. Only Droid supports forks among the
  adapters, and Droid rejects the boundary selectors.
- Reject server-initiated interactive requests explicitly; never let them hang.
- `--remote` stays a thin `ssh` passthrough. Windows `--detach` keeps
  `CREATE_BREAKAWAY_FROM_JOB`.

## What changes

- One binary. The adapters run as `ruddr app-server --provider NAME`
  (the runner spawns its own executable), replacing `bun run <provider>/main.ts`.
  They keep speaking line-delimited JSON-RPC on stdio like `codex app-server`.
- The Claude adapter speaks the `claude` CLI's stream-json protocol directly
  instead of using `@anthropic-ai/claude-agent-sdk`.
- The TUI and web dashboard steer, prompt, stop, and interrupt through
  `ruddr_core::control` directly instead of spawning `ruddr steer`.
- `tui --rs` and `RUDDR_TUI_ENTRY`/`RUDDR_WEB_ENTRY` go away. `--beta` and
  `--mobile` stay.
- The control channel uses the `interprocess` crate: a Unix socket on Unix, a
  named pipe on Windows (`socket_path` then holds `\\.\pipe\ruddr-<hex>`).

## Dependencies

Only the crates listed in the root `Cargo.toml` `[workspace.dependencies]`.
Use `default-features = false` where a crate allows it. Adding a crate needs
the integrator's approval: name it and the reason in your report instead of
adding it silently. The web crate alone may use tokio and axum; everything
else is synchronous (std threads and channels).

## Building and disk

The Mac has about 5 GiB free. Build only through mbx, which shares compiled
crates across worktrees as APFS clones and caps its cache at 3 GiB:

```
mbx check -p ruddr-runner          # while iterating
mbx test -p ruddr-runner
mbx clippy -p ruddr-runner --all-targets -- -D warnings
```

Never run `cargo clean`, never create another target directory, and never
install packages globally. `target` in each worktree is a symlink mbx manages.

## Tests

Port the Go and TypeScript tests for what you own, keeping their intent:
fresh, resume, fork, steer, interrupt, watchdog, stale state, blocked writes,
temporary accept errors, redaction, ordered output, multi-run waits, exit
codes. Assert the JSON-RPC request a fake app-server observes, not only CLI
text. For lifecycle tests, assert the returned error and the persisted
terminal state, and where relevant socket cleanup and child termination.
Tests stay deterministic and offline.

Never start a real provider run (Codex, Claude, OpenCode, Pi, Droid): they
cost money. Use fakes. The integrator runs the live checks.

## Ownership

Each agent works in its own worktree on its own branch, cut from
`rust-rewrite`. Edit only the files you own. If you need something in a
shared file, make the smallest change, keep it additive, and list it in your
report.

| Agent | Branch / worktree | Owns | Ports |
|---|---|---|---|
| runner | `rr/runner`, `.scratch/worktrees/runner` | `crates/ruddr-runner`, `ruddr-core/src/models.rs`, `ruddr-core/src/provider.rs`, server side of `ruddr-core/src/control.rs` | runner.go, control.go, detach.go, output.go, process_*.go, models.go, provider.go |
| adapters | `rr/adapters`, `.scratch/worktrees/adapters` | `crates/ruddr-adapters` | adapter/, claude/, opencode/, pi/, droid/ |
| cli | `rr/cli`, `.scratch/worktrees/cli` | `crates/ruddr-cli` | main.go (non-run), group.go, result.go, registry.go (ps scan), thread_commands.go, skill.go, update.go, remote.go, the `models` command |
| tui | `rr/tui`, `.scratch/worktrees/tui` | `crates/ruddr-tui` | tui/*.ts parity, plus the streaming redesign below |
| web | `rr/web`, `.scratch/worktrees/web` | `crates/ruddr-web`, the committed client bundle | web/server.ts, web_command.go |

The integrator owns `ruddr-core` (except the files above), the root
`Cargo.toml`, the release workflow, npm packaging, docs, and removing the Go
and TypeScript sources at the end.

## TUI streaming design

The current Rust TUI polls and re-parses up to 768 KB every 200 ms and
rebuilds every row each frame. Replace that with the pattern Codex's own
ratatui TUI uses (`codex-rs/tui/src/streaming/`, `markdown_stream.rs`):

1. A reader thread per selected session keeps a byte offset into
   `events.jsonl`, reads only appended bytes, and sends complete lines over a
   channel. The main loop wakes on terminal input, new data, or an animation
   tick, and redraws only when dirty, capped at 60 fps.
2. The transcript keeps parser state and applies new events incrementally.
3. Agent text is newline-gated: completed lines go through markdown; the line
   still arriving shows as plain text. Committed lines drain on a ~50 ms tick
   and speed up when the queue grows.
4. Rendered lines are cached per entry by (entry version, width, theme); only
   the streaming entry re-renders.
5. Upgrade to ratatui 0.30 and crossterm 0.29 (workspace versions).

## Report

When done, reply with: commits made, files changed, the exact test commands
and results, every behavior that differs from the Go/TypeScript original and
why, dependencies or shared-file changes you need, and open issues.
