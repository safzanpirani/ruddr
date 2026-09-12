# Ruddr

**A control plane for agents that run other agents.**

Ruddr is a small CLI that keeps a live handle on long-running Codex, Claude
Code, OpenCode 2, and Pi sessions. An orchestrating agent launches a turn in
the background, reads its progress from plain files, and redirects it mid-flight
over a local socket — no waiting for it to finish, no killing it and starting
over. Every command works the same from a human shell, so a person can watch or
steer too, but the primary operator is another agent.

![ruddr TUI dashboard](demo/tui-dashboard.png)

```bash
ruddr run --prompt-file task.md --state-dir run &   # start a turn
ruddr peek --state-dir run                          # watch it think
ruddr steer --state-dir run "tests first, skip the benchmark"
ruddr tui                                           # every session, live
```

## Why?

Agent harnesses increasingly delegate: one agent plans, spawns a coding agent
for the long turn, and keeps working while it runs. `codex exec --json` breaks
that loop — it is observable, but its stdin closes after the initial prompt, so
the orchestrator can only wait for the turn or kill it. Both waste the work
already done, and mid-flight discoveries (a wrong assumption, new user input, a
better plan) cannot reach the running turn.

Ruddr instead owns the provider connection and exposes a small local control
socket, so any later command — issued by the orchestrating agent, a cron job,
or a human — can steer the turn that is already in flight.

Agent-first by construction:

- **State is plain files** (`state.json`, `events.jsonl`, `output.md`) — an
  orchestrator polls or tails them without a TTY.
- **Commands speak JSON** where metadata matters (`ruddr thread ...`), so
  skills and scripts consume results without scraping.
- **Steering is just another CLI call** against the socket, safe to issue from
  a background task; it fails loudly if the turn is gone rather than starting a
  replacement.
- The TUI and human steering sit on top of the same contract, as the optional
  layer.

Providers:

- **Codex** — Ruddr owns a `codex app-server` connection and calls `turn/steer`
  on the running turn.
- **Claude Code** — Ruddr runs a small Bun adapter over the official Claude
  Agent SDK. Its persistent streaming-input queue makes steering part of the
  same live session, and structured SDK events expose summarized reasoning,
  assistant updates, and tool lifecycle to the same TUI used for Codex.
- **OpenCode 2** — Ruddr runs a private v2 server and uses its durable session
  inbox. Steering uses `delivery: "steer"` on the active session.
- **Pi** — Ruddr runs Pi in JSONL RPC mode. Pi exposes native steering,
  interruption, session persistence, streamed tool events, and usage totals.

All providers use the same state directory, commands, and TUI.

```text
task launcher ──stdio JSON-RPC──> provider app-server or adapter
      │
      ├── state.json / events.jsonl / trace.log / output.md
      │
      └── local Unix socket <── ruddr steer "focus on the failing test first"
```

The name is literal: Ruddr does not replace the engine, model, or auth
provider. It changes the heading of an in-flight turn.

## Status

Early working prototype. Each provider protocol evolves quickly. Reverify
Ruddr after provider upgrades. OpenCode support targets the 2.0 preview CLI
through `opencode2` or `opencode-next`. Ruddr does not support OpenCode 1 yet.

Adding a provider means implementing one adapter behind the existing
`--provider` flag. The state directory, control socket, steering commands, and
TUI are provider-agnostic already.

> Ruddr was previously published as Rudder and, before that, Codex Rudder.
> Settings, registries, and install locations from both earlier names keep
> working: `RUDDER_*` and `CODEX_RUDDER_*` environment variables are read as
> fallbacks, earlier `~/.config`, `~/.local/state`, and `~/.local/share`
> directories are searched after the new ones, and the `rudder` command stays
> as an alias.

## Install

The quickest route is the npm package, which ships the launcher, the TUI, and
the provider adapters, and fetches the prebuilt `ruddr` binary for your
platform from the matching GitHub release at install time:

```bash
npm install -g ruddr
# or
bun add -g ruddr
ruddr --help
```

