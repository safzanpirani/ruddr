package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func makeGroupRun(t *testing.T, stateDir string, state runState) {
	t.Helper()
	if err := os.MkdirAll(stateDir, 0o700); err != nil {
		t.Fatal(err)
	}
	writeWaitState(t, stateDir, state)
}

func TestDiscoverRunsFindsNestedRunsWithinDepth(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "ui", "run"), runState{Status: "completed"})
	makeGroupRun(t, filepath.Join(root, "api", "run"), runState{Status: "completed"})
	// A state dir is a leaf: nothing below it is another run.
	makeGroupRun(t, filepath.Join(root, "api", "run", "nested"), runState{Status: "completed"})
	makeGroupRun(t, filepath.Join(root, "a", "b", "c", "d", "run"), runState{Status: "completed"})
	if err := os.WriteFile(filepath.Join(root, "ui", "brief.md"), []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}

	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, ref := range refs {
		names = append(names, ref.Name)
	}
	if got := strings.Join(names, ","); got != "api/run,ui/run" {
		t.Fatalf("discovered %q, want api/run,ui/run", got)
	}
}

func TestDiscoverRunsAcceptsARunDirectoryAsRoot(t *testing.T) {
	stateDir := filepath.Join(t.TempDir(), "solo")
	makeGroupRun(t, stateDir, runState{Status: "completed"})
	refs, err := discoverRuns(stateDir)
	if err != nil || len(refs) != 1 || refs[0].Name != "solo" {
		t.Fatalf("discoverRuns(run dir) = %+v, %v", refs, err)
	}
}

func TestGroupSelectionKeepsSingleRunMode(t *testing.T) {
	cases := []struct {
		dirs, roots []string
		single      bool
	}{
		{nil, nil, true},
		{[]string{"a"}, nil, true},
		{[]string{"a", "b"}, nil, false},
		{nil, []string{"r"}, false},
		{[]string{"a"}, []string{"r"}, false},
	}
	for _, c := range cases {
		group := groupSelection{stateDirs: c.dirs, roots: c.roots}
		if _, single := group.single(); single != c.single {
			t.Errorf("dirs=%v roots=%v single=%v, want %v", c.dirs, c.roots, single, c.single)
		}
	}
}

func TestGroupResolveDeduplicatesAndRejectsEmptyRoots(t *testing.T) {
	root := t.TempDir()
	stateDir := filepath.Join(root, "api")
	makeGroupRun(t, stateDir, runState{Status: "completed"})
	group := groupSelection{stateDirs: stringList{stateDir}, roots: stringList{root}}
	refs, err := group.resolve()
	if err != nil || len(refs) != 1 || refs[0].Name != stateDir {
		t.Fatalf("resolve = %+v, %v", refs, err)
	}
	empty := groupSelection{roots: stringList{t.TempDir()}}
	if _, err := empty.resolve(); err == nil || !strings.Contains(err.Error(), "no runs found") {
		t.Fatalf("empty root error = %v", err)
	}
}

