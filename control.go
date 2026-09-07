package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"time"
)

type controlRequest struct {
	Command        string `json:"command"`
	Text           string `json:"text,omitempty"`
	ExpectedTurnID string `json:"expectedTurnId,omitempty"`
}

type controlResponse struct {
	OK    bool     `json:"ok"`
	Error string   `json:"error,omitempty"`
	State runState `json:"state"`
}

func (r *controller) startControlServer() error {
	path := r.store.snapshot().SocketPath
	parent, err := os.Stat(filepath.Dir(path))
	if err != nil {
		return fmt.Errorf("inspect control socket parent: %w", err)
	}
	if parent.Mode().Perm()&0o077 != 0 {
		return fmt.Errorf("control socket parent %s must be owner-only, mode is %o", filepath.Dir(path), parent.Mode().Perm())
	}
	if err := removeStaleSocket(path); err != nil {
		return err
	}
	listener, err := net.Listen("unix", path)
	if err != nil {
		return err
	}
	if err := os.Chmod(path, 0o600); err != nil {
		_ = listener.Close()
		_ = os.Remove(path)
		return err
	}
	r.listener = listener
	go r.acceptControl()
	return nil
}

func (r *controller) acceptControl() {
	retryDelay := 5 * time.Millisecond
	for {
		conn, err := r.listener.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return
			}
			select {
			case <-r.sessionDone:
				return
			default:
			}
			if netErr, ok := err.(net.Error); ok && (netErr.Timeout() || netErr.Temporary()) {
				r.tracef("[warn] temporary control accept error: %v", err)
				timer := time.NewTimer(retryDelay)
				select {
				case <-timer.C:
				case <-r.sessionDone:
					if !timer.Stop() {
						<-timer.C
					}
					return
				}
				if retryDelay < time.Second {
					retryDelay *= 2
				}
				continue
			}
			r.tracef("[warn] control socket closed after accept error: %v", err)
			return
		}
		retryDelay = 5 * time.Millisecond
		go r.handleControl(conn)
	}
}

func (r *controller) handleControl(conn net.Conn) {
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(2 * time.Minute))
	var request controlRequest
	if err := json.NewDecoder(conn).Decode(&request); err != nil {
		_ = json.NewEncoder(conn).Encode(controlResponse{OK: false, Error: err.Error(), State: r.store.snapshot()})
		return
	}
	response := controlResponse{State: r.store.snapshot()}
	switch request.Command {
	case "status":
		response.OK = true
	case "steer":
		if request.Text == "" {
			response.Error = "steering text is empty"
			break
		}
		if err := r.steer(request.Text, request.ExpectedTurnID); err != nil {
			response.Error = err.Error()
			break
		}
		response.OK = true
	case "interrupt":
		if err := r.interrupt(); err != nil {
			response.Error = err.Error()
			break
		}
		response.OK = true
	case "prompt":
		if request.Text == "" {
			response.Error = "prompt text is empty"
			break
		}
		if err := r.prompt(request.Text); err != nil {
			response.Error = err.Error()
			break
		}
		response.OK = true
	case "shutdown":
		if err := r.shutdown(); err != nil {
			response.Error = err.Error()
			break
		}
		response.OK = true
	default:
		response.Error = fmt.Sprintf("unknown control command %q", request.Command)
	}
	response.State = r.store.snapshot()
	_ = json.NewEncoder(conn).Encode(response)
}

func (r *controller) steer(text, expectedTurnID string) error {
	state := r.store.snapshot()
	if state.Status != "active" || state.ThreadID == "" || state.TurnID == "" {
		return fmt.Errorf("turn is not steerable: status=%s", state.Status)
	}
	if expectedTurnID != "" && state.TurnID != expectedTurnID {
		return fmt.Errorf("active turn changed from %s to %s; steer was not sent", expectedTurnID, state.TurnID)
	}
	var result struct {
		TurnID string `json:"turnId"`
	}
	err := r.call("turn/steer", map[string]any{
		"threadId":       state.ThreadID,
		"expectedTurnId": state.TurnID,
		"input": []map[string]any{{
			"type": "text",
			"text": text,
		}},
	}, &result, 30*time.Second)
	if err != nil {
		return err
	}
	if result.TurnID != state.TurnID {
		return fmt.Errorf("provider acknowledged unexpected turn %s", result.TurnID)
	}
	if err := r.store.update(func(current *runState) { current.Steers++ }); err != nil {
		persistErr := fmt.Errorf("persist steer count: %w", err)
		r.stopChild.Store(true)
		r.fail(persistErr)
		terminateProcessTree(r.child, false)
		return persistErr
	}
	r.tracef("[steer] accepted for turn %s: %s", state.TurnID, oneLine(text, 180))
	return nil
}

// prompt starts a new turn on the existing thread. It is only valid while the
// session is idle and is never converted into a steer (or vice versa).
func (r *controller) prompt(text string) error {
	if !r.cfg.Idle {
		return errors.New("session was not started with --idle; use a new run to continue the thread")
	}
	state := r.store.snapshot()
	if state.Status != "idle" {
		if state.Status == "active" {
			return errors.New("a turn is active; steer it instead")
		}
		return fmt.Errorf("session is not idle: status=%s", state.Status)
	}
	request := promptRequest{text: text, observedTurns: state.Turns, reply: make(chan error, 1)}
	// TODO(review): Propagate custom client deadlines if --timeout must guarantee that no turn starts after the client stops waiting.
	select {
	case r.promptCh <- request:
	case <-r.sessionDone:
		return errors.New("session ended before the prompt was accepted")
	case <-time.After(5 * time.Second):
		return errors.New("session did not accept the prompt; it may no longer be idle")
	}
	select {
	case err := <-request.reply:
		if err != nil {
			return err
		}
	case <-time.After(45 * time.Second):
		return errors.New("timed out waiting for turn/start")
	case <-r.sessionDone:
		return errors.New("session ended while waiting for turn/start")
	}
	r.tracef("[prompt] accepted: %s", oneLine(text, 180))
	return nil
}

