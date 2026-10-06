# AGENTS.md

Instructions for coding agents working in Codex Ruddr.

## Start here

Read `README.md` before changing behavior. It defines the user-facing CLI,
artifact contract, app-server composition, and supported Codex version. Inspect
`PAPERCUTS.md` for known workflow friction before debugging tooling failures.

This is a small Rust control plane around `codex app-server`, built as one
binary, `ruddr`. Keep it thin. Ruddr owns process lifecycle, JSON-RPC
transport, persisted run state, live steering, and thread-history operations;
it does not own authentication, model behavior, or repository business logic.
`docs/rust-rewrite.md` records the design of the 0.6.0 Rust port and the
contracts it kept.

## Repository map

`Cargo.toml` defines the workspace and is the only place third-party crate
versions live. Each crate's `src/lib.rs` or `src/main.rs` opens with a module
comment that says what it owns.

- `crates/ruddr-core` — shared contracts and pure helpers: the exit-code
  error type (`error.rs`), redacted `state.json` and stale-state rendering
  (`state.rs`), the client side of the control channel (`control.rs`; a Unix
  socket on Unix, a named pipe on Windows), line-delimited JSON-RPC
  (`jsonrpc.rs`), the global run registry and `prune` (`registry.rs`, which
  serializes registration and pruning with a lock file), session discovery
  for multi-run commands and the dashboards (`session.rs`), well-known paths
  and environment overrides (`paths.rs`), owner-only file helpers and
  file identity for detecting replaced logs (`fsutil.rs`), image attachments and the `localImage` input items
  (`images.rs`), omp's numbered edit diffs as unified hunks (`diff.rs`),
  Go-syntax durations capped at Go's range (`duration.rs`),
  provider selection, executable lookup, and `provider::command`, which
  resolves `.cmd` shims through PATHEXT on Windows (`provider.rs`), the model catalog (`models.rs`), and the
  theme list shared by the TUI and the web dashboard (`themes.json`).
- `crates/ruddr-runner` — `ruddr run`: flag parsing and usage text
  (`args.rs`), run configuration and validation (`config.rs`,
  `validate_run_config`), the long-lived controller (`controller.rs`,
  `run.rs`) with handshake, thread start/resume/fork, turns, JSON-RPC
  correlation, event handling, watchdog, logs, and shutdown, the server side
  of the control channel (`control_server.rs`), `run --detach` and
  `--prompt-file -` (`detach.rs`), append-only `output.md` writes
  (`output.rs`), platform process setup and process-tree termination
  (`process.rs`), signal handling (`signals.rs`), and the in-memory
  `state.json` copy (`store.rs`). `examples/fake_app_server.rs` is the fake
  app-server the lifecycle tests in `tests/` drive; extend it when adding
  protocol behavior.
- `crates/ruddr-adapters` — the hidden `ruddr app-server --provider NAME`
  command, which lets Claude Code, OpenCode 2, Pi, omp, and Factory Droid speak
  the Codex app-server protocol on stdio. `pi.rs` drives both Pi and omp
  (oh-my-pi, a Pi fork); `Flavor` selects omp's flags, settle event, and
  hashline edit reporting. `claude/cli.rs` drives the `claude`
  CLI's stream-json protocol directly. `droid.rs` drives `droid exec` in
  stream JSON-RPC mode; the README records the droid versions and Factory
  protocol versions it is verified against. `testing.rs` holds the fake
  provider CLIs the adapter tests use.