Prebuilt binaries cover macOS (Apple Silicon and Intel), Linux (x64 and
arm64), and Windows x64. Every download is verified against the SHA-256
checksums pinned inside the published package. On any other platform, or when
the download is blocked, the launcher builds the bundled Go sources with a
local Go 1.24 toolchain instead. Set `RUDDR_BINARY` to use a binary you built
yourself, or `RUDDR_SKIP_DOWNLOAD=1` to skip the fetch and always build.

The install-time fetch only saves time on the first run. The launcher checks
for the binary on every invocation and downloads it when it is missing. An npm
policy that blocks install scripts therefore delays the first `ruddr` call and
changes nothing else. That policy also skips the bundled delegate skill. Run
`ruddr skill install` once after an install that reported blocked scripts.

`npm install -g` needs write access to the global prefix. Install into a user
prefix when it does not have that access:

```bash
npm install -g --prefix "$HOME/.local" ruddr
```

The Codex provider needs only the binary. `ruddr tui` and the Claude,
OpenCode, and Pi providers also need Bun 1.4 or newer on `PATH`.

### Updating

```bash
ruddr update --check   # report whether a newer release exists
ruddr update           # install it
```

Ruddr looks up the latest GitHub release at most once a day, on `ruddr version`
and in the background of `ruddr tui`, and caches the answer under
`~/.local/state/ruddr/update-check.json`. When a newer release exists, `ruddr
version` prints a notice, and the TUI shows a status toast, keeps the version
in the idle status line, and offers **Update Ruddr** in the command palette.
Set `RUDDR_NO_UPDATE_CHECK=1` to disable the check.
Failed automatic checks also wait a day before retrying and retain the last
known release. `ruddr update --check` always performs a fresh lookup.

`ruddr update` picks the install channel from where the binary lives: a global
npm or bun package is reinstalled at the new version through that tool, a
standalone binary is replaced in place after the download is verified against
the release checksums, and a source checkout installed with
`scripts/install-local.sh` is told to pull and rerun the installer, since its
TUI assets live apart from the binary.

Releases are cut by pushing a `vX.Y.Z` tag that matches the `version`
constant in `main.go`. The workflow builds every platform, attaches the
binaries and checksums to a GitHub release, and publishes the npm package.

## Build

```bash
bun install
go build -o ruddr .
```

Requires Go 1.24 and Bun 1.4 or newer. Codex runs require a CLI with
`codex app-server` and `turn/steer` support; that command surface is verified
against `codex-cli 0.145.0`. Claude runs use the pinned official Claude Agent
SDK and the caller's normal Claude Code authentication. OpenCode runs require
the `opencode2` or `opencode-next` executable.

Pi runs require the `pi` executable with RPC mode. The adapters inherit each
CLI's normal authentication environment.

## Run a task

Create a self-contained prompt and a run directory:

```bash
mkdir -p .scratch/ruddr-demo
$EDITOR .scratch/ruddr-demo/prompt.md

./ruddr run \
  --provider codex \
  --cwd "$PWD" \
  --prompt-file .scratch/ruddr-demo/prompt.md \
  --state-dir .scratch/ruddr-demo/run \
  --model gpt-6-astra \
  --sandbox workspace-write
```

`--provider` defaults to `codex`, so existing commands do not need to change.

Run Claude Code through the same control plane:

```bash
./ruddr run \
  --provider claude \
  --cwd "$PWD" \
  --prompt-file .scratch/ruddr-demo/prompt.md \
  --state-dir .scratch/ruddr-demo/claude.run \
  --effort high \
  --sandbox workspace-write
```

Omit `--model` to use Claude Code's configured default. Use
`--claude-path /absolute/path/to/claude` when the executable is not normally
discoverable, or set `RUDDR_CLAUDE_PATH` to make that choice persistent;
`--claude-path` takes precedence. Point either at a wrapper script when your
Claude authentication depends on shell or Keychain setup that a detached
process does not inherit. `read-only` maps to Claude's `plan` mode.
`workspace-write` uses `acceptEdits` and enables Claude's command sandbox. It
automatically allows Bash only when Claude runs the command inside that
sandbox, adds the working directory to the writable paths, and fails if the
sandbox is unavailable. `danger-full-access` maps to `bypassPermissions`.
Ruddr denies any operation that still requires interactive approval.