func TestWaitForRunsReportsEveryRunAndFailsOnAnyFailure(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "api"), runState{Version: 2, PID: 1, Status: "completed", Provider: "codex", Model: "m",
		TokenUsage: &tokenUsage{TotalTokens: 84200}})
	makeGroupRun(t, filepath.Join(root, "ui"), runState{Version: 2, PID: 2, Status: "failed", Error: "turn failed; see trace.log"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	err = waitForRuns(&out, refs, time.Time{}, waitOptions{}, func(int) bool { return true }, time.Millisecond)
	if err == nil || err.Error() != "1 of 2 runs did not complete" {
		t.Fatalf("wait error = %v", err)
	}
	table := out.String()
	for _, want := range []string{"NAME", "api", "completed", "84.2K", "ui", "failed", "turn failed"} {
		if !strings.Contains(table, want) {
			t.Fatalf("table missing %q:\n%s", want, table)
		}
	}
}

func TestWaitForRunsWaitsForTheSlowestRun(t *testing.T) {
	root := t.TempDir()
	fast := filepath.Join(root, "fast")
	slow := filepath.Join(root, "slow")
	makeGroupRun(t, fast, runState{Version: 2, PID: 1, Status: "completed"})
	makeGroupRun(t, slow, runState{Version: 2, PID: 2, Status: "active"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	polls := 0
	alive := func(int) bool {
		polls++
		if polls == 3 {
			writeWaitState(t, slow, runState{Version: 2, PID: 2, Status: "completed"})
		}
		return true
	}
	if err := waitForRuns(&bytes.Buffer{}, refs, time.Time{}, waitOptions{}, alive, time.Millisecond); err != nil {
		t.Fatalf("wait = %v", err)
	}
	if polls < 3 {
		t.Fatalf("wait returned after %d polls, before the slow run finished", polls)
	}
}

// --any skips runs that had already finished, so repeated calls hand back
// the runs one at a time.
func TestWaitForRunsAnyWaitsForARunningRun(t *testing.T) {
	root := t.TempDir()
	busy := filepath.Join(root, "busy")
	makeGroupRun(t, filepath.Join(root, "done"), runState{Version: 2, PID: 1, Status: "failed", Error: "old failure"})
	makeGroupRun(t, busy, runState{Version: 2, PID: 2, Status: "active"})
	makeGroupRun(t, filepath.Join(root, "slow"), runState{Version: 2, PID: 3, Status: "active"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	polls := 0
	alive := func(int) bool {
		polls++
		if polls == 4 {
			writeWaitState(t, busy, runState{Version: 2, PID: 2, Status: "completed"})
		}
		return true
	}
	var out bytes.Buffer
	if err := waitForRuns(&out, refs, time.Time{}, waitOptions{Any: true}, alive, time.Millisecond); err != nil {
		t.Fatalf("wait --any = %v; the earlier failure must not count", err)
	}
	if !strings.Contains(out.String(), "finished: busy\n") {
		t.Fatalf("wait --any did not name the finished run:\n%s", out.String())
	}
}

func TestWaitForRunsAnyReturnsAtOnceWhenNothingIsRunning(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "a"), runState{Version: 2, PID: 1, Status: "completed"})
	makeGroupRun(t, filepath.Join(root, "b"), runState{Version: 2, PID: 2, Status: "failed"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	err = waitForRuns(&bytes.Buffer{}, refs, time.Time{}, waitOptions{Any: true}, func(int) bool { return true }, time.Millisecond)
	if err == nil || err.Error() != "1 of 2 runs did not complete" {
		t.Fatalf("wait --any with nothing running = %v", err)
	}
}

func TestWaitForRunsTurnCountsIdleSessionsByTheirLastTurn(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "ok"), runState{Version: 2, PID: 1, Status: "idle", Idle: true, LastTurn: "completed"})
	makeGroupRun(t, filepath.Join(root, "bad"), runState{Version: 2, PID: 2, Status: "idle", Idle: true, LastTurn: "failed"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	alive := func(int) bool { return true }
	err = waitForRuns(&bytes.Buffer{}, refs, time.Now().Add(20*time.Millisecond), waitOptions{}, alive, time.Millisecond)
	if err == nil || !strings.Contains(err.Error(), "timed out") {
		t.Fatalf("plain wait on idle sessions = %v; want it to keep waiting", err)
	}
	var out bytes.Buffer
	err = waitForRuns(&out, refs, time.Time{}, waitOptions{Turn: true}, alive, time.Millisecond)
	if err == nil || err.Error() != "1 of 2 runs did not complete" {
		t.Fatalf("wait --turn = %v", err)
	}
	if !strings.Contains(out.String(), "last turn failed") {
		t.Fatalf("table does not show the failed turn:\n%s", out.String())
	}
}

func TestSingleWaitTurnReportsTheLastTurn(t *testing.T) {
	stateDir := t.TempDir()
	writeWaitState(t, stateDir, runState{Version: 2, PID: 1, Status: "idle", Idle: true, LastTurn: "completed"})
	alive := func(int) bool { return true }
	output, err := captureStdout(func() error { return waitForRunState(stateDir, time.Time{}, true, alive, time.Millisecond) })
	if err != nil || string(output) != "idle (last turn completed)\n" {
		t.Fatalf("wait --turn = %q, %v", output, err)
	}
	writeWaitState(t, stateDir, runState{Version: 2, PID: 1, Status: "idle", Idle: true, LastTurn: "interrupted"})
	if err := waitForRunState(stateDir, time.Time{}, true, alive, time.Millisecond); err == nil || !strings.Contains(err.Error(), "interrupted") {
		t.Fatalf("wait --turn after an interrupted turn = %v", err)
	}
}

func writeEvents(t *testing.T, path string, events ...map[string]any) {
	t.Helper()
	var buf bytes.Buffer
	for _, event := range events {
		raw, err := json.Marshal(event)
		if err != nil {
			t.Fatal(err)
		}
		buf.Write(append(raw, '\n'))
	}
	if err := os.WriteFile(path, buf.Bytes(), 0o600); err != nil {
		t.Fatal(err)
	}
}

func agentMessageEvent(text string) map[string]any {
	return map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"type": "agentMessage", "text": text}}}
}

func TestLastAgentMessageReadsOnlyTheLatestTurn(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	turnStarted := map[string]any{"method": "turn/started", "params": map[string]any{"turn": map[string]any{"id": "t"}}}
	writeEvents(t, path, turnStarted, agentMessageEvent("first answer"),
		turnStarted, agentMessageEvent("looking at it"), agentMessageEvent("final\n\nanswer"))
	if message, err := lastAgentMessage(path); err != nil || message != "final\n\nanswer" {
		t.Fatalf("lastAgentMessage = %q, %v", message, err)
	}
	writeEvents(t, path, turnStarted, agentMessageEvent("first answer"), turnStarted)
	if message, err := lastAgentMessage(path); err != nil || message != "" {
		t.Fatalf("a turn without a message returned %q, %v", message, err)
	}
}

func TestResultReportsAnswersAndFailures(t *testing.T) {
	root := t.TempDir()
	done := filepath.Join(root, "done")
	makeGroupRun(t, done, runState{Version: 2, PID: 1, Status: "completed", EventsPath: filepath.Join(done, "events.jsonl")})
	writeEvents(t, filepath.Join(done, "events.jsonl"), agentMessageEvent("all fixed"))
	makeGroupRun(t, filepath.Join(root, "failed"), runState{Version: 2, PID: 2, Status: "failed", Error: "turn failed; see trace.log"})

	output, err := captureStdout(func() error { return resultCommand([]string{"--root", root}) })
	want := "== done: completed ==\nall fixed\n\n== failed: failed ==\nerror: turn failed; see trace.log\n"
	if string(output) != want || err == nil || err.Error() != "1 of 2 runs did not complete" {
		t.Fatalf("result = %q, %v", output, err)
	}
	output, err = captureStdout(func() error { return resultCommand([]string{"--state-dir", done}) })
	if err != nil || string(output) != "all fixed\n" {
		t.Fatalf("single result = %q, %v", output, err)
	}
	output, err = captureStdout(func() error { return resultCommand([]string{"--root", root, "--json"}) })
	var results []runResult
	if jsonErr := json.Unmarshal(output, &results); jsonErr != nil || err == nil || len(results) != 2 || results[0].Message != "all fixed" || results[1].Error == "" {
		t.Fatalf("result --json = %s, %v", output, err)
	}
}

func TestGroupPeekPrintsEachRunsTrace(t *testing.T) {
	root := t.TempDir()
	for _, name := range []string{"a", "b"} {
		dir := filepath.Join(root, name)
		trace := filepath.Join(dir, "trace.log")
		makeGroupRun(t, dir, runState{Version: 2, PID: 1, Status: "completed", TracePath: trace})
		if err := os.WriteFile(trace, []byte("one\ntwo\nthree "+name+"\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	if err := groupPeek(&out, refs, 2); err != nil {
		t.Fatal(err)
	}
	if want := "== a: completed ==\ntwo\nthree a\n\n== b: completed ==\ntwo\nthree b\n"; out.String() != want {
		t.Fatalf("peek = %q", out.String())
	}
}

func TestWaitForRunsTimesOutAndMarksStaleRuns(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "busy"), runState{Version: 2, PID: 2, Status: "active"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	err = waitForRuns(&bytes.Buffer{}, refs, time.Now().Add(20*time.Millisecond), waitOptions{}, func(int) bool { return true }, time.Millisecond)
	if err == nil || !strings.Contains(err.Error(), "timed out: 1 of 1 runs still running") {
		t.Fatalf("timeout error = %v", err)
	}

	var out bytes.Buffer
	err = waitForRuns(&out, refs, time.Time{}, waitOptions{}, func(int) bool { return false }, time.Millisecond)
	if err == nil || !strings.Contains(out.String(), "stale") {
		t.Fatalf("dead controller: err=%v table=\n%s", err, out.String())
	}
}

func TestGroupControlSkipsRunsInOtherStates(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "done"), runState{Version: 2, PID: 1, Status: "completed"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	if err := broadcastControl(&out, refs, "shutdown", "idle", time.Second); err != nil {
		t.Fatal(err)
	}
	if got := out.String(); got != "done: skipped (status=completed)\nno idle runs to stop\n" {
		t.Fatalf("output = %q", got)
	}
}

func TestInterruptRejectsExpectedTurnIDForSeveralRuns(t *testing.T) {
	err := interruptCommand([]string{"--root", t.TempDir(), "--expected-turn-id", "turn-1"})
	if err == nil || !strings.Contains(err.Error(), "applies to one run") {
		t.Fatalf("interrupt error = %v", err)
	}
}

// Two live idle sessions: status lists both, stop ends both, and wait
// returns once both have exited cleanly.
func TestGroupCommandsDriveSeveralLiveRuns(t *testing.T) {
	first, firstErr := startIdleHelperRun(t, nil, nil)
	second, secondErr := startIdleHelperRun(t, nil, nil)
	waitForRunStatus(t, first, "idle")
	waitForRunStatus(t, second, "idle")
	dirs := []string{"--state-dir", first, "--state-dir", second}

	output, err := captureStdout(func() error { return statusCommand(append([]string{"--json"}, dirs...)) })
	if err != nil {
		t.Fatal(err)
	}
	var states []runState
	if err := json.Unmarshal(output, &states); err != nil || len(states) != 2 || states[0].Status != "idle" || states[1].Status != "idle" {
		t.Fatalf("status --json = %s, %v", output, err)
	}

	if states[0].LastTurn != "completed" {
		t.Fatalf("idle state lastTurnStatus = %q, want completed", states[0].LastTurn)
	}
	output, err = captureStdout(func() error { return waitCommand(append([]string{"--turn", "--timeout", "5s"}, dirs...)) })
	if err != nil || strings.Count(string(output), "idle") != 2 {
		t.Fatalf("wait --turn = %q, %v", output, err)
	}
	output, err = captureStdout(func() error { return resultCommand(dirs) })
	if err != nil || strings.Count(string(output), "TURN 1") != 2 {
		t.Fatalf("result = %q, %v", output, err)
	}

	output, err = captureStdout(func() error { return stopCommand(dirs) })
	if err != nil || strings.Count(string(output), "shutdown requested") != 2 {
		t.Fatalf("stop = %q, %v", output, err)
	}
	for _, errCh := range []chan error{firstErr, secondErr} {
		select {
		case err := <-errCh:
			if err != nil {
				t.Fatalf("controller exit = %v", err)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("controller did not stop")
		}
	}
	output, err = captureStdout(func() error { return waitCommand(append([]string{"--timeout", "5s"}, dirs...)) })
	if err != nil || strings.Count(string(output), "completed") != 2 {
		t.Fatalf("wait = %q, %v", output, err)
	}
}
