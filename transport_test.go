package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

type bufferWriteCloser struct{ bytes.Buffer }

func (*bufferWriteCloser) Close() error { return nil }

func TestDuplicateRPCResponsesDoNotBlockReader(t *testing.T) {
	store, err := newStateStore(runConfig{StateDir: filepath.Join(t.TempDir(), "run")})
	if err != nil {
		t.Fatal(err)
	}
	responseCh := make(chan rpcEnvelope, 1)
	r := &controller{
		store: store, pending: map[string]chan rpcEnvelope{"ruddr-1": responseCh},
		turnDone: make(chan struct{}), sessionDone: make(chan struct{}), readDone: make(chan struct{}),
	}
	defer r.closeControlServer()
	if err := r.openLogs(); err != nil {
		t.Fatal(err)
	}
	defer r.closeLogs()
	input := strings.Repeat("{\"id\":\"ruddr-1\",\"result\":{}}\n", 3) +
		"{\"method\":\"item/completed\",\"params\":{\"item\":{\"type\":\"agentMessage\",\"text\":\"AFTER DUPLICATES\"}}}\n" +
		"{\"method\":\"turn/completed\",\"params\":{\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\"}}}\n"
	go r.readChild(strings.NewReader(input))
	select {
	case <-r.readDone:
	case <-time.After(time.Second):
		// Release the old reader on failure so the regression leaves no goroutine behind.
		for i := 0; i < 3; i++ {
			<-responseCh
		}
		<-r.readDone
		t.Fatal("duplicate responses blocked later provider output and completion")
	}
	state, err := readState(store.snapshot().StateDir)
	if err != nil || state.Status != "completed" {
		t.Fatalf("final state = %+v, %v", state, err)
	}
	output, err := os.ReadFile(state.OutputPath)
	if err != nil || string(output) != "AFTER DUPLICATES\n" {
		t.Fatalf("output = %q, %v", output, err)
	}
}

func TestInteractiveRequestRejectionPreservesID(t *testing.T) {
	for _, id := range []string{`"approval-1"`, `9007199254740993`, `9223372036854775807`} {
		for _, mode := range []string{"controller", "thread"} {
			t.Run(mode+"/"+id, func(t *testing.T) {
				writer := &bufferWriteCloser{}
				request := rpcEnvelope{ID: json.RawMessage(id), Method: "item/commandExecution/requestApproval"}
				if mode == "controller" {
					r := &controller{childIn: writer}
					r.rejectServerRequest(request)
				} else {
					raw, err := json.Marshal(request)
					if err != nil {
						t.Fatal(err)
					}
					s := &appServerSession{stdin: writer, scanner: bufio.NewScanner(strings.NewReader(string(raw) + "\n{\"id\":\"ruddr-query-1\",\"result\":{}}\n"))}
					if err := s.call("thread/list", map[string]any{}, nil); err != nil {
						t.Fatal(err)
					}
				}
				lines := strings.Split(strings.TrimSpace(writer.String()), "\n")
				var response rpcEnvelope
				if err := json.Unmarshal([]byte(lines[len(lines)-1]), &response); err != nil {
					t.Fatal(err)
				}
				if string(response.ID) != id || response.Error == nil || response.Error.Code != -32601 {
					t.Fatalf("rejection = %+v, want exact ID %s and -32601", response, id)
				}
			})
		}
	}
}

func TestDelayedInterruptErrorDoesNotStopNewTurn(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stateDir, errCh := startIdleHelperRunContext(t, ctx, map[string]string{
		"GO_WANT_RUDDR_MULTI_TURN_HOLD":       "1",
		"GO_WANT_RUDDR_DEFER_INTERRUPT_ERROR": "1",
	}, nil)
	t.Cleanup(func() {
		cancel()
		select {
		case err := <-errCh:
			if err == nil {
				t.Error("canceled controller reported success")
			}
			state, readErr := readState(stateDir)
			if readErr != nil || state.Status != "interrupted" || processAlive(state.ChildPID) {
				t.Errorf("cancellation left bad state: %+v, %v", state, readErr)
			}
			if _, err := os.Lstat(state.SocketPath); !os.IsNotExist(err) {
				t.Errorf("cancellation left the control socket: %v", err)
			}
		case <-time.After(5 * time.Second):
			t.Error("controller did not exit after cancellation")
		}
	})
	waitForRunStatus(t, stateDir, "active")
	interruptResult := make(chan controlResponse, 1)
	go func() {
		response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, 5*time.Second)
		if err != nil {
			response.Error = err.Error()
		}
		interruptResult <- response
	}()
	waitForRunStatus(t, stateDir, "idle")
	response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: "second turn"}, 5*time.Second)
	if err != nil || !response.OK {
		t.Fatalf("new prompt = %+v, %v", response, err)
	}
	select {
	case response := <-interruptResult:
		if response.OK || !strings.Contains(response.Error, "old turn already completed") {
			t.Fatalf("old interrupt = %+v", response)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("old interrupt did not return")
	}
	state, err := readState(stateDir)
	if err != nil || state.Status != "active" || state.TurnID != "turn-2" || !processAlive(state.ChildPID) {
		t.Fatalf("new turn was stopped by the old interrupt: %+v, %v", state, err)
	}
}

