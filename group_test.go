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
	err = waitForRuns(&out, refs, time.Time{}, false, func(int) bool { return true }, time.Millisecond)
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
	if err := waitForRuns(&bytes.Buffer{}, refs, time.Time{}, false, alive, time.Millisecond); err != nil {
		t.Fatalf("wait = %v", err)
	}
	if polls < 3 {
		t.Fatalf("wait returned after %d polls, before the slow run finished", polls)
	}
}

func TestWaitForRunsAnyReturnsOnFirstSettledRun(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "done"), runState{Version: 2, PID: 1, Status: "completed"})
	makeGroupRun(t, filepath.Join(root, "busy"), runState{Version: 2, PID: 2, Status: "active"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	if err := waitForRuns(&bytes.Buffer{}, refs, time.Time{}, true, func(int) bool { return true }, time.Millisecond); err != nil {
		t.Fatalf("wait --any = %v", err)
	}
}

func TestWaitForRunsTimesOutAndMarksStaleRuns(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "busy"), runState{Version: 2, PID: 2, Status: "active"})
	refs, err := discoverRuns(root)
	if err != nil {
		t.Fatal(err)
	}
	err = waitForRuns(&bytes.Buffer{}, refs, time.Now().Add(20*time.Millisecond), false, func(int) bool { return true }, time.Millisecond)
	if err == nil || !strings.Contains(err.Error(), "timed out: 1 of 1 runs still running") {
		t.Fatalf("timeout error = %v", err)
	}

	var out bytes.Buffer
	err = waitForRuns(&out, refs, time.Time{}, false, func(int) bool { return false }, time.Millisecond)
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
