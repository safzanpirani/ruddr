package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func writeWaitState(t *testing.T, stateDir string, state runState) {
	t.Helper()
	raw, err := marshalState(state)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(stateDir, stateFileName), raw, 0o600); err != nil {
		t.Fatal(err)
	}
}

// The controller persists its terminal state before exiting, so a wait that
// observes a live-looking state and then a dead pid must re-read rather than
// report a completed run as stale.
func TestWaitRereadsStateWhenControllerExitsAfterCompleting(t *testing.T) {
	stateDir := t.TempDir()
	writeWaitState(t, stateDir, runState{Version: 2, PID: 4242, Status: "active"})

	alive := func(int) bool {
		// Simulate the run finishing between the state read and the probe.
		writeWaitState(t, stateDir, runState{Version: 2, PID: 4242, Status: "completed"})
		return false
	}
	if err := waitForTerminalState(stateDir, time.Time{}, alive, time.Millisecond); err != nil {
		t.Fatalf("wait reported %v; want the persisted completed status", err)
	}
}

func TestWaitReportsStaleWhenNoTerminalStateWasPersisted(t *testing.T) {
	stateDir := t.TempDir()
	writeWaitState(t, stateDir, runState{Version: 2, PID: 4242, Status: "active"})

	err := waitForTerminalState(stateDir, time.Time{}, func(int) bool { return false }, time.Millisecond)
	if err == nil {
		t.Fatal("wait unexpectedly succeeded")
	}
	if !strings.Contains(err.Error(), "stale") {
		t.Fatalf("wait error %q does not report stale state", err)
	}
}

func TestWaitReportsFailedTerminalStateAfterControllerExit(t *testing.T) {
	stateDir := t.TempDir()
	writeWaitState(t, stateDir, runState{Version: 2, PID: 4242, Status: "active"})

	alive := func(int) bool {
		writeWaitState(t, stateDir, runState{Version: 2, PID: 4242, Status: "failed", Error: "turn failed; see trace.log"})
		return false
	}
	err := waitForTerminalState(stateDir, time.Time{}, alive, time.Millisecond)
	if err == nil {
		t.Fatal("wait unexpectedly succeeded")
	}
	if !strings.Contains(err.Error(), "turn failed") {
		t.Fatalf("wait error %q does not surface the persisted failure", err)
	}
}