Run OpenCode 2 or Pi through the same surface:

```bash
./ruddr run --provider opencode --cwd "$PWD" \
  --prompt-file .scratch/ruddr-demo/prompt.md \
  --state-dir .scratch/ruddr-demo/opencode.run

./ruddr run --provider pi --cwd "$PWD" \
  --prompt-file .scratch/ruddr-demo/prompt.md \
  --state-dir .scratch/ruddr-demo/pi.run
```

The default model for both adapters is
`openrouter/deepseek/deepseek-v4-flash-vision-exp`. Use `--opencode-path` or
`RUDDR_OPENCODE_PATH` to select an OpenCode 2 executable. Use `--pi-path` or
`RUDDR_PI_PATH` to select a Pi executable. OpenCode installs private
Ruddr-scoped agents with explicit permission rules for each sandbox value. Pi
disables extensions and enables only its read, grep, find, and list tools for
`read-only`. Both adapters use their provider's native permission system. They
do not provide Ruddr-enforced filesystem containment for `workspace-write`.
Run these adapters only in trusted workspaces. OpenCode 2 loads project
configuration and plugins, and Pi loads project-local resources after approval.
Those resources can execute code outside the adapters' tool permission rules.

Run it in the background from an agent harness so the harness can continue
reading user messages and issue steering commands.

`--turn-timeout` defaults to one hour and stops a silently hung turn and its
child process group. Set it to `0` only when an unbounded run is intentional.
`SIGINT` and `SIGTERM` mark the run interrupted, terminate the app-server
process group, and remove the control socket before Ruddr exits.

Resume an existing provider thread or session for another steerable turn:

```bash
./ruddr run \
  --resume-thread THREAD_ID \
  --cwd "$PWD" \
  --prompt-file .scratch/ruddr-demo/prompt.md \
  --state-dir .scratch/ruddr-demo/resumed-run
```

Fork first when the new work should preserve the source thread:

```bash
./ruddr run \
  --fork-thread THREAD_ID \
  --fork-before-turn TURN_ID \
  --cwd "$PWD" \
  --prompt-file .scratch/ruddr-demo/prompt.md \
  --state-dir .scratch/ruddr-demo/forked-run
```

Use `--fork-through-turn TURN_ID` instead to include the selected turn.

## Discover and manage Codex threads

`ruddr thread` is Codex-specific. It prints the app-server result as JSON so
agent skills and shell scripts can consume pagination cursors and complete
metadata without scraping human-formatted output:

```bash
./ruddr thread list --limit 20 --cwd-filter "$PWD"
./ruddr thread search --limit 10 "parser regression"
./ruddr thread read --include-turns THREAD_ID
./ruddr thread turns --limit 20 THREAD_ID

./ruddr thread fork --before-turn TURN_ID THREAD_ID
./ruddr thread name THREAD_ID "Parser regression investigation"
./ruddr thread archive THREAD_ID
./ruddr thread unarchive THREAD_ID
```

Every thread subcommand also accepts a stdio-compatible child command after
`--`, using the same private auth-bridge composition as `ruddr run`.

## Steer the active turn

```bash
./ruddr steer \
  --state-dir .scratch/ruddr-demo/run \
  "New information: the regression starts in parser.go. Focus there first."
```

The command fails if the turn is no longer active or the active turn ID does
not match. It never silently starts a replacement turn.

For exact multiline input:

```bash
./ruddr steer --state-dir .scratch/ruddr-demo/run \
  --message-file .scratch/ruddr-demo/steer.md
```

Automation can pass `--expected-turn-id ID` to reject a steer when the selected
session advances to another active turn before submission.

## Idle sessions: multi-turn without new processes

`ruddr run --idle` keeps the controller and provider alive after a turn
finishes. The run's status becomes `idle` and the control socket accepts new
turns on the same thread:

```bash
./ruddr run --idle --prompt-file task.md --state-dir .scratch/demo/run &
./ruddr wait  --state-dir .scratch/demo/run   # blocks through idle; Ctrl+C to stop watching
./ruddr prompt --state-dir .scratch/demo/run "Now add tests for the fix."
./ruddr stop   --state-dir .scratch/demo/run  # graceful shutdown while idle
```