- `crates/ruddr-cli` — the `ruddr` binary. `src/main.rs` routes the first
  argument to the crate that owns the command. `src/commands/` holds every
  other command: the top-level usage text (`mod.rs`), the shared GNU-style
  flag parser (`args.rs`), `status`, `peek`, and `wait` for one run or a group
  (`runs.rs`), `wait --progress` reporting (`progress.rs`), `prune`
  (`prune.rs`), `steer`, `prompt`, `stop`, and `interrupt` (`steering.rs`),
  `result` (`result.rs`), `thread` (`thread.rs`), `models` (`models.rs`),
  `skill` (`skill.rs`, which embeds `skills/ruddr-delegate/SKILL.md`),
  `update` and the daily release check (`update.rs`, which installs through
  npm with the existing package's `--prefix`), and the `--remote`
  `ssh` passthrough (`remote.rs`). `tests/cli.rs` runs the built binary
  end to end, including `--remote` through a fake `ssh`.
- `crates/ruddr-tui` — `ruddr tui`, a ratatui front end. `lib.rs` holds the
  usage text and entry point, `app.rs` the state and input handling, `ui.rs`
  the rendering, including the mobile layout, `tail.rs` the per-session
  reader thread that streams appended `events.jsonl` lines, and `history.rs`
  the `H` list of every agent's past sessions, read-only, with the `e`
  filter for sessions that edited files.
- `crates/ruddr-history` — reads every local agent's session transcripts
  (Codex, Claude Code, Pi, omp, OpenCode, Factory Droid) whether or not Ruddr
  started them, and rebuilds their file edits as unified diffs. The parsers
  follow dejavu's. It never opens the auth files stored beside transcripts.
  The TUI and the web server both use it.
- `crates/ruddr-web` — `ruddr web`, the browser dashboard server (axum and
  tokio, the only async code in the workspace). It gates every API call on
  the token in `~/.config/ruddr/web-token` and serves the browser client
  embedded from `assets/`. `history.rs` serves the read-only history list
  and reads only sessions it listed or found itself, never a path the client
  sends. `tests/bundle.rs` fails when `web/client` changed without a rebuilt
  bundle.
- `web/index.html`, `web/client/` — the browser client, still TypeScript.
  `transcript.ts` folds `events.jsonl` into chat entries, `diffs.ts` wraps
  `@pierre/diffs` and `@pierre/trees`, and `markdown.ts` escapes all model
  text before formatting it. `bun scripts/build-web.ts` bundles it into
  `crates/ruddr-web/assets`, which is committed.
- `scripts/` — the local installer (`install-local.sh`), the npm launcher
  and postinstall hook (`npm-binary.cjs`, `npm-postinstall.cjs`, with
  `bin/ruddr.cjs`), the release helper that pins checksums
  (`npm-prepare.cjs`), the web bundler (`build-web.ts`), and the OpenCode
  theme sync (`sync-opencode-themes.ts`).

## Non-negotiable invariants

- Use only the crates listed in the root `Cargo.toml`
  `[workspace.dependencies]`, with `default-features = false` where a crate
  allows it. A new crate needs a stated reason and the maintainer's approval;
  name it and the reason in your report instead of adding it silently. Only
  `ruddr-web` may use tokio and axum; everything else stays synchronous (std
  threads and channels).
- Never read or persist Codex OAuth tokens, broker secrets, bearer tokens,
  refresh tokens, or auth files. Authentication belongs to the child command.
- `state.json` must remain content-redacted: IDs, paths, lifecycle metadata,
  counts, timestamps, PID, and generic errors only. Prompt/completion/tool text
  belongs only in private transcript artifacts.
- Create run directories and socket parents as `0700`; content-bearing files
  and state files as `0600`. Keep the Unix socket in a private parent and below
  the conservative cross-platform path-length limit.
- Every `turn/steer` request must include both `threadId` and
  `expectedTurnId`. Never turn a rejected steer into a replacement turn.
- Signal cancellation, watchdog expiry, and explicit interrupt must terminate
  the full app-server process tree, close logs safely, remove the socket, and
  persist a terminal state.
- Bound child stdin writes and RPC calls. Do not hold a write lock across an
  unbounded operation.
- Preserve every completed `agentMessage` in `output.md` in arrival order.
- Treat a dead controller with non-terminal persisted state as `stale`; wait and
  control commands must fail promptly with exit 4 rather than poll forever.
- Exit codes are an API that agents branch on: 0 success, 1 failed, 2 usage,
  3 still running, 4 stale. Keep them stable and document any new one in
  the top-level usage text, the README, and the skill.
- A default state directory lives under `CWD/.scratch/ruddr`, which carries its
  own `.gitignore` so run files never reach a sub-agent's `git status`. The
  TUI and web launch directory, `CWD/.scratch/ruddr-tui`, does the same.
- Flags are GNU style: `--flag value` and `--flag=value`. Keep every command,
  subcommand, and long flag name stable; agents and older releases depend on
  them.

## Thread semantics

- Fresh runs use `thread/start`.
- `--resume-thread` uses `thread/resume`, continues the source thread identity,
  sends `excludeTurns: true`, and must not send start-only fields such as
  `ephemeral` or `serviceName`.
- `--fork-thread` uses `thread/fork` and must return a new thread ID.
- `--fork-before-turn` maps only to `beforeTurnId` and excludes that turn and
  everything after it.
- `--fork-through-turn` maps only to `lastTurnId` and includes history through
  that turn.
- Resume and fork are mutually exclusive. The two fork boundary selectors are
  mutually exclusive and invalid without `--fork-thread`.
- Among the adapter providers, only Droid supports `--fork-thread`. A Droid
  fork copies the whole session, so the boundary selectors are rejected for
  it. `validate_run_config` rejects every fork flag for the other adapters.
- Conversation forks do not create Git worktrees or roll filesystem state back.
- Thread subcommands print raw app-server results as formatted JSON. Do not
  replace this with presentation-oriented output; callers depend on complete
  metadata and pagination cursors.

## Protocol changes

The app-server protocol is experimental. Before changing request fields,
method names, notification handling, or response shapes, generate schemas from
the installed CLI and compare them with the implementation:

```bash
schema_dir=$(mktemp -d /tmp/codex-app-server-schema.XXXXXX)
codex app-server generate-json-schema --experimental --out "$schema_dir"
codex --version
```

Update the compatibility statement in `README.md` when support is verified
against a newer CLI. Use string JSON-RPC IDs for Ruddr-originated calls, but
continue accepting valid string or numeric response IDs from the server.
Reject server-initiated interactive requests explicitly; do not let them hang.

If the run uses a command after `--` (for example the private auth broker), the
child must remain a transparent stdio-compatible app-server. Ruddr must never
special-case or inspect its credentials.

## Development workflow

Work from the repository root. Preserve unrelated user changes. Search exact
symbols with `rg`. Do not commit the `target/` directory, run directories,
sockets, schema dumps, or dogfood artifacts.

Ruddr builds with Rust stable (the workspace sets `rust-version = "1.88"`).
On a machine with `mbx` installed, run Cargo commands through it: `mbx`
takes the same arguments and shares compiled crates across checkouts. Never
run `cargo clean` or create a second target directory there.

After modifying Rust code, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p ruddr-cli          # target/debug/ruddr
git diff --check
```

The browser client in `web/client` is the only TypeScript left. Bun is a
development dependency for it and nothing else. After changing `web/client`,
`web/index.html`, `scripts/build-web.ts`, or the `@pierre/*` versions in
`package.json`, run:

```bash
bun install --frozen-lockfile --ignore-scripts
bunx tsc -p tsconfig.json --noEmit
bun test web scripts
bun scripts/build-web.ts          # rebuild crates/ruddr-web/assets
bun scripts/build-web.ts --check  # exit 1 when the committed bundle is stale
```

Commit the rebuilt `crates/ruddr-web/assets` with the client change. The Rust
test `bundle_matches_sources` fails when the bundle is stale.

CI runs the Rust job on Ubuntu, macOS, and Windows with `cargo test
--no-fail-fast`, so one run lists every failing test. Code behind
`cfg(windows)` never compiles on macOS or Linux: check it against the
`windows-sys` source in the Cargo registry and read the Windows CI job before
calling it done. Spawn npm-installed tools (`npm`, `bun`, `codex`, `claude`,
`opencode`, `pi`, `droid`) through `ruddr_core::provider::command`, because
`std::process::Command` does not find `.cmd` shims on Windows.

For documentation-only changes, at minimum run `git diff --check` and verify
every command against `target/debug/ruddr --help`, `target/debug/ruddr
COMMAND --help`, or the relevant parser.

## Keep every surface in sync

A change to the CLI or TUI shape is not finished until every place that
describes it matches. That includes a new or renamed command, flag, default,
model, output format, or TUI control. Update these in the same change, without
waiting to be asked:

1. **Usage text.** The top-level usage in `usage_text`
   (`crates/ruddr-cli/src/commands/mod.rs`), the flag help each command in
   `crates/ruddr-cli/src/commands` declares, `usage` in
   `crates/ruddr-runner/src/args.rs` (`run --help`), `USAGE` in
   `crates/ruddr-tui/src/lib.rs`, the usage text in
   `crates/ruddr-web/src/lib.rs`, and the skill usage in
   `crates/ruddr-cli/src/commands/skill.rs`.
2. **README.md.** The section for the feature, plus the Agent setup guide's
   operating manual when agent-facing behavior changes.
3. **The embedded skill.** `skills/ruddr-delegate/SKILL.md` teaches agents how
   to drive Ruddr. Update it for any change an agent would act on: launch
   flags, defaults, models, remote use, waiting, or steering. The binary
   embeds the working-tree copy at build time.
4. **The model catalog.** When a built-in default changes, update
   `crates/ruddr-core/src/models.rs` and its tests, and every skill that names
   the model. The TUI and the web dashboard read the catalog from `ruddr
   models --json`; there is no TypeScript copy. The web server falls back to
   `ruddr_core::models::builtin_catalog()`. The TUI still keeps its own
   fallback list in `fallback_models` (`crates/ruddr-tui/src/core.rs`), so
   update that list too. Users add or override models in
   `~/.config/ruddr/models.json` through `ruddr models add|default|remove`.
   Keep the built-in list short, and do not bulk-import provider model lists.
5. **Skills installed on this machine.** Run `cargo build -p ruddr-cli &&
   target/debug/ruddr skill install` so `~/.claude/skills`,
   `~/.agents/skills`, and `~/.codex/skills` get the new delegate skill. Other
   personal skills on this machine also drive Ruddr, such as the review/solve
   `*-auto` skills. Search the skill directories for `ruddr` and bring each one
   in line. Each such skill has one canonical copy; edit it, copy it over the
   others, and confirm the hashes match. Do not change a skill's behavior
   without the user's approval; updating command names, flags, and defaults
   is in scope.
6. **Distribution.** `ruddr update` reinstalls the skill after every update
   path, including when Ruddr is already current, and the npm postinstall,
   the npm launcher's first-run download, and `scripts/install-local.sh` do
   the same. Keep that true when changing install or update code, and cover
   it with a test.

Report which of these surfaces you updated. A local `skill install` of an
uncommitted edit only changes this machine. Other machines get it only after
the change is committed and released.

## TUI changes

Run `cargo test -p ruddr-tui` and the clippy command above. Tests do not cover
layout, so render the TUI before calling a visual change done:

```bash
cargo build -p ruddr-cli
tmux new-session -d -s ruddr-check -x 46 -y 34 "RUDDR_NO_UPDATE_CHECK=1 target/debug/ruddr tui"
tmux capture-pane -p -e -t ruddr-check    # -e keeps colors
```

Check both a phone width (at or below 64 columns, the mobile layout) and a
desktop width. To test touch targets, send SGR mouse events with a pause
between press and release, because back-to-back sends get coalesced:

```bash
tmux send-keys -t ruddr-check -l $'\e[<0;COL;ROWM'; sleep 0.2
tmux send-keys -t ruddr-check -l $'\e[<0;COL;ROWm'
```

Keep mobile controls large enough to tap. Action-bar buttons span three rows
and take the tap anywhere on the box. Do not submit prompts during a visual
check, because that starts real provider runs.

## Remote and detached runs

`--remote` must stay a thin `ssh` passthrough: no remote-side daemon and no
credential handling. It renders the command for either a POSIX shell or
PowerShell, based on a cached per-target probe. On Windows, `--detach` must
keep `CREATE_BREAKAWAY_FROM_JOB`: OpenSSH kills every process in a session's job
when the connection closes. The TUI launches sessions through `run --detach`
for the same reason. Test Windows behavior on a real Windows host over raw
`ssh`, one session per step; a reused shell hides session teardown. Paths after `--remote` are remote paths, and remote `run`
depends on `--detach`, so both ends need the same release. Test remote changes
with the fake `ssh` in `crates/ruddr-cli/tests/cli.rs`. A real host check such as `ruddr
--remote HOST status` is useful but read-only; do not start remote runs or
install binaries on shared hosts without asking.

## Testing expectations

- Add a regression test for every lifecycle, persistence, protocol-field, or
  thread-boundary bug.
- Prefer asserting the JSON-RPC request observed by the fake app-server, not
  only the final CLI text.
- For run lifecycle tests, assert both the returned error and persisted terminal
  state. Where relevant, also assert socket cleanup and child termination.
- Preserve coverage for fresh, resume, fork, steer, interrupt, watchdog, stale
  state, blocked writes, temporary accept errors, redaction, ordered output,
  multi-run waits, and exit codes.
- Duration flags use Go duration syntax (`3600s`, `20m`, `1h`); bare integers
  must remain invalid.
- Keep tests deterministic and offline. A live Codex dogfood run is useful
  before releases but does not replace fake-server regression coverage.

## Git and release hygiene

The canonical remote is `origin` at the public GitHub repository
`safzanpirani/ruddr`; the default branch is `main`. Do not change
visibility, add collaborators, publish releases, or push tags unless the user
asks. Never commit private broker URLs, secret-file contents, session
transcripts, or local run artifacts.

### Cutting a release

Release only when the user asks. A release is a version bump plus a tag push:

1. Commit the work in logical commits and run the full verification above.
2. Bump `version` under `[workspace.package]` in `Cargo.toml` and
   `"version"` in `package.json` together, refresh `Cargo.lock` with a build,
   and commit that alone as `Release X.Y.Z`. The `Release` workflow refuses a
   tag that does not match both.
3. Push `main`, then tag `vX.Y.Z` and push the tag. The `Release` workflow
   builds every platform, attaches binaries and `checksums.txt` to the GitHub
   release, and publishes the npm package with provenance.
4. Watch it with `gh run watch <id> --exit-status`, and confirm the five
   binaries on `gh release view vX.Y.Z`.
5. npm lags the publish by several minutes. The publish log's `+ ruddr@X.Y.Z`
   line is the proof that it succeeded; `npm view ruddr version --prefer-online`
   shows the new version only once processing finishes. Install on another
   machine with `npm install -g ruddr@X.Y.Z --prefer-online`, and expect
   `notarget` until then.
6. Update the machines that run Ruddr (`ruddr update`, or the npm command
   above), and confirm that `ruddr version` and the installed skill match.

Before handing off, report the files changed, exact verification commands and
results, remaining limitations, and whether changes are committed or pushed.
