package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"
)

// TestDetachedRunHelper stands in for the background controller. It writes the
// state named by GO_WANT_RUDDR_DETACH_STATUS and records the args it was given.
func TestDetachedRunHelper(t *testing.T) {
	status := os.Getenv("GO_WANT_RUDDR_DETACH_STATUS")
	if status == "" {
		return
	}
	stateDir := os.Getenv("GO_WANT_RUDDR_DETACH_DIR")
	_ = os.WriteFile(filepath.Join(stateDir, "helper.args"), []byte(strings.Join(os.Args, "\n")), 0o600)
	if status == "crash" {
		os.Stderr.WriteString("provider binary not found\n")
		os.Exit(2)
	}
	raw, _ := marshalState(runState{Version: 2, PID: os.Getpid(), Status: status, StateDir: stateDir})
	_ = os.WriteFile(filepath.Join(stateDir, stateFileName), raw, 0o600)
	os.Exit(0)
}

func useDetachHelper(t *testing.T, status string) {
	t.Helper()
	previous := detachedRunCommand
	t.Cleanup(func() { detachedRunCommand = previous })
	detachedRunCommand = func(args []string) (*exec.Cmd, error) {
		cmd := exec.Command(os.Args[0], append([]string{"-test.run=^TestDetachedRunHelper$", "--"}, args...)...)
		return cmd, nil
	}
	t.Setenv("GO_WANT_RUDDR_DETACH_STATUS", status)
}

func TestStartDetachedRunReturnsOnceControllerIsLive(t *testing.T) {
	stateDir := t.TempDir()
	useDetachHelper(t, "active")
	t.Setenv("GO_WANT_RUDDR_DETACH_DIR", stateDir)
	state, err := startDetachedRun(stateDir, []string{"--state-dir", stateDir}, 10*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if state.Status != "active" || state.PID <= 0 {
		t.Fatalf("state = %+v", state)
	}
	info, err := os.Stat(filepath.Join(stateDir, launchStderrFileName))
	if err != nil {
		t.Fatal(err)
	}
	if mode := info.Mode().Perm(); mode != 0o600 && os.PathSeparator == '/' {
		t.Fatalf("launch log mode = %o, want 600", mode)
	}
}

func TestStartDetachedRunReportsStartupCrashFromStderr(t *testing.T) {
	stateDir := t.TempDir()
	useDetachHelper(t, "crash")
	t.Setenv("GO_WANT_RUDDR_DETACH_DIR", stateDir)
	_, err := startDetachedRun(stateDir, nil, 10*time.Second)
	if err == nil || !strings.Contains(err.Error(), "provider binary not found") {
		t.Fatalf("err = %v, want the child's stderr", err)
	}
}

func TestWaitForDetachedStartupLeavesSlowStartsRunning(t *testing.T) {
	stateDir := t.TempDir()
	state, err := waitForDetachedStartup(stateDir, filepath.Join(stateDir, launchStderrFileName), make(chan struct{}), time.Now(), time.Millisecond)
	if err != nil || state.Status != "starting" {
		t.Fatalf("state = %+v err = %v, want a still-starting run without error", state, err)
	}
}

func TestDetachedChildArgsDropDetachAndUseStoredPrompt(t *testing.T) {
	got := detachedChildArgs([]string{
		"--detach", "--provider=claude", "--prompt-file", "-", "-detach=true",
		"--state-dir", "x", "--", "codex", "--detach",
	}, "/abs/x/prompt.md")
	want := []string{"--provider=claude", "--prompt-file", "/abs/x/prompt.md", "--state-dir", "x", "--", "codex", "--detach"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("args = %q, want %q", got, want)
	}
}

func TestWriteStdinPromptIsPrivateAndRefusesReuse(t *testing.T) {
	stateDir := filepath.Join(t.TempDir(), "run")
	path, err := writeStdinPrompt(stateDir, strings.NewReader("do the thing\n"))
	if err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(path)
	if err != nil || string(raw) != "do the thing\n" {
		t.Fatalf("prompt = %q, %v", raw, err)
	}
	if os.PathSeparator == '/' {
		info, _ := os.Stat(path)
		dirInfo, _ := os.Stat(stateDir)
		if info.Mode().Perm() != 0o600 || dirInfo.Mode().Perm() != 0o700 {
			t.Fatalf("modes = %o %o", info.Mode().Perm(), dirInfo.Mode().Perm())
		}
	}
	if _, err := writeStdinPrompt(stateDir, strings.NewReader("again")); err == nil {
		t.Fatal("second stdin prompt into the same state dir succeeded")
	}
	if _, err := writeStdinPrompt(filepath.Join(t.TempDir(), "empty"), strings.NewReader("  \n")); err == nil {
		t.Fatal("empty stdin prompt was accepted")
	}
}