Rules:

- `prompt` works only while the session is idle; `steer` works only while a
  turn is active. Neither command is ever converted into the other.
- `interrupt` during a turn returns an idle session to `idle` instead of
  killing the provider; `stop` ends an idle session gracefully.
- `--idle-timeout` (default 4h) exits the session after that long idle; the
  final persisted status is the last turn's terminal status.
- `--turn-timeout` applies per turn.
- `output.md` separates turns with `---`; each prompt attempt is recorded as a
  synthetic `userMessage` item followed by an append-only decision event in
  `events.jsonl`. The TUI hides rejected attempts.
- state.json gains `idle`, `turns`, and `tokenUsage` (cumulative counts,
  context window, and cost when the provider reports one). Prompt text still
  never reaches state.json.

`ruddr models [--json]` prints the model catalog (providers, models, default
per provider) that the TUI's picker uses.

## Observe and control

```bash
./ruddr status --state-dir .scratch/ruddr-demo/run
./ruddr status --state-dir .scratch/ruddr-demo/run --json
./ruddr peek --state-dir .scratch/ruddr-demo/run -n 40
./ruddr wait --state-dir .scratch/ruddr-demo/run --timeout 20m
./ruddr interrupt --state-dir .scratch/ruddr-demo/run
```

Use `interrupt --expected-turn-id TURN_ID` to stop only the turn you observed.
If that turn has changed, Ruddr rejects the command. Without the flag, the CLI
captures the current turn ID before sending the control request. Delayed
interrupt failures also leave later turns running.

For a live fullscreen view of several runs, install the CLI and its TUI assets
once, then launch the dashboard from any directory:

```bash
./scripts/install-local.sh
ruddr tui
```

The dashboard shows live runs first, followed by every finished run from
Ruddr's private global registry, newest first. It also discovers `state.json`
files below `.scratch` in the directory where it was launched. New `ruddr run`
commands register themselves automatically. Scroll the list with the mouse
wheel or filter it with `/`. Point it at extra locations with repeatable
`--root DIR` and `--state-dir DIR` arguments (`--all` is still accepted and has
no effect):

```bash
./ruddr tui --root /path/to/project/.scratch
./ruddr tui --state-dir /path/to/one/run --state-dir /path/to/another/run
./ruddr tui --all
./ruddr tui --theme tokyonight
./ruddr tui --beta
```

The default TUI uses an at-a-glance dashboard. A persistent sessions pane shows
live and recent runs on the left. The right column shows session details, the
lowercase Chat, Activity, Output, and Diff tabs, and the selected artifact. The
prompt input spans the full width below the dashboard. Press `Tab` to switch
focus between the sessions pane and the selected artifact. Press `Esc` in the
sessions pane to return focus to the artifact.

Use `--beta` for the chat-first layout. `RUDDR_TUI_BETA=1` enables the same
layout.

On narrow terminals the TUI switches to a mobile layout on its own: a single
column with the sessions list as an overlay, no details panel, and a tappable
action bar (`≡ sessions`, `✎ prompt`, `■ stop`, `⋯ more`) in place of the key
hints, so a phone SSH client such as Blink or Termius can drive every action
by touch. The switch happens at or below 64 columns and reverses when the
window grows; set `mobileWidthThreshold` in `tui.json` to change the width, or
pass `--mobile` (or `RUDDR_TUI_MOBILE=1`) to force it at any size. Beta mode shows one session's borderless conversation and keeps the
sessions list behind a `Tab` overlay. `Enter` or `Esc` closes that overlay.

Typing in the prompt input routes by session status. An active turn gets a
steer. An idle `--idle` session gets a new turn over the control socket. A
finished session continues its thread in a fresh run. The prompt metadata shows
the selected session's model, token usage, cost, and status. For example, it
can show `gpt-6-astra · 186.1K (24%) · idle`.

The context meter uses the latest context usage reported by the provider.
Session token totals remain available in the details panel and are labeled
`total`; they never determine the context percentage. If current context usage
is unavailable, the footer shows the labeled session total without a meter.
The TUI can recover the last context reading from older Codex event logs.