// shutdown gracefully ends an idle session.
func (r *controller) shutdown() error {
	if !r.cfg.Idle {
		return errors.New("session was not started with --idle")
	}
	accepted := false
	status := ""
	r.turnMu.Lock()
	if err := r.store.update(func(state *runState) {
		status = state.Status
		if !r.sessionEnded && state.Status == "idle" {
			state.Status = "stopping"
			accepted = true
		}
	}); err != nil {
		r.turnMu.Unlock()
		return fmt.Errorf("persist stopping state: %w", err)
	}
	r.turnMu.Unlock()
	if !accepted {
		return fmt.Errorf("session is not idle: status=%s", status)
	}
	r.shutdownOnce.Do(func() { close(r.shutdownCh) })
	return nil
}

func (r *controller) interrupt() error {
	state := r.store.snapshot()
	if state.Status != "active" || state.ThreadID == "" || state.TurnID == "" {
		return fmt.Errorf("turn is not active: status=%s", state.Status)
	}
	r.turnMu.Lock()
	turnDone := r.turnDone
	r.turnMu.Unlock()
	interruptTimeout := r.cfg.InterruptTimeout
	if interruptTimeout <= 0 {
		interruptTimeout = defaultInterruptOperationTimeout
	}
	deadline := time.Now().Add(interruptTimeout)
	rpcErr := r.call("turn/interrupt", map[string]any{
		"threadId": state.ThreadID,
		"turnId":   state.TurnID,
	}, nil, time.Until(deadline))
	if r.cfg.Idle && rpcErr == nil {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			remaining = time.Nanosecond
		}
		timer := time.NewTimer(remaining)
		defer timer.Stop()
		select {
		case <-turnDone:
			return r.interruptSettlementResult()
		case <-r.sessionDone:
			select {
			case <-turnDone:
				return r.interruptSettlementResult()
			default:
			}
			return errors.New("session ended before the interrupted turn settled")
		case <-timer.C:
			settleErr := fmt.Errorf("interrupted turn %s did not settle within %s", state.TurnID, interruptTimeout)
			if !r.endSessionIfTurnOpen("failed", settleErr.Error()) {
				return r.interruptSettlementResult()
			}
			r.stopChild.Store(true)
			terminateProcessTree(r.child, false)
			return settleErr
		}
	}
	r.stopChild.Store(true)
	if rpcErr != nil {
		r.tracef("[warn] turn/interrupt failed; forcing local teardown: %v", rpcErr)
	}
	r.endSession("interrupted", "")
	terminateProcessTree(r.child, false)
	finalState := r.store.snapshot()
	if finalState.Status == "interrupted" {
		return nil
	}
	if resultErr := r.privateResultError(); resultErr != "" {
		return errors.New(resultErr)
	}
	return rpcErr
}

func (r *controller) interruptSettlementResult() error {
	r.turnMu.Lock()
	lastTurn := r.lastTurn
	r.turnMu.Unlock()
	state := r.store.snapshot()
	if state.Status == "failed" || lastTurn == "failed" {
		if resultErr := r.privateResultError(); resultErr != "" {
			return errors.New(resultErr)
		}
		if state.Error != "" {
			return errors.New(state.Error)
		}
		return errors.New("provider failed before the interrupted turn settled")
	}
	return nil
}

func sendControl(stateDir string, request controlRequest, timeout time.Duration) (controlResponse, error) {
	state, err := readState(stateDir)
	if err != nil {
		return controlResponse{}, err
	}
	if err := ensureControllerLive(state); err != nil {
		return controlResponse{}, err
	}
	conn, err := net.DialTimeout("unix", state.SocketPath, timeout)
	if err != nil {
		return controlResponse{}, fmt.Errorf("connect to Ruddr pid %d at %s: %w", state.PID, state.SocketPath, err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(timeout))
	if err := json.NewEncoder(conn).Encode(request); err != nil {
		return controlResponse{}, err
	}
	var response controlResponse
	if err := json.NewDecoder(bufio.NewReader(conn)).Decode(&response); err != nil {
		return controlResponse{}, err
	}
	return response, nil
}

func ensureControllerLive(state runState) error {
	if !processAlive(state.PID) {
		return fmt.Errorf("Ruddr pid %d is not running; state is stale at status=%s", state.PID, state.Status)
	}
	info, err := os.Lstat(state.SocketPath)
	if err != nil {
		return fmt.Errorf("Ruddr pid %d has no control socket at %s: %w", state.PID, state.SocketPath, err)
	}
	if info.Mode()&os.ModeSocket == 0 {
		return fmt.Errorf("Ruddr control path is not a socket: %s", state.SocketPath)
	}
	return nil
}

func removeStaleSocket(path string) error {
	info, err := os.Lstat(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	if info.Mode()&os.ModeSocket == 0 {
		return fmt.Errorf("refusing to remove non-socket control path %s", path)
	}
	return os.Remove(path)
}
