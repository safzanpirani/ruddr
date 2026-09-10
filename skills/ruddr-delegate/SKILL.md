---
name: ruddr-delegate
description: Delegate a hard, stuck, or context-heavy implementation task to a live-steerable Codex or Claude Code session managed by Ruddr, so a sub-agent investigates, edits, and verifies the current workspace end to end while the parent agent keeps working and can steer mid-turn. Use when the user asks to hand work to codex/claude/a sub-agent, when a bug or feature has resisted a couple of attempts, or when long autonomous work should run outside the parent agent's context.
metadata:
  short-description: Delegate work to a steerable Ruddr sub-agent
---

# ruddr-delegate

Package the problem into a self-contained brief, then run it through a Ruddr
sub-agent session so the provider investigates and *implements* the fix. This
is delegation, not review: the run should end with working, verified code. The
parent agent stays free to keep talking to the user and can redirect the run
mid-turn with `ruddr steer`.

Requires the `ruddr` CLI (see the repo's README "Agent setup guide" if it is
not installed).

## Pick a provider and model

- `--provider codex` (the default) runs a Codex app-server session. Use it
  when the user says "codex", or when work should run on the Codex quota
  instead of the parent agent's. Default to `--model gpt-6-astra
  --effort low`.
- `--provider claude` runs Claude Code through Ruddr's adapter. Use it when
  the user says "claude", or for a clean-context second Claude. Default to
  `--model claude-opus-5 --effort medium`.
- `--provider opencode` runs OpenCode 2 and `--provider pi` runs Pi, both on
  the models their own config exposes (the default is
  `openrouter/deepseek/deepseek-v4-flash-vision-exp`). Use them when the user
  names that tool, or for cheap parallel attempts. Only Pi accepts `--effort`.
  Neither adapter enforces Ruddr's filesystem containment for
  `workspace-write`; they rely on the provider's own permission system, so
  prefer codex or claude for anything touching files outside the workspace.

`ruddr models --json` lists every valid model and effort per provider. Honor
an explicit user choice; raise the effort to `high` only for genuinely subtle
problems. Pass `--model` explicitly so the run is reproducible from
`state.json`.

Claude only: if the bare `claude` binary cannot reach its credentials from a
detached process, pass the wrapper that works interactively via
`--claude-path` or `RUDDR_CLAUDE_PATH`. OpenCode and Pi take `--opencode-path`
and `--pi-path` the same way. Never put tokens in argv or prompts.

## Build the brief

The sub-agent has none of the parent conversation, so the prompt file must
stand alone:

1. The concrete goal and completion condition, in one or two lines.
2. What has already been tried and why it failed — real error output, stack
   traces, and failing test names verbatim, never paraphrased.
3. The load-bearing files: exact paths and, where known, functions/lines.
4. The exact reproduction and verification commands.
5. Project constraints from `AGENTS.md`, `CLAUDE.md`, `README.md`, or `docs/`
   that the sub-agent cannot infer from the diff.
6. Scope fences: what to touch, what is explicitly off-limits, and any
   unrelated dirty changes to preserve.
7. Commit policy: by default tell it to leave every change uncommitted and
   unpushed so the parent can review against the baseline. Only say otherwise
   when the user asked for commits.
8. The sandbox it runs in and what that means. `workspace-write` under Codex
   has no network, so dependency fetches (`npm install`, `go mod download`,
   `pip install`) fail; pre-install them from the parent before launching and
   say so, or state that the fetch is unavailable so the sub-agent does not
   misread the failure as a bug.
9. The paths to ignore: the brief and the run state dir (see Launch). Tell it
   those are the controller's files, must not be edited, and must not be
   committed.

Tell it to investigate before editing, implement directly, run the verify
commands and iterate until they pass, and mark genuinely uncertain behavioral
choices with a `// TODO(subagent):` marker instead of guessing. Then append
this block verbatim:

```text
Work autonomously to completion — do not stop to ask me for confirmation mid-task. Investigate, implement the fix directly in the files, then run the verify command(s) above and iterate until they pass (or until you've hit a genuine blocker).

At the very end, print a **"Handoff report"**:
- What the root cause turned out to be (one or two lines).
- Changes applied, grouped by file, one line each on what and why.
- Verification: the exact command you ran and its result (passed / still failing + the remaining error).
- Anything you deliberately did NOT do, with why (uncertain / behavioral / out of scope), and any follow-up the human must run (regenerate types, set env var, rerun migration, etc.).
```

## Launch

> **Dirty-tree guard:** run `git status -sb` first; if the tree has
> uncommitted changes, snapshot a baseline (`git diff > <fixed path>` or
> `git stash create`) so the sub-agent's edits stay attributable. Use a
> separate `git worktree` for risky or competing approaches.