`n` starts a brand-new session. Pick a provider and model in the T3-style
picker, type the first prompt,
and the TUI spawns a detached `ruddr run --idle` in the current directory.
`m` opens the same picker to override the model for continuations. When the
`deja` CLI is installed, `f` searches past Claude/Codex transcripts and resumes
a chosen session under ruddr.

`/` filters by project, thread, status, or model when the sessions pane has
focus. `/` searches the selected artifact when the artifact has focus. Chat,
Activity, Output, and Diff are clickable tabs (`o` cycles). Diff shows the
selected session's tracked staged and unstaged working-tree changes against
`HEAD` and refreshes while the TUI runs. Wide terminals also show a changed-file
tree with per-file line counts; selecting a file jumps to its patch. Drag the
divider beside the tree to reveal long paths or give the patch more room; Ruddr
remembers that width across launches. File status letters distinguish modified,
added, deleted, and renamed paths. The patch shows old and new line numbers in
a gutter, tints added and deleted lines, and renders each file as a banner with
its status and line counts. Patch lines get syntax coloring by file extension
from a dependency-free scanner that tracks block comments and multi-line
strings across a hunk and understands JSX and HTML tags, decorators, regex
literals, constants, and CSS, SQL, and Markdown; fenced code in Chat uses the
same scanner. Files the selected session edited since it started carry
a `●` marker in the banner and the tree. Use `]c` and `[c` to move between hunks, or
`]f` and `[f` to move between files. Enter, Space, or `z` folds and unfolds the
file under the cursor (clicking a file banner does the same), and `Z` folds or
unfolds every file. The Diff tab label carries the current `+added −deleted`
totals. Both panes support
mouse-wheel scrolling, `/` search with `n`/`N` match navigation, and `c` to copy
the selected row. In Activity, clicking selects a row and clicking a tool row
expands it; use the Output tab for normal mouse text selection. Enter also
expands Activity tool rows to show the full command, status, duration, working
directory, and captured output. Scrolling up
pauses follow mode; click the follow indicator or press End to return to live
output. While the selected session works, a spinner and elapsed time show in
the tab bar, the sessions list, and the bottom of Chat and Activity. Status
messages lead with a `✓`, `›`, `!`, or `×` glyph and clear themselves after a
few seconds. Chat renders agent Markdown (headings, lists, quotes, inline code,
and fenced code with syntax coloring) and types out the newest message as it
streams. The text is live: Codex reports partial assistant text as
`item/agentMessage/delta` and the Claude, OpenCode, and Pi adapters emit the
same notification, so a message appears while the model writes it rather than
all at once when the turn ends. Your steers appear in the transcript too.
Activity tool rows collapse to one line and expand into a card with
the command, status, working directory, input, and output. Empty states are
clickable: they start a session, open the prompt, or retry a failed diff read.
Right-click a session for a context menu: prompt or steer it, stop it, open its
chat or diff, copy its thread ID or state directory, and delete finished or
stale sessions. With a `/` filter active the menu also offers to delete every
finished session that matches, and it can clear all failed or stale sessions in
one step. Deletion asks for confirmation, removes the run's state directory and
registry entry, and never touches a live session. Right-click a patch or
activity row to copy it, fold its file, or expand the tool.
Press `:`, `?`, or `Ctrl+K` for the command palette, which lists every action
with its key and filters as you type. The prompt accepts multiple lines:
Enter sends, and Shift+Enter, Alt+Enter, or `Ctrl+J` insert a newline. The
meta line under the prompt shows a context-window meter that shifts from green
to yellow to red as it fills. Ruddr polls `git diff` less often while the
tree is quiet and remembers the sidebar width as a fraction of the pane so it
scales with the terminal. Classic mode shows compact session metadata by default. Press `i` to
cycle through expanded, hidden, and compact metadata. Beta mode starts with
metadata hidden and uses the same cycle.

Failed sends and early launch failures leave the prompt open for editing or an
explicit retry. A successful submission closes it only if the draft has not
changed. TUI launches retain `prompt.md` and `launch.stderr.log` in the private
run directory and surface startup errors. A controller that is still starting
after the brief launch check stays registered; Ruddr does not retry it.

