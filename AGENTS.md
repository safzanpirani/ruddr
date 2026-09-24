# AGENTS.md

Instructions for coding agents working in Codex Ruddr.

## Start here

Read `README.md` before changing behavior. It defines the user-facing CLI,
artifact contract, app-server composition, and supported Codex version. Inspect
`PAPERCUTS.md` for known workflow friction before debugging tooling failures.

This is a small Go control plane around `codex app-server`. Keep it thin. Ruddr
owns process lifecycle, JSON-RPC transport, persisted run state, live steering,
and thread-history operations; it does not own authentication, model behavior,
or repository business logic.

## Repository map

- `main.go` — CLI dispatch, top-level usage text, and argument parsing for
  `run`, `steer`, `prompt`, `stop`, `status`, `peek`, `interrupt`, and `wait`.
- `remote.go` — `--remote SSH_TARGET` passthrough: runs any command through
  `ssh`, streams local prompt/message files over stdin, forces `run --detach`,
  and propagates the remote exit status.
- `detach.go` — `run --detach` (background controller in its own session,
  startup wait, `launch.stderr.log`) and `--prompt-file -` stdin prompts.
- `runner.go` — long-lived app-server controller, handshake, thread
  start/resume/fork, turn execution, JSON-RPC correlation, event handling,
  watchdog, logs, and shutdown.
- `control.go` — private Unix-socket control plane for live steer/interrupt and
  controller-liveness checks.
- `state.go` — owner-only run directories, redacted `state.json`, stale-state
  rendering, socket-path selection, and private file helpers.
- `thread_commands.go` — short-lived app-server sessions for thread discovery,
  search, read, turn listing, fork, naming, archive, and unarchive.
- `provider.go`, `models.go` — provider selection and the model catalog. The
  catalog is the source of truth for per-provider default models.
- `skill.go`, `skills/ruddr-delegate/SKILL.md` — the delegate skill, embedded
  in the binary and installed by `ruddr skill install`.
- `update.go` — release checks and `ruddr update`, which also reinstalls the
  skill.
- `tui_command.go`, `tui/` — the Bun/OpenTUI TUI. `tui/index.ts` builds the
  layout, including the mobile layout; `tui/core.ts` holds pure logic,
  argument parsing, and a fallback copy of the model catalog.
- `adapter/`, `claude/`, `opencode/`, `pi/` — Bun app-server adapters that let
  non-Codex providers speak the Codex app-server protocol.
- `process_unix.go`, `process_windows.go`, `process_other.go` — platform process
  setup, detached-process setup, and process-tree termination.
- `scripts/` — the local installer, npm launcher, and npm postinstall hook.
- `runner_test.go` — unit and integration-style tests using the in-process fake
  app-server. Extend this fake when adding protocol behavior.

## Non-negotiable invariants

- Prefer the Go standard library. Add a dependency only when its value clearly
  outweighs the maintenance and supply-chain cost.
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
- Bound child stdin writes and RPC calls. Do not hold `writeMu` across an
  unbounded operation.
- Preserve every completed `agentMessage` in `output.md` in arrival order.
- Treat a dead controller with non-terminal persisted state as `stale`; wait and
  control commands must fail promptly rather than poll forever.

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
symbols with `rg`; use `gofmt` for Go formatting. Do not commit the generated
`ruddr` binary, run directories, sockets, schema dumps, or dogfood artifacts.

After modifying Go code, run:

```bash
gofmt -w *.go
go test ./...
go vet ./...
go build -o ruddr .
git diff --check
```

The binary is ignored; remove or leave it untracked only if `.gitignore`
continues to cover it. For documentation-only changes, at minimum run
`git diff --check` and verify every command against `./ruddr --help` or the
relevant subcommand parser.

## Keep every surface in sync

A change to the CLI or TUI shape is not finished until every place that
describes it matches. That includes a new or renamed command, flag, default,
model, output format, or TUI control. Update these in the same change, without
waiting to be asked:

1. **Usage text.** `printUsage` in `main.go`, `printTUIUsage` in
   `tui_command.go`, `printSkillUsage` in `skill.go`, and any subcommand help.
2. **README.md.** The section for the feature, plus the Agent setup guide's
   operating manual when agent-facing behavior changes.
3. **The embedded skill.** `skills/ruddr-delegate/SKILL.md` teaches agents how
   to drive Ruddr. Update it for any change an agent would act on: launch
   flags, defaults, models, remote use, waiting, or steering. The binary
   embeds the working-tree copy.
4. **The model catalog.** When a default model changes, update `models.go`, the
   `FALLBACK_MODELS` copy in `tui/core.ts`, `models_test.go`, and every skill
   that names the model.
5. **Skills installed on this machine.** Run `go build -o ruddr . && ./ruddr
   skill install` so `~/.claude/skills`, `~/.agents/skills`, and
   `~/.codex/skills` get the new delegate skill. Other personal skills on this
   machine also drive Ruddr, such as the review/solve `*-auto` skills. Search
   the skill directories for `ruddr` and bring each one in line. Each such skill
   has one canonical copy; edit it, copy it over the others, and confirm the
   hashes match. Do not change a skill's behavior without the user's approval;
   updating command names, flags, and defaults is in scope.
6. **Distribution.** `ruddr update` reinstalls the skill after every update
   path, including when Ruddr is already current, and the npm postinstall and
   `scripts/install-local.sh` do the same. Keep that true when changing
   install or update code, and cover it with a test.

Report which of these surfaces you updated. A local `skill install` of an
uncommitted edit only changes this machine. Other machines get it only after
the change is committed and released.

## TUI changes

Install dependencies first (`bun install --frozen-lockfile --ignore-scripts`).
Then run `bun test` and `bunx tsc -p tsconfig.json --noEmit`. Tests do not
cover layout, so render the TUI before calling a visual change done:

```bash
go build -o ruddr .
tmux new-session -d -s ruddr-check -x 46 -y 34 "RUDDR_NO_UPDATE_CHECK=1 ./ruddr tui"
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
credential handling. Paths after `--remote` are remote paths, and remote `run`
depends on `--detach`, so both ends need the same release. Test remote changes
with the fake `ssh` in `remote_test.go`. A real host check such as `ruddr
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
  state, blocked writes, temporary accept errors, redaction, and ordered output.
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

Before handing off, report the files changed, exact verification commands and
results, remaining limitations, and whether changes are committed or pushed.