func TestInterruptTimeoutCannotClaimLaterTurn(t *testing.T) {
	store, err := newStateStore(runConfig{StateDir: filepath.Join(t.TempDir(), "run"), Idle: true})
	if err != nil {
		t.Fatal(err)
	}
	oldTurn := make(chan struct{})
	close(oldTurn)
	r := &controller{store: store, turnDone: make(chan struct{}), sessionDone: make(chan struct{})}
	defer r.closeControlServer()
	if err := store.update(func(state *runState) { state.Status = "active"; state.TurnID = "turn-2" }); err != nil {
		t.Fatal(err)
	}
	if r.endSessionIfTurnOpen(oldTurn, "failed", "old interrupt timeout") {
		t.Fatal("old interrupt claimed the new turn")
	}
	state, err := readState(store.snapshot().StateDir)
	if err != nil || state.Status != "active" || r.stopChild.Load() {
		t.Fatalf("old timeout changed the new turn: %+v, %v", state, err)
	}
	if !r.endSessionIfTurnOpen(r.turnDone, "failed", "current interrupt timeout") {
		t.Fatal("current interrupt could not claim its turn")
	}
	state, err = readState(store.snapshot().StateDir)
	if err != nil || state.Status != "failed" || !r.stopChild.Load() || r.privateResultError() != "current interrupt timeout" {
		t.Fatalf("current timeout did not fail the session: %+v, %v", state, err)
	}
}

func TestInterruptExpectedTurnID(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stateDir, errCh := startIdleHelperRunContext(t, ctx, map[string]string{
		"GO_WANT_RUDDR_MULTI_TURN_HOLD": "1",
	}, nil)
	t.Cleanup(func() {
		cancel()
		select {
		case err := <-errCh:
			if err == nil {
				t.Error("canceled controller reported success")
			}
		case <-time.After(5 * time.Second):
			t.Error("controller did not exit")
		}
	})
	waitForRunStatus(t, stateDir, "active")
	err := interruptCommand([]string{"--state-dir", stateDir, "--expected-turn-id", "old-turn"})
	if err == nil || !strings.Contains(err.Error(), "interrupt was not sent") {
		t.Fatalf("stale CLI interrupt = %v", err)
	}
	response, err := sendControl(stateDir, controlRequest{Command: "interrupt", ExpectedTurnID: "old-turn"}, time.Second)
	if err != nil || response.OK || !strings.Contains(response.Error, "interrupt was not sent") {
		t.Fatalf("stale socket interrupt = %+v, %v", response, err)
	}
	state, err := readState(stateDir)
	if err != nil || state.Status != "active" || state.TurnID != "turn-1" {
		t.Fatalf("rejected interrupt changed state: %+v, %v", state, err)
	}
	output, err := captureStdout(func() error {
		return interruptCommand([]string{"--state-dir", stateDir, "--expected-turn-id", "turn-1"})
	})
	if err != nil || string(output) != "interrupt requested for turn turn-1\n" {
		t.Fatalf("targeted interrupt = %q, %v", output, err)
	}
	waitForRunStatus(t, stateDir, "idle")
	response, err = sendControl(stateDir, controlRequest{Command: "prompt", Text: "next turn"}, 5*time.Second)
	if err != nil || !response.OK {
		t.Fatalf("next prompt = %+v, %v", response, err)
	}
	output, err = captureStdout(func() error { return interruptCommand([]string{"--state-dir", stateDir}) })
	if err != nil || string(output) != "interrupt requested for turn turn-2\n" {
		t.Fatalf("default interrupt = %q, %v", output, err)
	}
	waitForRunStatus(t, stateDir, "idle")
}

func TestRejectedInterruptDoesNotWriteRPC(t *testing.T) {
	store, err := newStateStore(runConfig{StateDir: filepath.Join(t.TempDir(), "run")})
	if err != nil {
		t.Fatal(err)
	}
	if err := store.update(func(state *runState) {
		state.Status, state.ThreadID, state.TurnID = "active", "thread-1", "turn-2"
	}); err != nil {
		t.Fatal(err)
	}
	writer := &bufferWriteCloser{}
	r := &controller{store: store, childIn: writer}
	defer r.closeControlServer()
	if err := r.interrupt("turn-1"); err == nil || !strings.Contains(err.Error(), "interrupt was not sent") {
		t.Fatalf("stale interrupt = %v", err)
	}
	if writer.Len() != 0 {
		t.Fatalf("stale interrupt sent provider input: %q", writer.String())
	}
}