Press `t` to open the theme picker. Moving through the list previews each
palette immediately; Enter saves the choice globally and Escape restores the
previous palette. Ruddr includes the 33 built-in OpenCode themes (using their
dark variants) alongside its original theme. `--theme NAME` or
`RUDDR_TUI_THEME=NAME` overrides the saved choice for one launch.

For an active run, `s` focuses the prompt box to steer and `x x` interrupts
(an idle session returns to idle; `x x` on an idle session ends it). For a
finished run, `s` or `shift+R` continues the same provider thread/session in a
fresh private run while preserving its working directory, model, effort, and
sandbox. `r` refreshes, `i` toggles the session details panel, and `q` exits.
State and
artifacts otherwise refresh every 500ms; override that with a duration such as
`--interval 2s`.

`ruddr tui` requires Bun 1.4 or newer and the optional `@opentui/core`
dependency. The installer places the binary in `~/.local/bin` and the TUI in
`~/.local/share/ruddr` by default; both locations honor the standard
`RUDDR_BIN_DIR` and `XDG_DATA_HOME` overrides. The launcher also finds a TUI
beside the binary or in the current checkout. For custom development paths, set
`RUDDR_TUI_ENTRY`, `RUDDR_CLAUDE_ADAPTER_ENTRY`,
`RUDDR_OPENCODE_ADAPTER_ENTRY`, or `RUDDR_PI_ADAPTER_ENTRY`. Legacy
`CODEX_RUDDER_*` variables and installed assets remain readable.

Run artifacts:

- `.ruddr.claim` — atomic ownership marker that prevents state-directory reuse.
- `state.json` — IDs, status, paths, and timestamps; no prompt or output text.
- `events.jsonl` — raw provider protocol events plus Ruddr prompt decisions.
- `trace.log` — compact human-readable progress.
- `output.md` — all completed `agentMessage` items appended in order.
- `provider.stderr.log` — child diagnostics (legacy runs retain their persisted
  `app-server.stderr.log` path).

The run directory and all files are owner-only (`0700` / `0600`). The control
socket lives inside that directory when the Unix path limit permits; otherwise
Ruddr creates a random owner-only temporary parent and records it in state.
The raw events, trace, and output can contain prompt, command, and completion
content. Persisted errors in `state.json` are generic; details remain in the
private trace and stderr logs.

Output appends avoid rewriting long transcripts. A reported partial write is
rolled back to the previous file length; an abrupt process or machine crash can
leave a partial final message, as with the other append-only logs. `state.json`
continues to use atomic replacement.

The global run registry stores only private state-directory references under
`~/.local/state/ruddr/runs` (or `XDG_STATE_HOME`). It does not duplicate
prompt, trace, output, or authentication content.
The TUI also reads the legacy `codex-rudder/runs` registry so existing history
does not disappear.

If a process is killed without cleanup, `status` renders a non-terminal state
as `stale`, while `wait`, `steer`, and `interrupt` fail promptly instead of
polling forever or returning an opaque socket error.

## Layer over codex-auth-broker

Ruddr accepts any stdio-compatible app-server command after `--`. This lets
the existing auth bridge keep ownership of OAuth refresh while Ruddr adds
lifecycle and steering:

```bash
./ruddr run \
  --cwd "$PWD" \
  --prompt-file .scratch/ruddr-demo/prompt.md \
  --state-dir .scratch/ruddr-demo/run \
  --model gpt-6-astra \
  --sandbox workspace-write \
  -- \
  /Users/safzan/Development/projects/codex-auth-broker-private/codex-auth-broker \
    app-server-bridge \
    -broker-auth-url http://100.121.157.57:8765/v1/codex/auth \
    -secret-file /Users/safzan/.codex/codex-auth-broker.secret
```

The process chain is:

```text
ruddr
  └─ codex-auth-broker app-server-bridge
       └─ codex app-server --listen stdio://
```

Ruddr never reads or persists the broker secret or Codex OAuth tokens. The
bridge consumes those and presents the same app-server JSON-RPC stream.

## Why not `codex exec resume`?

Resume adds a later turn after the current one completes. `turn/steer` appends
input to the currently in-flight regular turn, so Codex can change direction
after the current tool call and before it commits to a final answer.