> **Fixed literal paths only.** Each shell tool call is a fresh shell, so a
> variable set in one call is empty in the next. Pick stable paths (for
> example under `.scratch/<task-slug>/`) and reuse them verbatim. Never reuse
> a previous run's state dir.

> **Keep controller files out of the sub-agent's diff.** The brief and state
> dir sit inside the worktree, so they show up in the sub-agent's `git
> status`. Make sure `.scratch/` is ignored (`git check-ignore .scratch` or
> add it to `.git/info/exclude`), or put the state dir outside the repo, and
> name the paths in the brief as off-limits.

```bash
ruddr run \
  --provider codex --model gpt-6-astra --effort low \
  --cwd "$PWD" \
  --prompt-file .scratch/<task-slug>/brief.md \
  --state-dir .scratch/<task-slug>/run \
  --sandbox workspace-write
```

Swap the first line for `--provider claude --model claude-opus-5 --effort
medium`, `--provider opencode`, or `--provider pi` as chosen above.

- Launch with the harness's background facility — a foreground tool call gets
  killed at the tool timeout, taking the controller with it. If the harness
  has no background mode, detach explicitly:
  `nohup ruddr run ... > run.log 2>&1 < /dev/null &`.
- `--sandbox workspace-write` is the safe default. Escalate to
  `danger-full-access` only when the task genuinely needs network or
  out-of-workspace access and the user's policy allows it; use `read-only`
  for an advisory consult where edits are unwanted (and say so in the brief).
- `--turn-timeout` defaults to one hour per turn.
- Expecting follow-up turns? Add `--idle` and send later turns with
  `ruddr prompt --state-dir DIR "next task"`; end the session with
  `ruddr stop --state-dir DIR`. Prompt (new turn) and steer (redirect the
  current turn) are different commands — never substitute one for the other.

## Monitor and steer

```bash
ruddr peek --state-dir .scratch/<task-slug>/run -n 25     # live trace
ruddr status --state-dir .scratch/<task-slug>/run --json  # machine-readable
ruddr tui                                                 # every session, live
```

When the user adds context, corrects a premise, or changes priority while the
turn is active, forward it immediately into the same turn:

```bash
ruddr steer --state-dir .scratch/<task-slug>/run "<exact update, literals preserved>"
```

Use `--message-file` for multiline or shell-sensitive text. A rejected steer
means the turn already ended — read the output; never silently start a
replacement run. To abort a wrong-premise turn use `ruddr interrupt`, not
kill. If `status` reports `stale`, the controller died: report it and start
fresh only with the user's go-ahead.

Wedge check: if `status` says `active` but `trace.log` has been silent far
longer than the work plausibly takes, interrupt and resume the same thread
with a brief that says what it already learned.

## Wait and verify

```bash
ruddr wait --state-dir .scratch/<task-slug>/run --timeout 10m
ruddr status --state-dir .scratch/<task-slug>/run --json
```

Always bound the wait, and never let it outlive the harness's tool timeout: a
foreground `wait --timeout 1h` is killed by a two-minute tool limit, which
looks like a failed hand-off while the run is still going. Either run the
wait through the harness's background facility, or wait in slices of a few
minutes with a `status --json` read between them. Keep doing the parent's own
work between slices. Trust `output.md` only when status is `completed`; on
`failed`/`interrupted`/`stale` report the error field instead — partial output
is not a successful handoff. Then verify independently: run the verify
command(s) yourself and cross-check claimed edits against the pre-run
baseline. Report the run/output paths, the Handoff report, the verification
evidence, and whether changes are local, committed, or pushed.

## Continue past work

Within one parent session: steer the active run; resume a terminal one for a
direct follow-up; otherwise start fresh.

```bash
ruddr run --resume-thread THREAD_ID ...   # threadId from ruddr status --json
ruddr run --fork-thread THREAD_ID ...     # branch, preserving the original
```

Codex threads are also discoverable: `ruddr thread list --cwd-filter "$PWD"`,
`ruddr thread search "keywords"`, `ruddr thread read --include-turns ID` —
verify the candidate's `cwd` and content before resuming, and never resume or
fork a thread whose turn is still active. Claude sessions resume by
`--resume-thread` with the session ID from `ruddr status --json` only; the
`thread` discovery/fork commands are Codex-specific. A resumed brief should
state what changed since the previous turn, not restate the whole task.

## Fallback

If `ruddr` is missing, fall back to the provider's own headless mode
(`codex exec --json ... < brief.md`, or `claude -p ... < brief.md`), noting to
the user that the run will not be steerable. If the provider CLI is missing
too, output the brief and say what was unavailable.
