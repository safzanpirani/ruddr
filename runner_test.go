package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestControlSocketLocationIsPrivateAndLengthSafe(t *testing.T) {
	stateDir, err := os.MkdirTemp("/tmp", "rr-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(stateDir) })
	path, privateDir, err := controlSocketLocation(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if privateDir != "" {
		t.Fatalf("short state directory unexpectedly used fallback %q", privateDir)
	}
	if filepath.Dir(path) != stateDir {
		t.Fatalf("socket path %q is outside state directory %q", path, stateDir)
	}

	longStateDir := filepath.Join(stateDir, strings.Repeat("long-segment-", 12))
	path, privateDir, err = controlSocketLocation(longStateDir)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(privateDir) })
	if privateDir == "" || filepath.Dir(path) != privateDir {
		t.Fatalf("long path did not use a private fallback: path=%q dir=%q", path, privateDir)
	}
	if len([]byte(path)) > maxControlSocketPathBytes {
		t.Fatalf("fallback socket path is too long: %d bytes", len([]byte(path)))
	}
	info, err := os.Stat(privateDir)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o700 {
		t.Fatalf("fallback directory mode = %o, want 700", info.Mode().Perm())
	}
}

func TestControlCleanupDoesNotRemoveUnownedSocketPath(t *testing.T) {
	dir := t.TempDir()
	socketPath := filepath.Join(dir, ".ruddr.sock")
	if err := os.WriteFile(socketPath, []byte("not a socket"), 0o600); err != nil {
		t.Fatal(err)
	}
	r := &controller{store: &stateStore{state: runState{SocketPath: socketPath}}}
	r.closeControlServer()
	if _, err := os.Stat(socketPath); err != nil {
		t.Fatalf("cleanup removed unowned socket path: %v", err)
	}
}

func TestStateDoesNotPersistPromptText(t *testing.T) {
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	if err := os.WriteFile(promptPath, []byte("TOP SECRET PROMPT"), 0o600); err != nil {
		t.Fatal(err)
	}
	store, err := newStateStore(runConfig{
		CWD:        dir,
		PromptFile: promptPath,
		StateDir:   filepath.Join(dir, "run"),
		Model:      "test-model",
		Sandbox:    "read-only",
	})
	if err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(store.path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(raw), "TOP SECRET PROMPT") {
		t.Fatal("state file persisted prompt text")
	}
	info, err := os.Stat(store.path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("state mode = %o, want 600", info.Mode().Perm())
	}
	claimInfo, err := os.Stat(filepath.Join(store.state.StateDir, stateClaimFileName))
	if err != nil {
		t.Fatal(err)
	}
	if claimInfo.Mode().Perm() != 0o600 {
		t.Fatalf("state claim mode = %o, want 600", claimInfo.Mode().Perm())
	}
}

func TestNewStateStoreRejectsTerminalStateDirectoryReuse(t *testing.T) {
	stateDir := t.TempDir()
	existing := runState{
		Version:  2,
		Provider: providerCodex,
		PID:      os.Getpid(),
		Status:   "completed",
		StateDir: stateDir,
	}
	raw, err := json.Marshal(existing)
	if err != nil {
		t.Fatal(err)
	}
	artifacts := map[string][]byte{
		stateFileName:         raw,
		"events.jsonl":        []byte("old events\n"),
		"trace.log":           []byte("old trace\n"),
		"output.md":           []byte("old output\n"),
		"provider.stderr.log": []byte("old stderr\n"),
	}
	for name, content := range artifacts {
		if err := os.WriteFile(filepath.Join(stateDir, name), content, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	_, err = newStateStore(runConfig{
		Provider: providerCodex,
		CWD:      t.TempDir(),
		StateDir: stateDir,
		Model:    "test-model",
		Sandbox:  "read-only",
	})
	if err == nil || !strings.Contains(err.Error(), "already contains a Ruddr run") {
		t.Fatalf("state directory reuse error = %v", err)
	}
	for name, want := range artifacts {
		got, readErr := os.ReadFile(filepath.Join(stateDir, name))
		if readErr != nil {
			t.Fatal(readErr)
		}
		if !bytes.Equal(got, want) {
			t.Fatalf("artifact %s changed after rejected reuse", name)
		}
	}
}

func TestNewStateStoreRejectsArtifactsWithoutState(t *testing.T) {
	for _, name := range []string{
		"output.md",
		stateClaimFileName,
		stateFileName + ".tmp",
		"output.md.tmp",
	} {
		t.Run(name, func(t *testing.T) {
			stateDir := t.TempDir()
			artifactPath := filepath.Join(stateDir, name)
			want := []byte("preserve me\n")
			if err := os.WriteFile(artifactPath, want, 0o600); err != nil {
				t.Fatal(err)
			}
			_, err := newStateStore(runConfig{
				Provider: providerCodex,
				CWD:      t.TempDir(),
				StateDir: stateDir,
				Model:    "test-model",
				Sandbox:  "read-only",
			})
			if err == nil || !strings.Contains(err.Error(), "already contains Ruddr artifact "+name) {
				t.Fatalf("artifact-only state directory error = %v", err)
			}
			got, readErr := os.ReadFile(artifactPath)
			if readErr != nil {
				t.Fatal(readErr)
			}
			if !bytes.Equal(got, want) {
				t.Fatalf("existing artifact changed to %q", got)
			}
		})
	}
}

func TestNewStateStoreAtomicallyClaimsStateDirectory(t *testing.T) {
	stateDir := t.TempDir()
	var ready sync.WaitGroup
	ready.Add(2)
	release := make(chan struct{})
	type result struct {
		store *stateStore
		err   error
	}
	results := make(chan result, 2)
	for _, model := range []string{"model-a", "model-b"} {
		cwd := t.TempDir()
		go func() {
			store, err := newStateStore(runConfig{
				Provider: providerCodex,
				CWD:      cwd,
				StateDir: stateDir,
				Model:    model,
				Sandbox:  "read-only",
				BeforeStateReserve: func() {
					ready.Done()
					<-release
				},
			})
			results <- result{store: store, err: err}
		}()
	}
	ready.Wait()
	close(release)
	var winner *stateStore
	failed := 0
	for range 2 {
		result := <-results
		if result.err != nil {
			failed++
			if !strings.Contains(result.err.Error(), "claimed by another Ruddr run") {
				t.Fatalf("losing state claim error = %v", result.err)
			}
			continue
		}
		if winner != nil {
			t.Fatal("two controllers claimed the same state directory")
		}
		winner = result.store
	}
	if winner == nil || failed != 1 {
		t.Fatalf("state claims produced winner=%v failures=%d", winner != nil, failed)
	}
	persisted, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if persisted.Model != winner.snapshot().Model {
		t.Fatalf("loser changed winner state: persisted model=%q winner=%q", persisted.Model, winner.snapshot().Model)
	}
}

func TestOpenPrivateLogRefusesExistingFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	want := []byte("preserve me\n")
	if err := os.WriteFile(path, want, 0o600); err != nil {
		t.Fatal(err)
	}
	if file, err := openPrivateLog(path); err == nil {
		_ = file.Close()
		t.Fatal("openPrivateLog replaced an existing file")
	}
	got, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, want) {
		t.Fatalf("existing log changed to %q", got)
	}
}