Review and manual compaction turns can reject steering. `review-codex-auto`
should continue using a normal prompt-driven turn (not `review/start`) when it
needs the result to remain steerable.

## Protocol compatibility

The installed CLI can emit exact schemas for its version:

```bash
schema_dir=$(mktemp -d /tmp/codex-app-server-schema.XXXXXX)
codex app-server generate-json-schema --experimental --out "$schema_dir"
```

Treat those schemas as candidates, not a runtime capability guarantee. In
`codex-cli 0.145.0`, for example, the experimental schema advertises
`thread/items/list` while the initialized app-server returns JSON-RPC `-32601`
for that method. Probe a method against the installed runtime before depending
on it; Ruddr intentionally does not expose `thread/items/list`.

Ruddr currently depends on:

- `initialize` then `initialized`
- `thread/start`
- `thread/list`, `thread/search`, and `thread/read`
- `thread/turns/list`
- `thread/resume` and `thread/fork`
- `thread/name/set`, `thread/archive`, and `thread/unarchive`
- `turn/start`
- `turn/steer`
- `turn/interrupt`
- `turn/started`, `item/*`, and `turn/completed` notifications

The Claude, OpenCode 2, and Pi adapters implement the lifecycle subset needed
by `ruddr run`, `steer`, `prompt`, and `interrupt`. They do not implement the
general Codex app-server surface. Sessions launched by another process do not
become live-observable through Ruddr. Each adapter forwards summarized or
provider-exposed reasoning. Ruddr does not expose hidden raw chain of thought.

## Agent setup guide

Hand this to Claude Code, Codex, Cursor, or any agent with shell access. It is
written to be pasted verbatim, and it covers both installing Ruddr and
operating it afterwards, so the agent ends up able to run and steer provider
sessions, not just build a binary.

