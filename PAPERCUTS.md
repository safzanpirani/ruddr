# PAPERCUTS

Small, non-blocking frictions encountered by agents while working. Review this file periodically and sand them down.

## 2026-07-21T19:32:36.264Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `misleading-error`
- **Resolved:** 2026-08-23T16:02:27.527Z — Global AGENTS.md now reserves zsh path and requires task-specific variable names.

While extracting Codex app-server schema fields in zsh, assigning a loop variable named path silently overwrote zsh's special PATH array and made jq appear missing. Avoid lowercase path as a zsh variable or run the loop under sh/bash.

## 2026-07-21T19:39:44.287Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `misleading-error`
- **Resolved:** 2026-09-10T12:31:19.817Z — README.md documents the method as unavailable and records that Ruddr intentionally does not expose it.

Codex CLI 0.145.0 generate-json-schema --experimental advertises thread/items/list, but a live initialized experimental app-server call returns JSON-RPC -32601: thread/items/list is not supported yet. Generated protocol availability does not guarantee the runtime handler exists; add a capability/runtime probe or document this method as unavailable.

## 2026-07-22T06:19:22.342Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `misleading-error`
- **Resolved:** 2026-08-23T16:02:27.672Z — Global AGENTS.md now reserves zsh status and path and provides safe replacements.

While reporting a Codex Rudder SIGTERM smoke in zsh, assigning to the ordinary-looking variable name status failed because zsh reserves it as read-only. Avoid status and path as zsh script variables; use task-specific names such as run_status and schema_file.

## 2026-07-22T06:19:54.248Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `stale-cache`
- **Resolved:** 2026-08-23T16:02:27.778Z — Global AGENTS.md now requires re-deriving and validating temporary paths between sessions.

A later Codex Rudder live smoke reused a /tmp prompt path from an earlier session, but the temporary file had already been cleaned, causing the run to fail before state creation. Recreate or validate temp artifacts immediately before each smoke instead of treating /tmp paths as durable.

## 2026-07-26T08:14:33.136Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `tooling`, `github-connector`

While opening a draft PR for the private codex-rudder repository, the GitHub connector returned a 404 after an authenticated git push succeeded. The connector likely lacks access to this private repo; falling back to the authenticated gh CLI worked around it.

## 2026-07-26T08:23:10.581Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `tooling`, `workflow`
- **Resolved:** 2026-08-23T16:21:33.745Z — Global operational runbook now requires creating workdirs before launch and separates temporary worktree create/use/remove calls.

While rebuilding Rudder from a clean temporary worktree, the unified shell rejected a narrowly scoped cleanup trap because it contained rm -f, and a follow-up command failed because its workdir was created inside the command rather than before process startup. Create the worktree in one call, build from it in a second call, and remove it with git worktree remove.

## 125087 · 2026-08-25T18:13:03.034Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `flaky-command`
- **Resolved:** 2026-08-25T18:15:38.052Z — Installer now copies to a fresh temp inode, chmods it, atomically renames it over the executable, and verifies SHA-256. This avoids in-place overwrite of binaries backing live Rudder processes.

While installing the global Rudder binary, macOS killed a final cmp -s verification with signal 9 after the copy and dependency install completed. Replace the verification with explicit SHA-256 output comparison so failures remain diagnosable.

## aafd02 · 2026-08-25T20:39:11.122Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **About:** `tmux-skill`
- **Tags:** `docs`
- **Resolved:** 2026-08-30T20:50:10.977Z — The installed tmux skill no longer contains the positional helper example. wait-for-text.sh --help documents -t and -p, and the skill package validates successfully.

The tmux SKILL.md helper example uses positional arguments for wait-for-text.sh, but the installed script requires -t and -p named flags. Update the example to match the helper's current CLI.

## 8656cb · 2026-08-27T19:49:15.671Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **About:** `skill-creator`
- **Tags:** `tooling`
- **Resolved:** 2026-08-30T20:50:10.919Z — The bundled skill-creator instructions now invoke quick_validate.py through python3. The documented command passes while direct execution still correctly reflects the file's non-executable mode.

The bundled quick_validate.py script is not executable, so the documented direct invocation fails with permission denied. Running it through python3 works; either add its executable bit or document python3 explicitly.

## 8a77fc · 2026-08-29T09:27:25.706Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `test-gap`
- **Resolved:** 2026-09-10T12:31:19.817Z — Shutdown awaits the refresh gate before destroying the renderer, the gate now settles even when the refresh fails, and refresh no longer writes status into a torn-down renderer.

While restarting the TUI after a PTY verification, pressing q during an in-flight refresh destroyed the renderer before refresh finished. refresh then called setStatus on a destroyed TextBuffer and crashed; shutdown should await or cancel refresh work before renderer destruction.

## c5fbac · 2026-08-30T20:45:45.194Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **Tags:** `flaky-command`
- **Resolved:** 2026-09-10T12:31:19.817Z — The Pi startup handshake has its own budget instead of borrowing the steady-state RPC deadline, and the test helper waits on a wall clock rather than a fixed poll count.

The Pi RPC timeout regression test failed when bun test ran concurrently with Go tests and TypeScript checking, then passed alone and in a serial full Bun run. The 500ms test deadline appears sensitive to host contention.

## 39d04c · 2026-09-02T12:21:16.158Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **About:** `fleet`
- **Tags:** `misleading-error`

While deploying Rudder to Ampere, I hardcoded an expected SHA-256 that did not match the freshly cross-built artifact. The integrity guard correctly stopped before installation. Derive the expected hash from the verified local artifact instead of transcribing it manually.

## fa5844 · 2026-09-02T12:26:21.423Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **About:** `tmux`
- **Tags:** `dx`
- **Resolved:** 2026-09-10T12:31:19.817Z — The TUI resolves --state-dir to an absolute path and pins explicitly named sessions to the top of the list, so they take the initial selection.

While proving Rudder's live diff tab with --state-dir, the TUI still prioritized a globally registered active session. The expected checkout-specific diff pattern timed out even though the viewer rendered another workspace's real diff. Explicit state directories should be easier to select or prioritize during focused inspection.

## 77ba5e · 2026-09-02T12:48:39.001Z — codex — gpt-5.6-sol

- **Directory:** `/Users/safzan/Development/projects/codex-rudder`
- **About:** `tmux`
- **Tags:** `tooling`

While replaying a Rudder tree click in a detached tmux PTY, tmux send-keys delivered the SGR mouse sequence as keyboard input instead of a mouse event. The pane capture could not validate mouse hit-testing; use a real attached client or a dedicated mouse-event harness for this proof.