func TestLogOpenFailurePersistsTerminalState(t *testing.T) {
	dir := t.TempDir()
	stateDir := filepath.Join(dir, "run")
	store, err := newStateStore(runConfig{
		Provider: providerCodex,
		CWD:      dir,
		StateDir: stateDir,
		Model:    "test-model",
		Sandbox:  "read-only",
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(stateDir, "events.jsonl"), 0o700); err != nil {
		t.Fatal(err)
	}
	r := &controller{
		store:       store,
		turnDone:    make(chan struct{}),
		sessionDone: make(chan struct{}),
	}
	err = r.openLogs()
	if err == nil {
		t.Fatal("log open unexpectedly succeeded")
	}
	r.fail(err)
	state, readErr := readState(stateDir)
	if readErr != nil {
		t.Fatal(readErr)
	}
	if state.Status != "failed" || state.CompletedAt.IsZero() {
		t.Fatalf("log-open failure left non-terminal state: %#v", state)
	}
}

func TestOneLineAndFlattenStrings(t *testing.T) {
	got := oneLine("hello\n  world", 20)
	if got != "hello world" {
		t.Fatalf("oneLine = %q", got)
	}
	longMessage := strings.Repeat("complete message ", 40)
	if got := singleLine(longMessage); got != strings.TrimSpace(longMessage) {
		t.Fatalf("singleLine truncated agent output: %q", got)
	}
	got = flattenStrings([]any{map[string]any{"text": "first"}, "second"})
	if got != "first second" {
		t.Fatalf("flattenStrings = %q", got)
	}
}

func TestRPCIDAcceptsStringAndIntegerIDs(t *testing.T) {
	for _, test := range []struct {
		name string
		raw  string
		want string
		ok   bool
	}{
		{name: "string", raw: `"ruddr-1"`, want: "ruddr-1", ok: true},
		{name: "integer", raw: `42`, want: "42", ok: true},
		{name: "negative integer", raw: `-7`, want: "-7", ok: true},
		{name: "empty string", raw: `""`},
		{name: "float", raw: `1.5`},
		{name: "null", raw: `null`},
	} {
		t.Run(test.name, func(t *testing.T) {
			got, ok := rpcID(json.RawMessage(test.raw))
			if ok != test.ok || got != test.want {
				t.Fatalf("rpcID(%s) = %q, %v; want %q, %v", test.raw, got, ok, test.want, test.ok)
			}
		})
	}
}

func TestTailLines(t *testing.T) {
	path := filepath.Join(t.TempDir(), "trace.log")
	if err := os.WriteFile(path, []byte("one\ntwo\nthree\nfour\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	lines, err := tailLines(path, 2)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(lines, ",") != "three,four" {
		t.Fatalf("tail = %#v", lines)
	}
}

func TestLiveSteerOverControlSocket(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("Initially say ORIGINAL"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	errCh := make(chan error, 1)
	go func() {
		errCh <- runController(runConfig{
			CWD:            dir,
			PromptFile:     promptPath,
			StateDir:       stateDir,
			Model:          "test-model",
			Sandbox:        "read-only",
			ApprovalPolicy: "never",
			ChildCommand:   []string{os.Args[0], "-test.run=TestLiveSteerOverControlSocket"},
		})
	}()

	deadline := time.Now().Add(3 * time.Second)
	for {
		state, err := readState(stateDir)
		if err == nil && state.Status == "active" && state.TurnID != "" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("fake turn did not become active")
		}
		time.Sleep(10 * time.Millisecond)
	}
	activeState, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	socketInfo, err := os.Stat(activeState.SocketPath)
	if err != nil {
		t.Fatal(err)
	}
	if socketInfo.Mode().Perm() != 0o600 {
		t.Fatalf("socket mode = %o, want 600", socketInfo.Mode().Perm())
	}
	parentInfo, err := os.Stat(filepath.Dir(activeState.SocketPath))
	if err != nil {
		t.Fatal(err)
	}
	if parentInfo.Mode().Perm() != 0o700 {
		t.Fatalf("socket parent mode = %o, want 700", parentInfo.Mode().Perm())
	}
	response, err := sendControl(stateDir, controlRequest{Command: "steer", Text: "Say STEERED"}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("steer failed: %s", response.Error)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
	output, err := os.ReadFile(filepath.Join(stateDir, "output.md"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.TrimSpace(string(output)) != "STEERED" {
		t.Fatalf("output = %q, want STEERED", output)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Steers != 1 || state.Status != "completed" {
		t.Fatalf("unexpected final state: %#v", state)
	}
}

func TestInterruptPreservesInterruptedStatus(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("stay active"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	errCh := make(chan error, 1)
	go func() {
		errCh <- runController(runConfig{
			CWD:            dir,
			PromptFile:     promptPath,
			StateDir:       stateDir,
			Model:          "test-model",
			Sandbox:        "read-only",
			ApprovalPolicy: "never",
			ChildCommand:   []string{os.Args[0], "-test.run=TestInterruptPreservesInterruptedStatus"},
		})
	}()
	waitForRunStatus(t, stateDir, "active")
	response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("interrupt failed: %s", response.Error)
	}
	if err := <-errCh; err == nil || !strings.Contains(err.Error(), "interrupted") {
		t.Fatalf("unexpected interrupted run result: %v", err)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Status != "interrupted" {
		t.Fatalf("status = %q, want interrupted", state.Status)
	}
}

func TestInterruptAcknowledgementForcesLocalTeardown(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("stay active"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_INTERRUPT_ACK_ONLY", "1")
	grandchildPIDFile := filepath.Join(dir, "grandchild.pid")
	if runtime.GOOS != "windows" {
		t.Setenv("GO_WANT_RUDDR_GRANDCHILD_PID_FILE", grandchildPIDFile)
		t.Setenv("GO_WANT_RUDDR_TERM_IGNORING_GRANDCHILD", "1")
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	errCh := make(chan error, 1)
	go func() {
		errCh <- runControllerContext(ctx, runConfig{
			CWD:            dir,
			PromptFile:     promptPath,
			StateDir:       stateDir,
			Model:          "test-model",
			Sandbox:        "read-only",
			ApprovalPolicy: "never",
			ChildCommand:   []string{os.Args[0], "-test.run=TestInterruptAcknowledgementForcesLocalTeardown"},
		})
	}()

	activeState := waitForRunStatus(t, stateDir, "active")
	grandchildPID := 0
	if runtime.GOOS != "windows" {
		rawPID, err := os.ReadFile(grandchildPIDFile)
		if err != nil {
			t.Fatal(err)
		}
		grandchildPID, err = strconv.Atoi(strings.TrimSpace(string(rawPID)))
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() {
			if process, err := os.FindProcess(grandchildPID); err == nil {
				_ = process.Kill()
			}
		})
	}
	response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("interrupt failed: %s", response.Error)
	}
	select {
	case err := <-errCh:
		if err == nil || !strings.Contains(err.Error(), "interrupted") {
			t.Fatalf("unexpected interrupted run result: %v", err)
		}
	case <-time.After(2 * time.Second):
		cancel()
		<-errCh
		t.Fatal("acknowledged interrupt did not stop the local controller")
	}
	finalState, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if finalState.Status != "interrupted" {
		t.Fatalf("status = %q, want interrupted", finalState.Status)
	}
	if processAlive(activeState.ChildPID) {
		t.Fatalf("child pid %d is still alive", activeState.ChildPID)
	}
	if grandchildPID != 0 {
		deadline := time.Now().Add(2 * time.Second)
		for processAlive(grandchildPID) && time.Now().Before(deadline) {
			time.Sleep(10 * time.Millisecond)
		}
		if processAlive(grandchildPID) {
			t.Fatalf("grandchild pid %d is still alive", grandchildPID)
		}
	}
	if _, err := os.Lstat(activeState.SocketPath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("socket still exists or cannot be checked: %v", err)
	}
}

func TestResumeAndForkThreadBeforeTurn(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	for _, test := range []struct {
		name       string
		resumeID   string
		forkID     string
		wantThread string
	}{
		{name: "resume", resumeID: "source-thread", wantThread: "thread-resumed"},
		{name: "fork", forkID: "source-thread", wantThread: "thread-forked"},
	} {
		t.Run(test.name, func(t *testing.T) {
			dir := t.TempDir()
			promptPath := filepath.Join(dir, "prompt.md")
			stateDir := filepath.Join(dir, "run")
			if err := os.WriteFile(promptPath, []byte("continue the task"), 0o600); err != nil {
				t.Fatal(err)
			}
			t.Setenv("GO_WANT_RUDDR_HELPER", "1")
			t.Setenv("GO_WANT_RUDDR_COMPLETE_ON_START", "1")
			err := runController(runConfig{
				CWD:            dir,
				PromptFile:     promptPath,
				StateDir:       stateDir,
				Model:          "test-model",
				Sandbox:        "read-only",
				ApprovalPolicy: "never",
				ResumeThreadID: test.resumeID,
				ForkThreadID:   test.forkID,
				ChildCommand:   []string{os.Args[0], "-test.run=TestResumeAndForkThreadBeforeTurn"},
			})
			if err != nil {
				t.Fatal(err)
			}
			state, err := readState(stateDir)
			if err != nil {
				t.Fatal(err)
			}
			if state.ThreadID != test.wantThread || state.Status != "completed" {
				t.Fatalf("unexpected final state: %#v", state)
			}
		})
	}
}

func TestForkBoundarySelectorsReachAppServer(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	for _, test := range []struct {
		name      string
		beforeID  string
		throughID string
		expectEnv string
	}{
		{name: "before-turn-excludes-boundary", beforeID: "turn-old", expectEnv: "GO_WANT_RUDDR_EXPECT_FORK_BEFORE"},
		{name: "through-turn-includes-boundary", throughID: "turn-new", expectEnv: "GO_WANT_RUDDR_EXPECT_FORK_THROUGH"},
	} {
		t.Run(test.name, func(t *testing.T) {
			dir := t.TempDir()
			promptPath := filepath.Join(dir, "prompt.md")
			stateDir := filepath.Join(dir, "run")
			if err := os.WriteFile(promptPath, []byte("alternate approach"), 0o600); err != nil {
				t.Fatal(err)
			}
			t.Setenv("GO_WANT_RUDDR_HELPER", "1")
			t.Setenv("GO_WANT_RUDDR_COMPLETE_ON_START", "1")
			want := test.beforeID + test.throughID
			t.Setenv(test.expectEnv, want)
			err := runController(runConfig{
				CWD:               dir,
				PromptFile:        promptPath,
				StateDir:          stateDir,
				Model:             "test-model",
				Sandbox:           "read-only",
				ApprovalPolicy:    "never",
				ForkThreadID:      "source-thread",
				ForkBeforeTurnID:  test.beforeID,
				ForkThroughTurnID: test.throughID,
				ChildCommand:      []string{os.Args[0], "-test.run=TestForkBoundarySelectorsReachAppServer"},
			})
			if err != nil {
				t.Fatal(err)
			}
			state, err := readState(stateDir)
			if err != nil {
				t.Fatal(err)
			}
			if state.ThreadID != "thread-forked" || state.ThreadID == "source-thread" {
				t.Fatalf("fork did not produce a new thread: %#v", state)
			}
		})
	}
}

func TestForkInvalidTurnFailsRun(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("alternate approach"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	err := runController(runConfig{
		CWD:              dir,
		PromptFile:       promptPath,
		StateDir:         stateDir,
		Model:            "test-model",
		Sandbox:          "read-only",
		ApprovalPolicy:   "never",
		ForkThreadID:     "source-thread",
		ForkBeforeTurnID: "turn-missing",
		ChildCommand:     []string{os.Args[0], "-test.run=TestForkInvalidTurnFailsRun"},
	})
	if err == nil || !strings.Contains(err.Error(), "unknown turn") {
		t.Fatalf("invalid fork turn did not fail the run: %v", err)
	}
	state, readErr := readState(stateDir)
	if readErr != nil {
		t.Fatal(readErr)
	}
	if state.Status != "failed" || state.ThreadID != "" {
		t.Fatalf("invalid fork left bad state: %#v", state)
	}
}

func TestResumeSendsCleanParams(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("continue"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_COMPLETE_ON_START", "1")
	t.Setenv("GO_WANT_RUDDR_EXPECT_RESUME_PARAMS", "1")
	err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		Ephemeral:      true,
		ResumeThreadID: "source-thread",
		ChildCommand:   []string{os.Args[0], "-test.run=TestResumeSendsCleanParams"},
	})
	if err != nil {
		t.Fatal(err)
	}
	state, readErr := readState(stateDir)
	if readErr != nil {
		t.Fatal(readErr)
	}
	if state.ThreadID != "thread-resumed" {
		t.Fatalf("resume did not continue the source thread: %#v", state)
	}
}

func TestValidateRunConfigRejectsForkSelectorMisuse(t *testing.T) {
	base := runConfig{
		CWD:          os.TempDir(),
		Model:        "test-model",
		Sandbox:      "read-only",
		ChildCommand: []string{"codex"},
	}
	both := base
	both.ForkThreadID = "source-thread"
	both.ForkBeforeTurnID = "turn-a"
	both.ForkThroughTurnID = "turn-b"
	if err := validateRunConfig(&both); err == nil || !strings.Contains(err.Error(), "mutually exclusive") {
		t.Fatalf("expected selector mutual-exclusion error, got %v", err)
	}
	orphan := base
	orphan.ForkBeforeTurnID = "turn-a"
	if err := validateRunConfig(&orphan); err == nil || !strings.Contains(err.Error(), "require --fork-thread") {
		t.Fatalf("expected orphan selector error, got %v", err)
	}
	resumeSelector := base
	resumeSelector.ResumeThreadID = "source-thread"
	resumeSelector.ForkThroughTurnID = "turn-b"
	if err := validateRunConfig(&resumeSelector); err == nil {
		t.Fatal("fork selector with --resume-thread was accepted")
	}
}

func TestValidateRunConfigRejectsNegativeTimeouts(t *testing.T) {
	base := runConfig{
		CWD:          t.TempDir(),
		Model:        "test-model",
		Sandbox:      "read-only",
		ChildCommand: []string{"codex"},
	}
	negativeTurn := base
	negativeTurn.TurnTimeout = -time.Second
	if err := validateRunConfig(&negativeTurn); err == nil || !strings.Contains(err.Error(), "--turn-timeout") {
		t.Fatalf("negative turn timeout error = %v", err)
	}
	negativeIdle := base
	negativeIdle.IdleTimeout = -time.Second
	if err := validateRunConfig(&negativeIdle); err == nil || !strings.Contains(err.Error(), "--idle-timeout") {
		t.Fatalf("negative idle timeout error = %v", err)
	}
	if err := validateRunConfig(&base); err != nil {
		t.Fatalf("zero timeouts should remain valid: %v", err)
	}
}

func TestWaitTimeoutParsesGoDurations(t *testing.T) {
	stateDir := t.TempDir()
	state := runState{Version: 1, PID: os.Getpid(), Status: "completed", StateDir: stateDir}
	raw, err := json.Marshal(state)
	if err != nil {
		t.Fatal(err)
	}
	if err := writePrivateFile(filepath.Join(stateDir, stateFileName), raw); err != nil {
		t.Fatal(err)
	}
	if err := waitCommand([]string{"--state-dir", stateDir, "--timeout", "3600s"}); err != nil {
		t.Fatalf("duration timeout rejected: %v", err)
	}
	err = waitCommand([]string{"--state-dir", stateDir, "--timeout", "3600"})
	if err == nil || !strings.Contains(err.Error(), "parse") {
		t.Fatalf("bare integer timeout should fail duration parsing, got %v", err)
	}
	err = waitCommand([]string{"--state-dir", stateDir, "--timeout", "-1s"})
	if err == nil || !strings.Contains(err.Error(), "zero or positive") {
		t.Fatalf("negative wait timeout should fail, got %v", err)
	}
}

func TestRunCommandRequiresExplicitChildSeparator(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	if err := os.WriteFile(promptPath, []byte("task"), 0o600); err != nil {
		t.Fatal(err)
	}
	base := []string{"--prompt-file", promptPath, "--state-dir", filepath.Join(dir, "bare-run")}
	err := runCommand(append(base, os.Args[0], "-test.run=TestRunCommandRequiresExplicitChildSeparator"))
	if err == nil || !strings.Contains(err.Error(), "after --") {
		t.Fatalf("bare child command error = %v", err)
	}
	if err := runCommand(append(base, "--")); err == nil || !strings.Contains(err.Error(), "after -- is empty") {
		t.Fatalf("empty child command error = %v", err)
	}

	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_COMPLETE_ON_START", "1")
	args := []string{
		"--prompt-file", promptPath,
		"--state-dir", filepath.Join(dir, "explicit-run"),
		"--", os.Args[0], "-test.run=TestRunCommandRequiresExplicitChildSeparator",
	}
	if err := runCommand(args); err != nil {
		t.Fatalf("explicit child command failed: %v", err)
	}
}

func TestThreadListRPC(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	raw, err := invokeAppServer(t.TempDir(), []string{os.Args[0], "-test.run=TestThreadListRPC"}, "thread/list", map[string]any{"limit": 2})
	if err != nil {
		t.Fatal(err)
	}
	var result struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
		NextCursor string `json:"nextCursor"`
	}
	if err := json.Unmarshal(raw, &result); err != nil {
		t.Fatal(err)
	}
	if len(result.Data) != 1 || result.Data[0].ID != "thread-listed" || result.NextCursor != "next-page" {
		t.Fatalf("unexpected list result: %#v", result)
	}
}

func TestThreadSubcommands(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	child := []string{"--", os.Args[0], "-test.run=TestThreadSubcommands"}
	tests := []struct {
		name string
		args []string
	}{
		{name: "list", args: append([]string{"list", "--limit", "1"}, child...)},
		{name: "search", args: append([]string{"search", "needle"}, child...)},
		{name: "read", args: append([]string{"read", "--include-turns", "source-thread"}, child...)},
		{name: "turns", args: append([]string{"turns", "--limit", "1", "source-thread"}, child...)},
		{name: "fork", args: append([]string{"fork", "--before-turn", "turn-old", "source-thread"}, child...)},
		{name: "name", args: append([]string{"name", "source-thread", "New", "Name"}, child...)},
		{name: "archive", args: append([]string{"archive", "source-thread"}, child...)},
		{name: "unarchive", args: append([]string{"unarchive", "source-thread"}, child...)},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			output, err := captureStdout(func() error { return threadCommand(test.args) })
			if err != nil {
				t.Fatal(err)
			}
			if !json.Valid(output) {
				t.Fatalf("thread command returned invalid JSON: %q", output)
			}
		})
	}
}

func captureStdout(fn func() error) ([]byte, error) {
	reader, writer, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	original := os.Stdout
	os.Stdout = writer
	runErr := fn()
	_ = writer.Close()
	os.Stdout = original
	output, readErr := io.ReadAll(reader)
	_ = reader.Close()
	if runErr != nil {
		return output, runErr
	}
	return output, readErr
}

func TestValidateRunConfigRejectsConflictingThreadModes(t *testing.T) {
	cfg := runConfig{
		CWD:            t.TempDir(),
		Model:          "test-model",
		Sandbox:        "read-only",
		ResumeThreadID: "resume",
		ForkThreadID:   "fork",
		ChildCommand:   []string{"codex"},
	}
	if err := validateRunConfig(&cfg); err == nil || !strings.Contains(err.Error(), "mutually exclusive") {
		t.Fatalf("expected mutually exclusive error, got %v", err)
	}
}

func TestEphemeralRunPassesThreadOption(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	if err := os.WriteFile(promptPath, []byte("ephemeral task"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_EXPECT_EPHEMERAL", "1")
	t.Setenv("GO_WANT_RUDDR_COMPLETE_ON_START", "1")
	err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       filepath.Join(dir, "run"),
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		Ephemeral:      true,
		ChildCommand:   []string{os.Args[0], "-test.run=TestEphemeralRunPassesThreadOption"},
	})
	if err != nil {
		t.Fatal(err)
	}
}

func TestWaitMissingStateFailsImmediately(t *testing.T) {
	started := time.Now()
	err := waitCommand([]string{"--state-dir", filepath.Join(t.TempDir(), "missing")})
	if err == nil {
		t.Fatal("wait unexpectedly succeeded")
	}
	if time.Since(started) > time.Second {
		t.Fatalf("wait did not fail promptly: %v", time.Since(started))
	}
	if !strings.Contains(err.Error(), stateFileName) {
		t.Fatalf("wait error %q does not identify the missing state file", err)
	}
}

func TestControlRequestRejectsStaleActiveState(t *testing.T) {
	stateDir := t.TempDir()
	state := runState{
		Version:    1,
		PID:        99999999,
		Status:     "active",
		ThreadID:   "thread-stale",
		TurnID:     "turn-stale",
		StateDir:   stateDir,
		SocketPath: filepath.Join(stateDir, "missing.sock"),
	}
	raw, err := json.Marshal(state)
	if err != nil {
		t.Fatal(err)
	}
	if err := writePrivateFile(filepath.Join(stateDir, stateFileName), raw); err != nil {
		t.Fatal(err)
	}
	_, err = sendControl(stateDir, controlRequest{Command: "steer", Text: "new direction"}, time.Second)
	if err == nil || !strings.Contains(err.Error(), "is not running") {
		t.Fatalf("expected stale-process error, got %v", err)
	}
}

func TestDisplayedStateMarksDeadControllerStale(t *testing.T) {
	state := displayedState(runState{PID: 99999999, Status: "active", Error: ""})
	if state.Status != "stale" || !strings.Contains(state.Error, "not running") {
		t.Fatalf("dead controller was not marked stale: %#v", state)
	}
	completed := displayedState(runState{PID: 99999999, Status: "completed"})
	if completed.Status != "completed" {
		t.Fatalf("terminal state changed: %#v", completed)
	}
}

func TestCanceledRunCleansUpChildSocketAndState(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("stay active"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	grandchildPIDFile := filepath.Join(dir, "grandchild.pid")
	if runtime.GOOS != "windows" {
		t.Setenv("GO_WANT_RUDDR_GRANDCHILD_PID_FILE", grandchildPIDFile)
	}
	ctx, cancel := context.WithCancel(context.Background())
	errCh := make(chan error, 1)
	go func() {
		errCh <- runControllerContext(ctx, runConfig{
			CWD:            dir,
			PromptFile:     promptPath,
			StateDir:       stateDir,
			Model:          "test-model",
			Sandbox:        "read-only",
			ApprovalPolicy: "never",
			ChildCommand:   []string{os.Args[0], "-test.run=TestCanceledRunCleansUpChildSocketAndState"},
		})
	}()

	state := waitForRunStatus(t, stateDir, "active")
	grandchildPID := 0
	if runtime.GOOS != "windows" {
		rawPID, err := os.ReadFile(grandchildPIDFile)
		if err != nil {
			t.Fatal(err)
		}
		grandchildPID, err = strconv.Atoi(strings.TrimSpace(string(rawPID)))
		if err != nil {
			t.Fatal(err)
		}
	}
	cancel()
	select {
	case err := <-errCh:
		if err == nil || !strings.Contains(err.Error(), "interrupted") {
			t.Fatalf("unexpected cancellation result: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("canceled run did not exit")
	}
	finalState, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if finalState.Status != "interrupted" {
		t.Fatalf("status = %q, want interrupted", finalState.Status)
	}
	if processAlive(state.ChildPID) {
		t.Fatalf("child pid %d is still alive", state.ChildPID)
	}
	if grandchildPID != 0 {
		deadline := time.Now().Add(2 * time.Second)
		for processAlive(grandchildPID) && time.Now().Before(deadline) {
			time.Sleep(10 * time.Millisecond)
		}
		if processAlive(grandchildPID) {
			t.Fatalf("grandchild pid %d is still alive", grandchildPID)
		}
	}
	if _, err := os.Lstat(state.SocketPath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("socket still exists or cannot be checked: %v", err)
	}
	if state.SocketDir != "" {
		if _, err := os.Stat(state.SocketDir); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("fallback socket directory still exists: %v", err)
		}
	}
}

func TestTurnWatchdogStopsHungRun(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("stay active"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		TurnTimeout:    50 * time.Millisecond,
		ChildCommand:   []string{os.Args[0], "-test.run=TestTurnWatchdogStopsHungRun"},
	})
	if err == nil || !strings.Contains(err.Error(), "watchdog") {
		t.Fatalf("unexpected watchdog result: %v", err)
	}
	state, readErr := readState(stateDir)
	if readErr != nil {
		t.Fatal(readErr)
	}
	if state.Status != "failed" || processAlive(state.ChildPID) {
		t.Fatalf("watchdog left bad state: %#v", state)
	}
}

type blockingWriteCloser struct {
	closed chan struct{}
	once   sync.Once
}

func (w *blockingWriteCloser) Write([]byte) (int, error) {
	<-w.closed
	return 0, os.ErrClosed
}

func (w *blockingWriteCloser) Close() error {
	w.once.Do(func() { close(w.closed) })
	return nil
}

func TestRPCCallBoundsBlockedStdinWrite(t *testing.T) {
	writer := &blockingWriteCloser{closed: make(chan struct{})}
	r := &controller{
		childIn:     writer,
		pending:     make(map[string]chan rpcEnvelope),
		turnDone:    make(chan struct{}),
		sessionDone: make(chan struct{}),
	}
	errCh := make(chan error, 1)
	go func() {
		errCh <- r.call("thread/list", map[string]any{}, nil, 50*time.Millisecond)
	}()
	select {
	case err := <-errCh:
		if err == nil || !strings.Contains(err.Error(), "write timed out") {
			t.Fatalf("unexpected blocked-write result: %v", err)
		}
	case <-time.After(500 * time.Millisecond):
		_ = writer.Close()
		<-errCh
		t.Fatal("blocked stdin write ignored the RPC timeout")
	}
}

func TestRPCDeadlineIncludesWaitingForWriter(t *testing.T) {
	writer := &blockingWriteCloser{closed: make(chan struct{})}
	r := &controller{childIn: writer, pending: make(map[string]chan rpcEnvelope)}
	r.writeOnce.Do(func() { r.writeGate = make(chan struct{}, 1) })
	r.writeGate <- struct{}{}
	defer func() { <-r.writeGate }()
	errCh := make(chan error, 1)
	go func() { errCh <- r.call("turn/steer", map[string]any{}, nil, 20*time.Millisecond) }()
	select {
	case err := <-errCh:
		if err == nil || !strings.Contains(err.Error(), "write timed out") {
			t.Fatalf("queued RPC error: %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("queued RPC did not honor its deadline")
	}
	select {
	case <-writer.closed:
		t.Fatal("queued timeout closed stdin belonging to another writer")
	default:
	}
	if len(r.pending) != 0 {
		t.Fatal("timed out RPC left a pending response")
	}
}

func TestIdleTurnDoesNotPublishTerminalSessionState(t *testing.T) {
	for _, status := range []string{"completed", "failed", "interrupted"} {
		t.Run(status, func(t *testing.T) {
			store, err := newStateStore(runConfig{StateDir: filepath.Join(t.TempDir(), "run"), Idle: true})
			if err != nil {
				t.Fatal(err)
			}
			r := &controller{cfg: runConfig{Idle: true}, store: store, turnDone: make(chan struct{}), sessionDone: make(chan struct{})}
			defer r.closeControlServer()
			if !r.finishTurn(status, "") {
				t.Fatal("turn did not finish")
			}
			state, err := readState(store.snapshot().StateDir)
			if err != nil || state.Status != "idle" || !state.CompletedAt.IsZero() {
				t.Fatalf("inter-turn state = %+v, %v", state, err)
			}
			if got := r.interruptSettlementResult(); (got != nil) != (status == "failed") {
				t.Fatalf("interrupted turn outcome for %s = %v", status, got)
			}
			r.persistFinalIdleExit()
			state, err = readState(store.snapshot().StateDir)
			if err != nil || state.Status != status || state.CompletedAt.IsZero() {
				t.Fatalf("final state = %+v, %v", state, err)
			}
		})
	}
}

type temporaryAcceptError struct{}

func (temporaryAcceptError) Error() string   { return "temporary accept failure" }
func (temporaryAcceptError) Timeout() bool   { return false }
func (temporaryAcceptError) Temporary() bool { return true }

type sequenceListener struct {
	conn  net.Conn
	calls int
}

func (l *sequenceListener) Accept() (net.Conn, error) {
	l.calls++
	switch l.calls {
	case 1:
		return nil, temporaryAcceptError{}
	case 2:
		return l.conn, nil
	default:
		return nil, net.ErrClosed
	}
}

func (l *sequenceListener) Close() error   { return nil }
func (l *sequenceListener) Addr() net.Addr { return &net.UnixAddr{Name: "test", Net: "unix"} }

func TestControlAcceptRetriesTemporaryFailure(t *testing.T) {
	server, client := net.Pipe()
	defer client.Close()
	r := &controller{
		listener:    &sequenceListener{conn: server},
		store:       &stateStore{state: runState{Status: "active"}},
		turnDone:    make(chan struct{}),
		sessionDone: make(chan struct{}),
	}
	go r.acceptControl()
	_ = client.SetDeadline(time.Now().Add(time.Second))
	if err := json.NewEncoder(client).Encode(controlRequest{Command: "status"}); err != nil {
		t.Fatal(err)
	}
	var response controlResponse
	if err := json.NewDecoder(client).Decode(&response); err != nil {
		t.Fatalf("control socket did not recover after temporary accept failure: %v", err)
	}
	if !response.OK {
		t.Fatalf("unexpected control response: %#v", response)
	}
}

func TestRunPreservesAgentMessagesAndRedactsPersistedError(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("sensitive user prompt"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_MULTI_OUTPUT_ERROR", "1")
	err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		ChildCommand:   []string{os.Args[0], "-test.run=TestRunPreservesAgentMessagesAndRedactsPersistedError"},
	})
	if err == nil {
		t.Fatal("failed helper turn unexpectedly succeeded")
	}
	output, readErr := os.ReadFile(filepath.Join(stateDir, "output.md"))
	if readErr != nil {
		t.Fatal(readErr)
	}
	if string(output) != "FIRST\n\nSECOND\n" {
		t.Fatalf("output = %q, want both agent messages", output)
	}
	stateRaw, readErr := os.ReadFile(filepath.Join(stateDir, stateFileName))
	if readErr != nil {
		t.Fatal(readErr)
	}
	if bytes.Contains(stateRaw, []byte("SECRET_ECHO")) {
		t.Fatalf("state persisted app-server content: %s", stateRaw)
	}
	state, readErr := readState(stateDir)
	if readErr != nil {
		t.Fatal(readErr)
	}
	if state.Error == "" || !strings.Contains(state.Error, "see trace.log") {
		t.Fatalf("state error is not a useful redacted diagnostic: %q", state.Error)
	}
}

func TestNestedTurnLifecycleDoesNotReplaceOrCompleteRootTurn(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("delegate and finish"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_NESTED_TURN", "1")
	err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		ChildCommand:   []string{os.Args[0], "-test.run=TestNestedTurnLifecycleDoesNotReplaceOrCompleteRootTurn"},
	})
	if err != nil {
		t.Fatal(err)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.ThreadID != "thread-test" || state.TurnID != "turn-test" {
		t.Fatalf("nested lifecycle replaced root identity: thread=%q turn=%q", state.ThreadID, state.TurnID)
	}
	output, err := os.ReadFile(filepath.Join(stateDir, "output.md"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.TrimSpace(string(output)) != "ROOT DONE" {
		t.Fatalf("output = %q, want root output after nested completion", output)
	}
}

func TestChildCommandTraceDoesNotPersistArguments(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("complete"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_COMPLETE_ON_START", "1")
	const secretArgument = "TOP-SECRET-BEARER-TOKEN"
	err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		ChildCommand:   []string{os.Args[0], "-test.run=TestChildCommandTraceDoesNotPersistArguments", secretArgument},
	})
	if err != nil {
		t.Fatal(err)
	}
	trace, err := os.ReadFile(filepath.Join(stateDir, "trace.log"))
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(trace, []byte(secretArgument)) {
		t.Fatalf("trace persisted child argument: %s", trace)
	}
	if !bytes.Contains(trace, []byte("[start] child pid=")) {
		t.Fatalf("trace omitted child startup marker: %s", trace)
	}
}

func TestRunDoesNotReportSuccessWhenStatePersistenceFails(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("stay active"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	errCh := make(chan error, 1)
	go func() {
		errCh <- runControllerContext(ctx, runConfig{
			CWD:            dir,
			PromptFile:     promptPath,
			StateDir:       stateDir,
			Model:          "test-model",
			Sandbox:        "read-only",
			ApprovalPolicy: "never",
			ChildCommand:   []string{os.Args[0], "-test.run=TestRunDoesNotReportSuccessWhenStatePersistenceFails"},
		})
	}()

	active := waitForRunStatus(t, stateDir, "active")
	// Connect before moving the directory, since short paths keep the socket
	// inside it. The open connection survives the rename on every platform.
	conn, err := net.DialTimeout("unix", active.SocketPath, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(time.Second))
	// Keep the last durable state readable, but make the controller's original
	// artifact directory unwritable regardless of temporary-file naming.
	savedDir := stateDir + "-saved"
	if err := os.Rename(stateDir, savedDir); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(stateDir, []byte("blocked"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := json.NewEncoder(conn).Encode(controlRequest{Command: "steer", Text: "finish now"}); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-errCh:
		if err == nil || !strings.Contains(err.Error(), "persist") {
			t.Fatalf("run result = %v, want persistence failure", err)
		}
	case <-time.After(2 * time.Second):
		cancel()
		<-errCh
		t.Fatal("run did not stop after state persistence failed")
	}
	persisted, err := readState(savedDir)
	if err != nil {
		t.Fatal(err)
	}
	if persisted.Status != "active" {
		t.Fatalf("partially persisted status = %q, want prior durable active state", persisted.Status)
	}
}

func TestRunFailsWhenAgentOutputCannotBePersisted(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("stay active"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	errCh := make(chan error, 1)
	go func() {
		errCh <- runControllerContext(ctx, runConfig{
			CWD:            dir,
			PromptFile:     promptPath,
			StateDir:       stateDir,
			Model:          "test-model",
			Sandbox:        "read-only",
			ApprovalPolicy: "never",
			ChildCommand:   []string{os.Args[0], "-test.run=TestRunFailsWhenAgentOutputCannotBePersisted"},
		})
	}()

	waitForRunStatus(t, stateDir, "active")
	outputPath := filepath.Join(stateDir, "output.md")
	if err := os.Rename(outputPath, outputPath+".saved"); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(outputPath, 0o700); err != nil {
		t.Fatal(err)
	}
	_, _ = sendControl(stateDir, controlRequest{Command: "steer", Text: "finish now"}, time.Second)
	select {
	case err := <-errCh:
		if err == nil || !strings.Contains(err.Error(), "persist agent output") {
			t.Fatalf("run result = %v, want output persistence failure", err)
		}
	case <-time.After(2 * time.Second):
		cancel()
		<-errCh
		t.Fatal("run did not stop after output persistence failed")
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Status != "failed" {
		t.Fatalf("status = %q, want failed", state.Status)
	}
	output, err := os.ReadFile(outputPath + ".saved")
	if err != nil {
		t.Fatal(err)
	}
	if len(output) != 0 {
		t.Fatalf("failed output persistence changed reserved output.md to %q", output)
	}
}

func waitForRunStatus(t *testing.T, stateDir, want string) runState {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		state, err := readState(stateDir)
		if err == nil && state.Status == want {
			return state
		}
		if time.Now().After(deadline) {
			t.Fatalf("run did not reach %s", want)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func runHelperAppServer() {
	if pidFile := os.Getenv("GO_WANT_RUDDR_GRANDCHILD_PID_FILE"); pidFile != "" {
		var child *exec.Cmd
		if os.Getenv("GO_WANT_RUDDR_TERM_IGNORING_GRANDCHILD") == "1" {
			child = exec.Command("sh", "-c", `trap '' TERM; echo $$ > "$1"; while :; do sleep 1; done`, "ruddr-grandchild", pidFile)
		} else {
			child = exec.Command("sleep", "60")
		}
		if err := child.Start(); err == nil {
			if os.Getenv("GO_WANT_RUDDR_TERM_IGNORING_GRANDCHILD") != "1" {
				_ = os.WriteFile(pidFile, []byte(strconv.Itoa(child.Process.Pid)), 0o600)
			} else {
				deadline := time.Now().Add(2 * time.Second)
				for time.Now().Before(deadline) {
					if raw, err := os.ReadFile(pidFile); err == nil && len(bytes.TrimSpace(raw)) > 0 {
						break
					}
					time.Sleep(time.Millisecond)
				}
			}
		}
	}
	scanner := bufio.NewScanner(os.Stdin)
	enc := json.NewEncoder(os.Stdout)
	turnCounter := 0
	currentTurn := "turn-test"
	rejectedSecondTurn := false
	deferredInterruptID := ""
	for scanner.Scan() {
		var request struct {
			ID     string         `json:"id"`
			Method string         `json:"method"`
			Params map[string]any `json:"params"`
		}
		if err := json.Unmarshal(scanner.Bytes(), &request); err != nil {
			continue
		}
		switch request.Method {
		case "initialize":
			if os.Getenv("GO_WANT_RUDDR_IGNORE_INITIALIZE") == "1" {
				continue
			}
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"userAgent": "fake", "codexHome": "/tmp", "platformFamily": "unix", "platformOs": "test"}})
		case "thread/start":
			if os.Getenv("GO_WANT_RUDDR_EXPECT_EPHEMERAL") == "1" && request.Params["ephemeral"] != true {
				_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "ephemeral option missing"}})
				continue
			}
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"thread": map[string]any{"id": "thread-test"}}})
		case "thread/resume":
			if request.Params["threadId"] != "source-thread" {
				_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "wrong source thread"}})
				continue
			}
			if os.Getenv("GO_WANT_RUDDR_EXPECT_RESUME_PARAMS") == "1" {
				if request.Params["excludeTurns"] != true {
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "resume missing excludeTurns"}})
					continue
				}
				if _, has := request.Params["ephemeral"]; has {
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "resume must not send ephemeral"}})
					continue
				}
				if _, has := request.Params["serviceName"]; has {
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "resume must not send serviceName"}})
					continue
				}
			}
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"thread": map[string]any{"id": "thread-resumed"}}})
		case "thread/fork":
			if request.Params["threadId"] != "source-thread" {
				_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "wrong source thread"}})
				continue
			}
			if request.Params["beforeTurnId"] == "turn-missing" || request.Params["lastTurnId"] == "turn-missing" {
				_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "unknown turn turn-missing"}})
				continue
			}
			if want := os.Getenv("GO_WANT_RUDDR_EXPECT_FORK_BEFORE"); want != "" {
				if request.Params["beforeTurnId"] != want {
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "beforeTurnId mismatch"}})
					continue
				}
				if _, has := request.Params["lastTurnId"]; has {
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "beforeTurnId fork must not send lastTurnId"}})
					continue
				}
			}
			if want := os.Getenv("GO_WANT_RUDDR_EXPECT_FORK_THROUGH"); want != "" {
				if request.Params["lastTurnId"] != want {
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "lastTurnId mismatch"}})
					continue
				}
				if _, has := request.Params["beforeTurnId"]; has {
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "lastTurnId fork must not send beforeTurnId"}})
					continue
				}
			}
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"thread": map[string]any{"id": "thread-forked"}}})
		case "thread/list":
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{
				"data":       []map[string]any{{"id": "thread-listed"}},
				"nextCursor": "next-page",
			}})
		case "thread/search":
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"data": []map[string]any{{"thread": map[string]any{"id": "thread-found"}, "snippet": "needle"}}}})
		case "thread/read":
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"thread": map[string]any{"id": request.Params["threadId"]}}})
		case "thread/turns/list":
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"data": []map[string]any{{"id": "turn-listed"}}}})
		case "thread/name/set", "thread/archive":
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{}})
		case "thread/unarchive":
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"thread": map[string]any{"id": request.Params["threadId"]}}})
		case "turn/start":
			if os.Getenv("GO_WANT_RUDDR_MULTI_TURN") == "1" {
				turnCounter++
				currentTurn = fmt.Sprintf("turn-%d", turnCounter)
				if turnCounter > 1 && !rejectedSecondTurn && os.Getenv("GO_WANT_RUDDR_REJECT_SECOND_TURN") == "1" {
					rejectedSecondTurn = true
					_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "second turn rejected"}})
					turnCounter--
					currentTurn = fmt.Sprintf("turn-%d", turnCounter)
					continue
				}
				if turnCounter > 1 && os.Getenv("GO_WANT_RUDDR_AMBIGUOUS_SECOND_TURN") == "1" {
					_ = enc.Encode(map[string]any{"method": "turn/started", "params": map[string]any{"threadId": "thread-test", "turn": map[string]any{"id": currentTurn, "status": "inProgress"}}})
					continue
				}
				if os.Getenv("GO_WANT_RUDDR_DELAY_TURN_START") == "1" {
					time.Sleep(200 * time.Millisecond)
				}
				_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"turn": map[string]any{"id": currentTurn, "status": "inProgress"}}})
				_ = enc.Encode(map[string]any{"method": "turn/started", "params": map[string]any{"threadId": "thread-test", "turn": map[string]any{"id": currentTurn, "status": "inProgress"}}})
				if deferredInterruptID != "" {
					_ = enc.Encode(map[string]any{"id": deferredInterruptID, "error": map[string]any{"code": -32602, "message": "old turn already completed"}})
					deferredInterruptID = ""
				}
				if os.Getenv("GO_WANT_RUDDR_MULTI_TURN_HOLD") != "1" {
					_ = enc.Encode(map[string]any{"method": "thread/tokenUsage/updated", "params": map[string]any{
						"threadId": "thread-test",
						"tokenUsage": map[string]any{
							"total":              map[string]any{"totalTokens": 100 * turnCounter, "inputTokens": 80 * turnCounter, "cachedInputTokens": 10 * turnCounter, "outputTokens": 20 * turnCounter},
							"modelContextWindow": 1000,
						},
					}})
					_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"id": fmt.Sprintf("message-%d", turnCounter), "type": "agentMessage", "text": fmt.Sprintf("TURN %d", turnCounter)}}})
					_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": currentTurn, "status": "completed"}}})
					if os.Getenv("GO_WANT_RUDDR_EXIT_AFTER_TURN") == "1" {
						return
					}
				}
				continue
			}
			if os.Getenv("GO_WANT_RUDDR_COMPLETE_BEFORE_TURN_RESPONSE") == "1" {
				_ = enc.Encode(map[string]any{"method": "turn/started", "params": map[string]any{"threadId": "thread-test", "turn": map[string]any{"id": "turn-test", "status": "inProgress"}}})
				_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"id": "message-early", "type": "agentMessage", "text": "EARLY"}}})
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": "turn-test", "status": "completed"}}})
				_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"turn": map[string]any{"id": "turn-test", "status": "completed"}}})
				return
			}
			if os.Getenv("GO_WANT_RUDDR_TURN_RESPONSE_NO_ID") == "1" {
				_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"turn": map[string]any{"status": "inProgress"}}})
				return
			}
			if os.Getenv("GO_WANT_RUDDR_EARLY_ITEM_BEFORE_TURN_RESPONSE") == "1" {
				_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"id": "message-live", "type": "agentMessage", "text": "LIVE BEFORE RESPONSE"}}})
			}
			if delay := os.Getenv("GO_WANT_RUDDR_TURN_RESPONSE_DELAY"); delay != "" {
				if duration, err := time.ParseDuration(delay); err == nil {
					time.Sleep(duration)
				}
			}
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"turn": map[string]any{"id": "turn-test", "status": "inProgress"}}})
			_ = enc.Encode(map[string]any{"method": "turn/started", "params": map[string]any{"threadId": "thread-test", "turn": map[string]any{"id": "turn-test", "status": "inProgress"}}})
			if os.Getenv("GO_WANT_RUDDR_NESTED_TURN") == "1" {
				_ = enc.Encode(map[string]any{"method": "turn/started", "params": map[string]any{"threadId": "thread-child", "turn": map[string]any{"id": "turn-child", "status": "inProgress"}}})
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-child", "turn": map[string]any{"id": "turn-child", "status": "completed"}}})
				_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"threadId": "thread-test", "turnId": "turn-test", "item": map[string]any{"id": "message-root", "type": "agentMessage", "text": "ROOT DONE"}}})
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-test", "turn": map[string]any{"id": "turn-test", "status": "completed"}}})
				continue
			}
			if os.Getenv("GO_WANT_RUDDR_MULTI_OUTPUT_ERROR") == "1" {
				_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"id": "message-first", "type": "agentMessage", "text": "FIRST"}}})
				_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"id": "message-second", "type": "agentMessage", "text": "SECOND"}}})
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": "turn-test", "status": "failed", "error": map[string]any{"code": 99, "message": "SECRET_ECHO from prompt"}}}})
				continue
			}
			if os.Getenv("GO_WANT_RUDDR_COMPLETE_ON_START") == "1" {
				_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"id": "message-test", "type": "agentMessage", "text": "DONE"}}})
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": "turn-test", "status": "completed"}}})
			}
		case "turn/steer":
			expectedTurn := "turn-test"
			if os.Getenv("GO_WANT_RUDDR_MULTI_TURN") == "1" {
				expectedTurn = currentTurn
			}
			if request.Params["threadId"] != "thread-test" || request.Params["expectedTurnId"] != expectedTurn {
				_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32600, "message": "wrong turn"}})
				continue
			}
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{"turnId": expectedTurn}})
			_ = enc.Encode(map[string]any{"method": "item/completed", "params": map[string]any{"item": map[string]any{"id": "message-test", "type": "agentMessage", "text": "STEERED"}}})
			_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": expectedTurn, "status": "completed"}}})
		case "turn/interrupt":
			if request.Params["threadId"] != "thread-test" || request.Params["turnId"] != currentTurn {
				_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32602, "message": "interrupt targeted the wrong turn"}})
				continue
			}
			if os.Getenv("GO_WANT_RUDDR_DEFER_INTERRUPT_ERROR") == "1" {
				deferredInterruptID = request.ID
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": currentTurn, "status": "completed"}}})
				continue
			}
			if os.Getenv("GO_WANT_RUDDR_INTERRUPT_COMPLETE_FIRST") == "1" {
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": currentTurn, "status": "interrupted"}}})
			}
			_ = enc.Encode(map[string]any{"id": request.ID, "result": map[string]any{}})
			if os.Getenv("GO_WANT_RUDDR_EXIT_AFTER_INTERRUPT_ACK") == "1" {
				return
			}
			if os.Getenv("GO_WANT_RUDDR_INTERRUPT_ACK_ONLY") == "1" {
				continue
			}
			if os.Getenv("GO_WANT_RUDDR_INTERRUPT_COMPLETE_FIRST") != "1" {
				if delay := os.Getenv("GO_WANT_RUDDR_INTERRUPT_COMPLETION_DELAY"); delay != "" {
					if duration, err := time.ParseDuration(delay); err == nil {
						time.Sleep(duration)
					}
				}
				_ = enc.Encode(map[string]any{"method": "turn/completed", "params": map[string]any{"turn": map[string]any{"id": currentTurn, "status": "interrupted"}}})
			}
		default:
			if request.ID != "" {
				_ = enc.Encode(map[string]any{"id": request.ID, "error": map[string]any{"code": -32601, "message": fmt.Sprintf("unsupported %s", request.Method)}})
			}
		}
	}
	if marker := os.Getenv("GO_WANT_RUDDR_EOF_MARKER"); marker != "" {
		_ = os.WriteFile(marker, []byte("stdin closed\n"), 0o600)
	}
}