````text
Set up Ruddr (https://github.com/safzanpirani/ruddr) on this machine, verify
it works, and learn how to operate it. Ruddr runs Codex, Claude Code,
OpenCode 2, or Pi as an observable, steerable child process. It writes every
event to
disk and exposes a control socket so you can redirect or stop a turn while it
runs. Follow Part 1 in order and stop at the first failure; keep Part 2 as your
operating manual.

PART 1 — INSTALL AND VERIFY

1. Check prerequisites. Report the version of each and stop if any is missing:
   - Go 1.24 or newer  (`go version`)
   - Bun 1.4 or newer  (`bun --version`)
   - At least one provider CLI: `codex --version`, `claude --version`,
     `opencode2 --version`, or `pi --version`.

2. Clone the repo somewhere sensible and build it:
   git clone https://github.com/safzanpirani/ruddr
   cd ruddr
   bun install
   go build -o ruddr .

3. Run the test suite and stop if anything fails:
   go test ./...
   go vet ./...
   bun test
   bunx tsc -p tsconfig.json --noEmit

4. Install it onto PATH:
   ./scripts/install-local.sh
   Then confirm `ruddr --help` runs from a directory other than the repo.

5. Smoke-test a real turn. Create a scratch prompt that asks the provider to
   reply with exactly SMOKE_OK, then:
   ruddr run --provider codex --cwd /tmp/ruddr-smoke/ws \
     --prompt-file /tmp/ruddr-smoke/prompt.md \
     --state-dir /tmp/ruddr-smoke/run --sandbox read-only
   ruddr peek --state-dir /tmp/ruddr-smoke/run
   Confirm `ruddr status --state-dir /tmp/ruddr-smoke/run --json` reports
   "completed" and that output.md contains SMOKE_OK.

6. If you are setting up the Claude provider and it reports an authentication
   failure, the cause is almost always that the resolved `claude` executable
   cannot reach its credentials from a detached process. Do not put any token
   into Ruddr. Instead point Ruddr at the wrapper or launcher that does work
   interactively, using `--claude-path` or `RUDDR_CLAUDE_PATH`.

7. Report what you installed, where the binary landed, which providers
   authenticated, and the exact output of any step that failed. Do not modify
   my shell configuration without telling me what you changed.

PART 2 — HOW TO OPERATE RUDDR

Core model. One `ruddr run` owns one provider session. You choose a
--state-dir; everything about the run lands there:
   state.json      status, thread/turn IDs, token usage — never prompt text
   events.jsonl    every raw provider event, append-only
   trace.log       human-readable activity trace
   output.md       completed agent messages, in order
Trust output.md only when `ruddr status --json` says "completed"; on
"failed" or "interrupted" report the error field instead. The state dir must
be fresh per run. Prompts always come from --prompt-file, never argv.

Starting runs. Useful `ruddr run` flags:
   --provider codex|claude|opencode|pi   default codex
   --model / --effort           `ruddr models --json` lists valid combos
   --sandbox                    read-only | workspace-write (default) |
                                danger-full-access
   --cwd DIR                    the workspace the provider edits
   --turn-timeout 1h            per-turn watchdog; 0 disables
Long runs: launch in the background (or your harness's background mode), then
watch with `ruddr peek --state-dir DIR -n 25` and block bounded with
`ruddr wait --state-dir DIR --timeout 30m`. Never poll in a foreground loop.

Steering. While a turn is active you can redirect it without restarting:
   ruddr steer --state-dir DIR "the correction, exact literals preserved"
   ruddr steer --state-dir DIR --message-file FILE   (multiline/shell-unsafe)
Use steer when new information arrives mid-turn. To abort a wrong-premise turn
use `ruddr interrupt --state-dir DIR`, never kill -9. A rejected steer means
the turn already ended — read the output; do not silently start a new run.

Multi-turn (idle) sessions. Add --idle to `ruddr run` and the process stays
alive after each turn instead of exiting:
   ruddr prompt --state-dir DIR "next task"    starts the next turn
   ruddr stop   --state-dir DIR                graceful shutdown
   --idle-timeout 4h                            auto-exit when unused
status "idle" means ready for the next prompt; "active" means a turn is
running (steer, don't prompt). Prompt and steer are different commands with
different semantics — never substitute one for the other. Use idle mode when
you expect follow-up turns: it keeps one process and one thread instead of
spawning a fresh run per message.

Continuing past work. Threads persist in the provider's own store:
   ruddr thread list --cwd-filter "$PWD"       recent threads for this repo
   ruddr thread search "keywords"              global search — verify cwd
   ruddr thread read --include-turns ID        inspect before resuming
   ruddr run --resume-thread ID ...            continue a thread in a new run
   ruddr run --fork-thread ID ...              branch it, preserving original
Verify a candidate thread's cwd and content before resuming; never resume or
fork a thread whose turn is still active.

Watching everything at once. `ruddr tui` shows a dashboard of live and
recent sessions with a prompt box: type to steer an active turn, prompt an
idle one, or continue a finished thread; `n` starts a new session, `m` picks
the model, `x x` stops. `ruddr tui --beta` switches to a chat-first layout.

Ground rules:
- Report token usage/cost from `ruddr status --json` when the human asks
  what a run cost.
- state.json is intentionally content-redacted; never write prompt or
  completion text into it or rely on it being there.
- One state dir, one run, ever. New run, new dir.
````

## Agent skill: delegate work through Ruddr

[`skills/ruddr-delegate`](skills/ruddr-delegate/SKILL.md) is an installable
agent skill that teaches a coding agent to hand a hard or long task to a
steerable provider through Ruddr: build a self-contained
brief, launch in the background, monitor, steer mid-turn, wait bounded, and
verify the handoff. The skill is embedded in the binary, and both the npm
package and `scripts/install-local.sh` install it into `~/.claude/skills/` and
`~/.agents/skills/` for you; `ruddr update` refreshes it. Run it yourself to
reinstall it or to target another location:

```bash
ruddr skill install                      # ~/.claude/skills and ~/.agents/skills
ruddr skill install --dir .claude/skills # this project only
ruddr skill show                         # print the skill
```

## Development

```bash
gofmt -w *.go
go test ./...
go vet ./...
go build -o ruddr .
bun test
bunx tsc -p tsconfig.json --noEmit
```

## License

MIT. See `LICENSE`. Third-party components are listed in
`THIRD_PARTY_NOTICES.md`.