func startIdleHelperRun(t *testing.T, extraEnv map[string]string, cfgMutate func(*runConfig)) (string, chan error) {
	return startIdleHelperRunContext(t, context.Background(), extraEnv, cfgMutate)
}

func startIdleHelperRunContext(t *testing.T, ctx context.Context, extraEnv map[string]string, cfgMutate func(*runConfig)) (string, chan error) {
	t.Helper()
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("FIRST SECRET TASK"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_MULTI_TURN", "1")
	for key, value := range extraEnv {
		t.Setenv(key, value)
	}
	cfg := runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		Idle:           true,
		IdleTimeout:    time.Minute,
		ChildCommand:   []string{os.Args[0], "-test.run=TestIdlePromptStartsSecondTurn"},
	}
	if cfgMutate != nil {
		cfgMutate(&cfg)
	}
	errCh := make(chan error, 1)
	go func() { errCh <- runControllerContext(ctx, cfg) }()
	return stateDir, errCh
}

func TestIdlePromptStartsSecondTurn(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, nil, nil)
	waitForRunStatus(t, stateDir, "idle")
	response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: "SECOND SECRET TASK"}, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("prompt failed: %s", response.Error)
	}
	state := waitForRunStatus(t, stateDir, "idle")
	if state.Turns != 2 {
		t.Fatalf("turns = %d, want 2", state.Turns)
	}
	if state.TokenUsage == nil || state.TokenUsage.TotalTokens != 200 || state.TokenUsage.ContextWindow != 1000 {
		t.Fatalf("token usage not persisted: %#v", state.TokenUsage)
	}
	raw, err := os.ReadFile(filepath.Join(stateDir, stateFileName))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(raw), "SECRET TASK") {
		t.Fatal("state file persisted prompt text")
	}
	output, err := os.ReadFile(filepath.Join(stateDir, "output.md"))
	if err != nil {
		t.Fatal(err)
	}
	if want := "TURN 1\n\n---\n\nTURN 2"; !strings.Contains(string(output), want) {
		t.Fatalf("output = %q, want to contain %q", output, want)
	}
	events, err := os.ReadFile(filepath.Join(stateDir, "events.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(events), `"userMessage"`) || !strings.Contains(string(events), "SECOND SECRET TASK") {
		t.Fatal("events.jsonl is missing the synthetic userMessage items")
	}
	stopResponse, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !stopResponse.OK {
		t.Fatalf("shutdown failed: %s", stopResponse.Error)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
	final, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if final.Status != "completed" || final.CompletedAt.IsZero() {
		t.Fatalf("unexpected final state: %#v", final)
	}
}

func TestSteerRejectedWhileIdleAndPromptNeverConverts(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, nil, nil)
	waitForRunStatus(t, stateDir, "idle")
	response, err := sendControl(stateDir, controlRequest{Command: "steer", Text: "nope"}, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK || !strings.Contains(response.Error, "not steerable") {
		t.Fatalf("steer while idle = %#v", response)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Turns != 1 {
		t.Fatalf("rejected steer started a turn: %#v", state)
	}
	if _, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
}

func TestPromptRejectedWhileActiveAndInterruptReturnsToIdle(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{
		"GO_WANT_RUDDR_MULTI_TURN_HOLD":          "1",
		"GO_WANT_RUDDR_INTERRUPT_COMPLETE_FIRST": "1",
	}, nil)
	active := waitForRunStatus(t, stateDir, "active")
	childPID := active.ChildPID
	response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: "too early"}, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK || !strings.Contains(response.Error, "steer it instead") {
		t.Fatalf("prompt while active = %#v", response)
	}
	if response, err = sendControl(stateDir, controlRequest{Command: "interrupt"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("interrupt failed: %s", response.Error)
	}
	waitForRunStatus(t, stateDir, "idle")
	if !processAlive(childPID) {
		t.Fatal("idle-mode interrupt killed the provider child")
	}
	if response, err = sendControl(stateDir, controlRequest{Command: "prompt", Text: "after interrupt"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("prompt after interrupt failed: %s", response.Error)
	}
	state := waitForRunStatus(t, stateDir, "active")
	if state.Turns != 2 {
		t.Fatalf("turns = %d, want 2", state.Turns)
	}
	if response, err = sendControl(stateDir, controlRequest{Command: "interrupt"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("second interrupt failed: %s", response.Error)
	}
	waitForRunStatus(t, stateDir, "idle")
	if _, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if err := <-errCh; err == nil {
		t.Fatal("run with an interrupted final turn unexpectedly returned nil")
	}
	final, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if final.Status != "interrupted" {
		t.Fatalf("final status = %q, want interrupted", final.Status)
	}
}

func TestAcceptedPromptPrecedesEarlyProviderOutput(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("EARLY PROMPT"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_COMPLETE_BEFORE_TURN_RESPONSE", "1")
	if err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		ChildCommand:   []string{os.Args[0], "-test.run=TestAcceptedPromptPrecedesEarlyProviderOutput"},
	}); err != nil {
		t.Fatal(err)
	}
	events, err := os.ReadFile(filepath.Join(stateDir, "events.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	promptIndex := bytes.Index(events, []byte(`"origin":"ruddr"`))
	outputIndex := bytes.Index(events, []byte(`"text":"EARLY"`))
	if promptIndex < 0 || outputIndex < 0 || promptIndex >= outputIndex {
		t.Fatalf("accepted prompt did not precede provider output:\n%s", events)
	}
	output, err := os.ReadFile(filepath.Join(stateDir, "output.md"))
	if err != nil {
		t.Fatal(err)
	}
	if string(output) != "EARLY\n" {
		t.Fatalf("fast provider output = %q", output)
	}
}

func TestProviderEventsRemainVisibleWhileTurnStartIsPending(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("VISIBLE PROMPT"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_EARLY_ITEM_BEFORE_TURN_RESPONSE", "1")
	t.Setenv("GO_WANT_RUDDR_TURN_RESPONSE_DELAY", "500ms")
	t.Setenv("GO_WANT_RUDDR_COMPLETE_ON_START", "1")
	errCh := make(chan error, 1)
	go func() {
		errCh <- runController(runConfig{
			CWD:            dir,
			PromptFile:     promptPath,
			StateDir:       stateDir,
			Model:          "test-model",
			Sandbox:        "read-only",
			ApprovalPolicy: "never",
			ChildCommand:   []string{os.Args[0], "-test.run=TestProviderEventsRemainVisibleWhileTurnStartIsPending"},
		})
	}()
	deadline := time.Now().Add(2 * time.Second)
	for {
		events, err := os.ReadFile(filepath.Join(stateDir, "events.jsonl"))
		if err == nil &&
			bytes.Contains(events, []byte(`"origin":"ruddr"`)) &&
			bytes.Contains(events, []byte(`"text":"LIVE BEFORE RESPONSE"`)) {
			state, stateErr := readState(stateDir)
			if stateErr != nil {
				t.Fatal(stateErr)
			}
			if state.Status != "starting" {
				t.Fatalf("provider event became visible only after turn/start resolved: status=%s", state.Status)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("start-time provider activity was not visible before the response")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
}

func TestTurnStartWithoutIDRecordsUnknownPromptOutcome(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	promptPath := filepath.Join(dir, "prompt.md")
	stateDir := filepath.Join(dir, "run")
	if err := os.WriteFile(promptPath, []byte("UNKNOWN PROMPT"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_TURN_RESPONSE_NO_ID", "1")
	err := runController(runConfig{
		CWD:            dir,
		PromptFile:     promptPath,
		StateDir:       stateDir,
		Model:          "test-model",
		Sandbox:        "read-only",
		ApprovalPolicy: "never",
		ChildCommand:   []string{os.Args[0], "-test.run=TestTurnStartWithoutIDRecordsUnknownPromptOutcome"},
	})
	if err == nil || !strings.Contains(err.Error(), "no turn id") {
		t.Fatalf("missing turn id error = %v", err)
	}
	events, readErr := os.ReadFile(filepath.Join(stateDir, "events.jsonl"))
	if readErr != nil {
		t.Fatal(readErr)
	}
	if !bytes.Contains(events, []byte(`"method":"ruddr/prompt/unknown"`)) ||
		bytes.Contains(events, []byte(`"method":"ruddr/prompt/accepted"`)) {
		t.Fatalf("missing-ID prompt decision is wrong:\n%s", events)
	}
}

func TestIdleInterruptWaitsForProviderCompletion(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{
		"GO_WANT_RUDDR_MULTI_TURN_HOLD":            "1",
		"GO_WANT_RUDDR_INTERRUPT_COMPLETION_DELAY": "250ms",
	}, nil)
	waitForRunStatus(t, stateDir, "active")
	type controlResult struct {
		response controlResponse
		err      error
	}
	resultCh := make(chan controlResult, 1)
	go func() {
		response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, 2*time.Second)
		resultCh <- controlResult{response: response, err: err}
	}()
	select {
	case result := <-resultCh:
		t.Fatalf("interrupt returned before provider completion: response=%#v err=%v", result.response, result.err)
	case <-time.After(50 * time.Millisecond):
	}
	if state, err := readState(stateDir); err != nil {
		t.Fatal(err)
	} else if state.Status != "active" {
		t.Fatalf("status before provider completion = %q, want active", state.Status)
	}
	result := <-resultCh
	if result.err != nil || !result.response.OK {
		t.Fatalf("interrupt response = %#v, err=%v", result.response, result.err)
	}
	waitForRunStatus(t, stateDir, "idle")
	if _, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 2*time.Second); err != nil {
		t.Fatal(err)
	}
	if err := <-errCh; err == nil || !strings.Contains(err.Error(), "interrupted") {
		t.Fatalf("final run error = %v", err)
	}
}

func TestIdleInterruptSettlementTimeoutFailsSession(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{
		"GO_WANT_RUDDR_MULTI_TURN_HOLD":    "1",
		"GO_WANT_RUDDR_INTERRUPT_ACK_ONLY": "1",
	}, func(cfg *runConfig) {
		cfg.InterruptTimeout = 50 * time.Millisecond
	})
	waitForRunStatus(t, stateDir, "active")
	response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK || !strings.Contains(response.Error, "did not settle") {
		t.Fatalf("interrupt timeout response = %#v", response)
	}
	if err := <-errCh; err == nil || !strings.Contains(err.Error(), "did not settle") {
		t.Fatalf("run error = %v", err)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Status != "failed" || processAlive(state.ChildPID) {
		t.Fatalf("interrupt timeout left bad state: %#v", state)
	}
}

func TestIdleInterruptReportsProviderExitAfterAcknowledgement(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{
		"GO_WANT_RUDDR_MULTI_TURN_HOLD":          "1",
		"GO_WANT_RUDDR_EXIT_AFTER_INTERRUPT_ACK": "1",
	}, nil)
	waitForRunStatus(t, stateDir, "active")
	response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK || !strings.Contains(response.Error, "provider output closed") {
		t.Fatalf("interrupt provider-exit response = %#v", response)
	}
	if err := <-errCh; err == nil || !strings.Contains(err.Error(), "provider output closed") {
		t.Fatalf("run error = %v", err)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Status != "failed" {
		t.Fatalf("provider exit status = %q, want failed", state.Status)
	}
}

func TestAmbiguousSecondTurnStartFailsSession(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{
		"GO_WANT_RUDDR_AMBIGUOUS_SECOND_TURN": "1",
	}, func(cfg *runConfig) {
		cfg.IdleTurnStartTimeout = 50 * time.Millisecond
	})
	waitForRunStatus(t, stateDir, "idle")
	response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: "ambiguous turn"}, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK {
		t.Fatalf("ambiguous prompt succeeded: %#v", response)
	}
	if err := <-errCh; err == nil || !strings.Contains(err.Error(), "ambiguous") {
		t.Fatalf("run error = %v", err)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Status != "failed" || processAlive(state.ChildPID) {
		t.Fatalf("ambiguous start left bad state: %#v", state)
	}
}

func TestRejectedSecondTurnRestoresIdle(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{
		"GO_WANT_RUDDR_REJECT_SECOND_TURN": "1",
	}, nil)
	waitForRunStatus(t, stateDir, "idle")
	response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: "rejected turn"}, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK || !strings.Contains(response.Error, "second turn rejected") {
		t.Fatalf("rejected prompt response = %#v", response)
	}
	waitForRunStatus(t, stateDir, "idle")
	response, err = sendControl(stateDir, controlRequest{Command: "prompt", Text: "accepted turn"}, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !response.OK {
		t.Fatalf("prompt after rejection failed: %#v", response)
	}
	state := waitForRunStatus(t, stateDir, "idle")
	if state.Turns != 2 {
		t.Fatalf("turns after rejection and retry = %d, want 2", state.Turns)
	}
	output, err := os.ReadFile(filepath.Join(stateDir, "output.md"))
	if err != nil {
		t.Fatal(err)
	}
	if string(output) != "TURN 1\n\n---\n\nTURN 2\n" {
		t.Fatalf("output after rejection and retry = %q", output)
	}
	events, err := os.ReadFile(filepath.Join(stateDir, "events.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if got := bytes.Count(events, []byte(`"origin":"ruddr"`)); got != 3 {
		t.Fatalf("prompt attempt count = %d, want 3", got)
	}
	if got := bytes.Count(events, []byte(`"method":"ruddr/prompt/accepted"`)); got != 2 {
		t.Fatalf("accepted prompt decision count = %d, want 2", got)
	}
	if got := bytes.Count(events, []byte(`"method":"ruddr/prompt/rejected"`)); got != 1 {
		t.Fatalf("rejected prompt decision count = %d, want 1", got)
	}
	if _, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 2*time.Second); err != nil {
		t.Fatal(err)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
}

func TestThreadCommandCancellationTerminatesProcessTree(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	if runtime.GOOS == "windows" {
		t.Skip("process-group assertion requires Unix")
	}
	dir := t.TempDir()
	pidFile := filepath.Join(dir, "grandchild.pid")
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_IGNORE_INITIALIZE", "1")
	t.Setenv("GO_WANT_RUDDR_GRANDCHILD_PID_FILE", pidFile)
	t.Setenv("GO_WANT_RUDDR_TERM_IGNORING_GRANDCHILD", "1")
	ctx, cancel := context.WithCancel(context.Background())
	session, err := startAppServerSession(ctx, dir, []string{os.Args[0], "-test.run=TestThreadCommandCancellationTerminatesProcessTree"})
	if err != nil {
		t.Fatal(err)
	}
	initErrCh := make(chan error, 1)
	go func() { initErrCh <- session.initialize() }()
	deadline := time.Now().Add(2 * time.Second)
	var grandchildPID int
	for time.Now().Before(deadline) {
		raw, readErr := os.ReadFile(pidFile)
		if readErr == nil {
			parsedPID, parseErr := strconv.Atoi(strings.TrimSpace(string(raw)))
			if parseErr == nil && parsedPID > 0 {
				grandchildPID = parsedPID
				break
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	if grandchildPID == 0 {
		t.Fatal("helper did not record its grandchild pid")
	}
	t.Cleanup(func() {
		if processAlive(grandchildPID) {
			if process, findErr := os.FindProcess(grandchildPID); findErr == nil {
				_ = process.Kill()
			}
		}
	})
	cancel()
	select {
	case err := <-initErrCh:
		if err == nil {
			t.Fatal("canceled initialize returned nil")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("canceled initialize did not return")
	}
	session.close()
	for processAlive(grandchildPID) && time.Now().Before(deadline.Add(5*time.Second)) {
		time.Sleep(10 * time.Millisecond)
	}
	if processAlive(grandchildPID) {
		t.Fatalf("thread command grandchild pid %d is still alive", grandchildPID)
	}
}

func TestThreadCommandCloseAllowsGracefulEOF(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	dir := t.TempDir()
	marker := filepath.Join(dir, "eof.marker")
	t.Setenv("GO_WANT_RUDDR_HELPER", "1")
	t.Setenv("GO_WANT_RUDDR_EOF_MARKER", marker)
	session, err := startAppServerSession(context.Background(), dir, []string{os.Args[0], "-test.run=TestThreadCommandCloseAllowsGracefulEOF"})
	if err != nil {
		t.Fatal(err)
	}
	if err := session.initialize(); err != nil {
		t.Fatal(err)
	}
	session.close()
	if raw, err := os.ReadFile(marker); err != nil {
		t.Fatalf("helper did not finish its EOF cleanup: %v", err)
	} else if string(raw) != "stdin closed\n" {
		t.Fatalf("EOF marker = %q", raw)
	}
}

func TestSecondTurnSteerCarriesCurrentThreadAndTurn(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{"GO_WANT_RUDDR_MULTI_TURN_HOLD": "1"}, nil)
	waitForRunStatus(t, stateDir, "active")
	if response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, 5*time.Second); err != nil || !response.OK {
		t.Fatalf("interrupt response = %#v, err=%v", response, err)
	}
	waitForRunStatus(t, stateDir, "idle")
	if response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: "second turn"}, 5*time.Second); err != nil || !response.OK {
		t.Fatalf("prompt response = %#v, err=%v", response, err)
	}
	state := waitForRunStatus(t, stateDir, "active")
	if state.TurnID != "turn-2" {
		t.Fatalf("turn id = %q, want turn-2", state.TurnID)
	}
	response, err := sendControl(stateDir, controlRequest{
		Command:        "steer",
		Text:           "stale direction",
		ExpectedTurnID: "turn-1",
	}, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK || !strings.Contains(response.Error, "active turn changed") {
		t.Fatalf("stale-turn steer response = %#v", response)
	}
	if state = waitForRunStatus(t, stateDir, "active"); state.Steers != 0 {
		t.Fatalf("stale-turn steer changed state: %#v", state)
	}
	if response, err := sendControl(stateDir, controlRequest{Command: "steer", Text: "new direction", ExpectedTurnID: "turn-2"}, 5*time.Second); err != nil || !response.OK {
		t.Fatalf("steer response = %#v, err=%v", response, err)
	}
	waitForRunStatus(t, stateDir, "idle")
	if _, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
}

func TestPromptRejectedWithoutIdleFlag(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{"GO_WANT_RUDDR_MULTI_TURN_HOLD": "1"}, func(cfg *runConfig) {
		cfg.Idle = false
	})
	waitForRunStatus(t, stateDir, "active")
	response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: "nope"}, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if response.OK || !strings.Contains(response.Error, "--idle") {
		t.Fatalf("prompt without idle flag = %#v", response)
	}
	if _, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	<-errCh
}

func TestIdleTimeoutExitsWithLastStatus(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, nil, func(cfg *runConfig) {
		cfg.IdleTimeout = 200 * time.Millisecond
	})
	select {
	case err := <-errCh:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("idle timeout did not end the run")
	}
	final, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if final.Status != "completed" || final.CompletedAt.IsZero() {
		t.Fatalf("unexpected final state: %#v", final)
	}
}

func TestChildExitWhileIdleFailsSession(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{"GO_WANT_RUDDR_EXIT_AFTER_TURN": "1"}, nil)
	select {
	case err := <-errCh:
		if err == nil {
			t.Fatal("run unexpectedly succeeded after the provider died while idle")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("provider death while idle did not end the run")
	}
	final, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if final.Status != "failed" {
		t.Fatalf("final status = %q, want failed", final.Status)
	}
}

func TestIdleContextCancellationPersistsInterrupted(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	ctx, cancel := context.WithCancel(context.Background())
	stateDir, errCh := startIdleHelperRunContext(t, ctx, nil, nil)
	waitForRunStatus(t, stateDir, "idle")
	cancel()
	if err := <-errCh; err == nil || !strings.Contains(err.Error(), "interrupted") {
		t.Fatalf("unexpected cancellation result: %v", err)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Status != "interrupted" {
		t.Fatalf("status = %q, want interrupted", state.Status)
	}
}

func TestConcurrentIdlePromptsAcceptOneGeneration(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, map[string]string{"GO_WANT_RUDDR_DELAY_TURN_START": "1"}, nil)
	waitForRunStatus(t, stateDir, "idle")
	type result struct {
		response controlResponse
		err      error
	}
	start := make(chan struct{})
	results := make(chan result, 2)
	for _, text := range []string{"second-a", "second-b"} {
		go func(message string) {
			<-start
			response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: message}, 5*time.Second)
			results <- result{response: response, err: err}
		}(text)
	}
	close(start)
	accepted := 0
	for range 2 {
		result := <-results
		if result.err != nil {
			t.Fatal(result.err)
		}
		if result.response.OK {
			accepted++
		}
	}
	if accepted != 1 {
		t.Fatalf("accepted prompts = %d, want 1", accepted)
	}
	state := waitForRunStatus(t, stateDir, "idle")
	if state.Turns != 2 {
		t.Fatalf("turns = %d, want 2", state.Turns)
	}
	if _, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
}

func TestShutdownRejectsLaterPrompt(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_HELPER") == "1" {
		runHelperAppServer()
		os.Exit(0)
	}
	stateDir, errCh := startIdleHelperRun(t, nil, nil)
	waitForRunStatus(t, stateDir, "idle")
	response, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, 5*time.Second)
	if err != nil || !response.OK {
		t.Fatalf("shutdown response = %#v, err=%v", response, err)
	}
	if response, promptErr := sendControl(stateDir, controlRequest{Command: "prompt", Text: "too late"}, 5*time.Second); promptErr == nil && response.OK {
		t.Fatalf("prompt was accepted after shutdown: %#v", response)
	}
	if err := <-errCh; err != nil {
		t.Fatal(err)
	}
	state, err := readState(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	if state.Turns != 1 || state.Status != "completed" {
		t.Fatalf("unexpected final state: %#v", state)
	}
}

func TestSessionEndingStatusOverridesLastTurn(t *testing.T) {
	dir := t.TempDir()
	store, err := newStateStore(runConfig{
		Provider: providerCodex,
		CWD:      dir,
		StateDir: filepath.Join(dir, "run"),
		Model:    "test-model",
		Sandbox:  "read-only",
		Idle:     true,
	})
	if err != nil {
		t.Fatal(err)
	}
	r := &controller{store: store, turnDone: make(chan struct{}), sessionDone: make(chan struct{})}
	if !r.finishTurn("completed", "") {
		t.Fatal("turn did not finish")
	}
	r.endSession("interrupted", "")
	if state := store.snapshot(); state.Status != "interrupted" {
		t.Fatalf("status = %q, want interrupted", state.Status)
	}
}
