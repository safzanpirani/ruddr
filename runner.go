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
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	maxRPCLineBytes                  = 64 * 1024 * 1024
	defaultRPCWriteTimeout           = 10 * time.Second
	defaultIdleTurnStartTimeout      = 40 * time.Second
	defaultInterruptOperationTimeout = 30 * time.Second
)

type runConfig struct {
	Provider          string
	CWD               string
	PromptFile        string
	StateDir          string
	Model             string
	Effort            string
	Sandbox           string
	ApprovalPolicy    string
	ClaudePath        string
	OpenCodePath      string
	PiPath            string
	ProviderPath      string
	Ephemeral         bool
	ResumeThreadID    string
	ForkThreadID      string
	ForkBeforeTurnID  string
	ForkThroughTurnID string
	TurnTimeout       time.Duration
	Idle              bool
	IdleTimeout       time.Duration
	ChildCommand      []string
	RegisterRun       bool
	// The internal seams keep lifecycle regression tests fast and deterministic.
	IdleTurnStartTimeout time.Duration
	InterruptTimeout     time.Duration
	BeforeStateReserve   func()
}

type promptRequest struct {
	text          string
	observedTurns int
	reply         chan error
}

type rpcError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type rpcResponseError struct {
	code    int
	message string
}

func (e *rpcResponseError) Error() string {
	return fmt.Sprintf("%s (%d)", e.message, e.code)
}

type ambiguousTurnStartError struct {
	err error
}

func (e *ambiguousTurnStartError) Error() string {
	return e.err.Error()
}

func (e *ambiguousTurnStartError) Unwrap() error {
	return e.err
}

type rpcEnvelope struct {
	ID     json.RawMessage `json:"id,omitempty"`
	Method string          `json:"method,omitempty"`
	Params json.RawMessage `json:"params,omitempty"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *rpcError       `json:"error,omitempty"`
}

type controller struct {
	cfg           runConfig
	store         *stateStore
	child         *exec.Cmd
	childIn       io.WriteCloser
	listener      net.Listener
	events        *os.File
	trace         *os.File
	stderr        *os.File
	writeOnce     sync.Once
	writeGate     chan struct{}
	eventsMu      sync.Mutex
	traceMu       sync.Mutex
	outputMu      sync.Mutex
	outputParts   []string
	resultMu      sync.Mutex
	resultError   string
	pendingMu     sync.Mutex
	pending       map[string]chan rpcEnvelope
	nextID        atomic.Uint64
	promptEventID atomic.Uint64
	turnMu        sync.Mutex
	turnDone      chan struct{}
	turnEnded     bool
	turnCount     int
	lastTurn      string
	sessionEnded  bool
	sessionDone   chan struct{}
	sessionOnce   sync.Once
	cancelOnce    sync.Once
	promptCh      chan promptRequest
	shutdownCh    chan struct{}
	shutdownOnce  sync.Once
	waitCh        chan error
	readDone      chan struct{}
	stopChild     atomic.Bool
}

func runController(cfg runConfig) error {
	return runControllerContext(context.Background(), cfg)
}

func runControllerContext(ctx context.Context, cfg runConfig) error {
	if err := validateRunConfig(&cfg); err != nil {
		return err
	}
	prompt, err := os.ReadFile(cfg.PromptFile)
	if err != nil {
		return err
	}
	if len(bytes.TrimSpace(prompt)) == 0 {
		return errors.New("prompt file is empty")
	}
	store, err := newStateStore(cfg)
	if err != nil {
		return err
	}
	r := &controller{
		cfg:         cfg,
		store:       store,
		pending:     make(map[string]chan rpcEnvelope),
		turnDone:    make(chan struct{}),
		sessionDone: make(chan struct{}),
		promptCh:    make(chan promptRequest),
		shutdownCh:  make(chan struct{}),
		waitCh:      make(chan error, 1),
		readDone:    make(chan struct{}),
	}
	defer r.closeControlServer()
	if err := r.openLogs(); err != nil {
		r.fail(err)
		return err
	}
	defer r.closeLogs()
	if err := r.startChild(); err != nil {
		r.fail(err)
		return err
	}
	defer r.shutdownChild()
	cancelWatchDone := make(chan struct{})
	defer close(cancelWatchDone)
	go func() {
		select {
		case <-ctx.Done():
			r.cancelSession()
		case <-cancelWatchDone:
		}
	}()
	if err := r.startControlServer(); err != nil {
		r.fail(err)
		return err
	}

	if err := r.initialize(string(prompt)); err != nil {
		if ctx.Err() != nil {
			return fmt.Errorf("run interrupted: %w", ctx.Err())
		}
		r.fail(err)
		return err
	}
	r.waitTurn()
	if cfg.Idle {
		r.idleLoop(ctx)
	}
	state := r.store.snapshot()
	if state.Status == "completed" {
		return nil
	}
	if resultErr := r.privateResultError(); resultErr != "" {
		return errors.New(resultErr)
	}
	if state.Error != "" {
		return errors.New(state.Error)
	}
	return fmt.Errorf("turn ended with status %s", state.Status)
}

func validateRunConfig(cfg *runConfig) error {
	provider, err := normalizeProvider(cfg.Provider)
	if err != nil {
		return err
	}
	cfg.Provider = provider
	if len(cfg.ChildCommand) == 0 {
		return errors.New("provider command is empty")
	}
	cwd, err := filepath.Abs(cfg.CWD)
	if err != nil {
		return err
	}
	cfg.CWD = cwd
	if cfg.Provider == providerCodex && cfg.Model == "" {
		return errors.New("model is required")
	}
	if cfg.Provider != providerCodex {
		if cfg.ApprovalPolicy != "never" {
			return fmt.Errorf("%s runs require --approval-policy never because Ruddr has no interactive approval surface", cfg.Provider)
		}
		if cfg.ForkThreadID != "" || cfg.ForkBeforeTurnID != "" || cfg.ForkThroughTurnID != "" {
			return fmt.Errorf("%s runs do not yet support --fork-thread or fork turn selectors; use --resume-thread", cfg.Provider)
		}
	}
	if cfg.Idle && cfg.Ephemeral && cfg.Provider == providerClaude {
		return errors.New("--idle requires a persisted Claude session; drop --ephemeral")
	}
	if cfg.ResumeThreadID != "" && cfg.ForkThreadID != "" {
		return errors.New("--resume-thread and --fork-thread are mutually exclusive")
	}
	if cfg.ForkBeforeTurnID != "" && cfg.ForkThroughTurnID != "" {
		return errors.New("--fork-before-turn and --fork-through-turn are mutually exclusive")
	}
	if (cfg.ForkBeforeTurnID != "" || cfg.ForkThroughTurnID != "") && cfg.ForkThreadID == "" {
		return errors.New("fork turn selectors require --fork-thread")
	}
	if cfg.TurnTimeout < 0 {
		return errors.New("--turn-timeout must be zero or positive")
	}
	if cfg.IdleTimeout < 0 {
		return errors.New("--idle-timeout must be zero or positive")
	}
	switch cfg.Sandbox {
	case "read-only", "workspace-write", "danger-full-access":
	default:
		return fmt.Errorf("unsupported sandbox %q", cfg.Sandbox)
	}
	return nil
}

func (r *controller) openLogs() error {
	state := r.store.snapshot()
	var err error
	if r.events, err = openPrivateLog(state.EventsPath); err != nil {
		return err
	}
	if r.trace, err = openPrivateLog(state.TracePath); err != nil {
		r.events.Close()
		return err
	}
	if r.stderr, err = openPrivateLog(state.StderrPath); err != nil {
		r.trace.Close()
		r.events.Close()
		return err
	}
	output, err := openPrivateLog(state.OutputPath)
	if err != nil {
		r.stderr.Close()
		r.trace.Close()
		r.events.Close()
		return err
	}
	if err := output.Close(); err != nil {
		r.stderr.Close()
		r.trace.Close()
		r.events.Close()
		return err
	}
	return nil
}

func openPrivateLog(path string) (*os.File, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return nil, err
	}
	if err := f.Chmod(0o600); err != nil {
		f.Close()
		return nil, err
	}
	return f, nil
}

func (r *controller) closeLogs() {
	if r.stderr != nil {
		r.stderr.Close()
	}
	if r.trace != nil {
		r.trace.Close()
	}
	if r.events != nil {
		r.events.Close()
	}
}

func (r *controller) startChild() error {
	r.child = exec.Command(r.cfg.ChildCommand[0], r.cfg.ChildCommand[1:]...)
	configureChildProcess(r.child)
	r.child.Dir = r.cfg.CWD
	childOut, err := r.child.StdoutPipe()
	if err != nil {
		return err
	}
	r.childIn, err = r.child.StdinPipe()
	if err != nil {
		_ = childOut.Close()
		return err
	}
	r.child.Stderr = r.stderr
	if err := r.child.Start(); err != nil {
		_ = childOut.Close()
		_ = r.childIn.Close()
		return err
	}
	go r.readChild(childOut)
	go func() {
		<-r.readDone
		r.waitCh <- r.child.Wait()
	}()
	if err := r.store.update(func(state *runState) { state.ChildPID = r.child.Process.Pid }); err != nil {
		r.stopChild.Store(true)
		r.shutdownChild()
		return fmt.Errorf("persist child pid: %w", err)
	}
	r.tracef(
		"[start] child pid=%d executable=%s args=%d",
		r.child.Process.Pid,
		filepath.Base(r.cfg.ChildCommand[0]),
		len(r.cfg.ChildCommand)-1,
	)
	return nil
}

func (r *controller) initialize(prompt string) error {
	if err := r.initializeSession(); err != nil {
		return err
	}

	threadID, mode, err := r.acquireThread()
	if err != nil {
		return err
	}
	if err := r.store.update(func(state *runState) { state.ThreadID = threadID }); err != nil {
		return fmt.Errorf("persist thread id: %w", err)
	}
	r.tracef("[thread] %s %s", mode, threadID)
	return r.startTurn(prompt, 60*time.Second)
}

// startTurn opens a new turn lifecycle and issues turn/start on the acquired
// thread. Turn one reuses the channel allocated at construction; later turns
// (idle mode) get a fresh one. The prompt text never reaches state.json or the
// trace beyond a truncated single line.
func (r *controller) startTurn(prompt string, timeout time.Duration) error {
	r.turnMu.Lock()
	r.turnCount++
	turnNumber := r.turnCount
	if turnNumber > 1 {
		r.turnDone = make(chan struct{})
		r.turnEnded = false
	}
	r.turnMu.Unlock()
	if err := r.store.update(func(state *runState) {
		state.Turns = turnNumber
		if turnNumber > 1 {
			state.TurnID = ""
			state.Error = ""
			state.CompletedAt = time.Time{}
		}
	}); err != nil {
		r.abandonTurn()
		return fmt.Errorf("persist turn count: %w", err)
	}
	if turnNumber > 1 {
		r.appendOutputSeparator()
		r.tracef("[turn] prompt #%d: %s", turnNumber, oneLine(prompt, 180))
	}
	state := r.store.snapshot()
	turnParams := map[string]any{
		"threadId": state.ThreadID,
		"input": []map[string]any{{
			"type": "text",
			"text": prompt,
		}},
	}
	if r.cfg.Effort != "" {
		turnParams["effort"] = r.cfg.Effort
	}
	var turnResult struct {
		Turn struct {
			ID     string `json:"id"`
			Status string `json:"status"`
		} `json:"turn"`
	}
	promptEventID := fmt.Sprintf("ruddr-prompt-%d", r.promptEventID.Add(1))
	if err := r.recordPromptAttempt(promptEventID, prompt); err != nil {
		if rollbackErr := r.rollbackRejectedTurn(turnNumber); rollbackErr != nil {
			return fmt.Errorf("record prompt attempt: %v; rollback: %w", err, rollbackErr)
		}
		return fmt.Errorf("record prompt attempt: %w", err)
	}
	if err := r.call("turn/start", turnParams, &turnResult, timeout); err != nil {
		accepted := r.store.snapshot().TurnID != ""
		var responseErr *rpcResponseError
		if !accepted && errors.As(err, &responseErr) {
			if eventErr := r.recordPromptDecision(promptEventID, "rejected"); eventErr != nil {
				return &ambiguousTurnStartError{err: fmt.Errorf("record rejected prompt: %w", eventErr)}
			}
			if rollbackErr := r.rollbackRejectedTurn(turnNumber); rollbackErr != nil {
				return &ambiguousTurnStartError{err: fmt.Errorf("turn/start rejection could not be rolled back: %w", rollbackErr)}
			}
			return fmt.Errorf("start turn: %w", err)
		}
		decision := "unknown"
		if accepted {
			decision = "accepted"
		}
		if eventErr := r.recordPromptDecision(promptEventID, decision); eventErr != nil {
			return &ambiguousTurnStartError{err: fmt.Errorf("record ambiguous prompt outcome: %w", eventErr)}
		}
		return &ambiguousTurnStartError{err: fmt.Errorf("start turn outcome is ambiguous: %w", err)}
	}
	if turnResult.Turn.ID == "" {
		if err := r.recordPromptDecision(promptEventID, "unknown"); err != nil {
			return &ambiguousTurnStartError{err: fmt.Errorf("turn/start response returned no turn id; record unknown prompt outcome: %w", err)}
		}
		return &ambiguousTurnStartError{err: errors.New("turn/start outcome is ambiguous: response returned no turn id")}
	}
	if err := r.recordPromptDecision(promptEventID, "accepted"); err != nil {
		return &ambiguousTurnStartError{err: fmt.Errorf("record accepted prompt: %w", err)}
	}
	r.turnMu.Lock()
	err := r.store.update(func(current *runState) {
		current.TurnID = turnResult.Turn.ID
		if !r.turnEnded && !r.sessionEnded {
			current.Status = "active"
		}
	})
	r.turnMu.Unlock()
	if err != nil {
		return &ambiguousTurnStartError{err: fmt.Errorf("turn/start outcome is ambiguous: persist active turn: %w", err)}
	}
	r.tracef("[turn] active thread=%s turn=%s", state.ThreadID, turnResult.Turn.ID)
	return nil
}

func (r *controller) rollbackRejectedTurn(turnNumber int) error {
	if err := r.store.update(func(state *runState) {
		if state.Turns == turnNumber {
			state.Turns = turnNumber - 1
		}
		state.TurnID = ""
	}); err != nil {
		return fmt.Errorf("persist rejected turn rollback: %w", err)
	}
	r.outputMu.Lock()
	if len(r.outputParts) > 0 && r.outputParts[len(r.outputParts)-1] == "---" {
		r.outputParts = r.outputParts[:len(r.outputParts)-1]
	}
	r.outputMu.Unlock()
	r.turnMu.Lock()
	defer r.turnMu.Unlock()
	if r.turnEnded {
		return errors.New("rejected turn ended before rollback")
	}
	r.turnEnded = true
	if r.turnCount == turnNumber {
		r.turnCount--
	}
	close(r.turnDone)
	return nil
}

// abandonTurn closes an open turn lifecycle without persisting a terminal
// status; used when turn/start itself fails so the session can return to idle.
func (r *controller) abandonTurn() {
	r.turnMu.Lock()
	defer r.turnMu.Unlock()
	if r.turnEnded {
		return
	}
	r.turnEnded = true
	close(r.turnDone)
}

func (r *controller) waitTurn() {
	r.turnMu.Lock()
	turnDone := r.turnDone
	r.turnMu.Unlock()
	if r.cfg.TurnTimeout > 0 {
		timer := time.NewTimer(r.cfg.TurnTimeout)
		select {
		case <-turnDone:
			if !timer.Stop() {
				<-timer.C
			}
		case <-timer.C:
			r.stopChild.Store(true)
			r.tracef("[error] active turn exceeded watchdog %s", r.cfg.TurnTimeout)
			r.endSession("failed", "turn watchdog expired")
			terminateProcessTree(r.child, false)
		}
	} else {
		<-turnDone
	}
}

// idleLoop keeps the session alive between turns, accepting prompt and
// shutdown commands from the control socket until the session ends.
func (r *controller) idleLoop(ctx context.Context) {
	for {
		select {
		case <-r.sessionDone:
			r.ensureTerminalExit()
			return
		case <-r.shutdownCh:
			r.tracef("[shutdown] requested while idle")
			r.persistFinalIdleExit()
			return
		default:
		}
		r.turnMu.Lock()
		if r.sessionEnded {
			r.turnMu.Unlock()
			r.ensureTerminalExit()
			return
		}
		err := r.store.update(func(state *runState) {
			state.Status = "idle"
			state.Error = ""
			state.CompletedAt = time.Time{}
		})
		r.turnMu.Unlock()
		if err != nil {
			r.fail(fmt.Errorf("persist idle state: %w", err))
			return
		}
		r.tracef("[idle] waiting for prompt")
		var idleTimeout <-chan time.Time
		var idleTimer *time.Timer
		if r.cfg.IdleTimeout > 0 {
			idleTimer = time.NewTimer(r.cfg.IdleTimeout)
			idleTimeout = idleTimer.C
		}
		stopTimer := func() {
			if idleTimer != nil && !idleTimer.Stop() {
				select {
				case <-idleTimer.C:
				default:
				}
			}
		}
		select {
		case request := <-r.promptCh:
			stopTimer()
			accepted := false
			r.turnMu.Lock()
			err := r.store.update(func(state *runState) {
				if !r.sessionEnded && state.Status == "idle" && state.Turns == request.observedTurns {
					state.Status = "starting"
					state.TurnID = ""
					accepted = true
				}
			})
			r.turnMu.Unlock()
			if err == nil && !accepted {
				err = errors.New("session left the observed idle turn before the prompt was accepted")
			}
			if err == nil {
				timeout := r.cfg.IdleTurnStartTimeout
				if timeout <= 0 {
					timeout = defaultIdleTurnStartTimeout
				}
				err = r.startTurn(request.text, timeout)
			}
			if err != nil {
				r.tracef("[error] prompt turn failed to start: %v", err)
				var ambiguous *ambiguousTurnStartError
				if errors.As(err, &ambiguous) {
					r.stopChild.Store(true)
					r.fail(err)
					terminateProcessTree(r.child, false)
					request.reply <- err
					return
				}
				if accepted {
					r.turnMu.Lock()
					updateErr := r.store.update(func(state *runState) {
						if !r.sessionEnded && state.Status == "starting" {
							state.Status = "idle"
						}
					})
					r.turnMu.Unlock()
					if updateErr != nil {
						r.fail(fmt.Errorf("restore idle state: %w", updateErr))
						return
					}
				}
				request.reply <- err
				continue
			}
			request.reply <- nil
			r.waitTurn()
		case <-r.shutdownCh:
			stopTimer()
			r.tracef("[shutdown] requested while idle")
			r.persistFinalIdleExit()
			return
		case <-idleTimeout:
			r.tracef("[idle] timeout after %s", r.cfg.IdleTimeout)
			r.persistFinalIdleExit()
			return
		case <-r.sessionDone:
			stopTimer()
			r.ensureTerminalExit()
			return
		case <-ctx.Done():
			stopTimer()
			r.cancelSession()
			return
		}
	}
}

// ensureTerminalExit guards against exiting with a non-terminal "idle" status
// when the session ends between turns.
func (r *controller) ensureTerminalExit() {
	if !terminalStatus(r.store.snapshot().Status) {
		r.persistFinalIdleExit()
	}
}

// persistFinalIdleExit restores the last turn's terminal status so the run
// exits with a truthful state instead of "idle".
func (r *controller) persistFinalIdleExit() {
	r.turnMu.Lock()
	if r.sessionEnded {
		r.turnMu.Unlock()
		return
	}
	r.sessionEnded = true
	status := r.lastTurn
	if status == "" {
		status = "completed"
	}
	if err := r.store.update(func(state *runState) {
		state.Status = status
		state.CompletedAt = time.Now().UTC()
	}); err != nil {
		r.appendResultError(fmt.Sprintf("persist final state: %v", err))
	}
	r.turnMu.Unlock()
	r.sessionOnce.Do(func() { close(r.sessionDone) })
}

func (r *controller) initializeSession() error {
	var initialized map[string]any
	if err := r.call("initialize", map[string]any{
		"clientInfo": map[string]any{
			"name":    "ruddr",
			"title":   "Ruddr",
			"version": version,
		},
		"capabilities": map[string]any{
			"experimentalApi": true,
		},
	}, &initialized, 30*time.Second); err != nil {
		return fmt.Errorf("initialize provider: %w", err)
	}
	if err := r.notify("initialized", map[string]any{}); err != nil {
		return err
	}
	return nil
}

func (r *controller) acquireThread() (string, string, error) {
	var threadResult struct {
		Thread struct {
			ID string `json:"id"`
		} `json:"thread"`
	}
	baseParams := map[string]any{
		"cwd":            r.cfg.CWD,
		"approvalPolicy": r.cfg.ApprovalPolicy,
		"sandbox":        r.cfg.Sandbox,
		"provider":       r.cfg.Provider,
	}
	if r.cfg.Model != "" {
		baseParams["model"] = r.cfg.Model
	}
	if r.cfg.Provider == providerClaude {
		baseParams["persistSession"] = !r.cfg.Ephemeral
		if r.cfg.ClaudePath != "" {
			baseParams["claudePath"] = r.cfg.ClaudePath
		}
	}
	if r.cfg.ProviderPath != "" {
		baseParams["providerPath"] = r.cfg.ProviderPath
	}
	method := "thread/start"
	mode := "started"
	baseParams["ephemeral"] = r.cfg.Ephemeral
	baseParams["serviceName"] = "ruddr"
	if r.cfg.ResumeThreadID != "" {
		method = "thread/resume"
		mode = "resumed"
		delete(baseParams, "ephemeral")
		delete(baseParams, "serviceName")
		baseParams["threadId"] = r.cfg.ResumeThreadID
		baseParams["excludeTurns"] = true
	} else if r.cfg.ForkThreadID != "" {
		method = "thread/fork"
		mode = "forked"
		delete(baseParams, "serviceName")
		baseParams["threadId"] = r.cfg.ForkThreadID
		baseParams["excludeTurns"] = true
		if r.cfg.ForkBeforeTurnID != "" {
			baseParams["beforeTurnId"] = r.cfg.ForkBeforeTurnID
		}
		if r.cfg.ForkThroughTurnID != "" {
			baseParams["lastTurnId"] = r.cfg.ForkThroughTurnID
		}
	}
	if err := r.call(method, baseParams, &threadResult, 60*time.Second); err != nil {
		return "", mode, fmt.Errorf("%s: %w", method, err)
	}
	if threadResult.Thread.ID == "" {
		return "", mode, fmt.Errorf("%s returned no thread id", method)
	}
	return threadResult.Thread.ID, mode, nil
}

func (r *controller) call(method string, params any, target any, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	id := fmt.Sprintf("ruddr-%d", r.nextID.Add(1))
	responseCh := make(chan rpcEnvelope, 1)
	r.pendingMu.Lock()
	r.pending[id] = responseCh
	r.pendingMu.Unlock()
	defer func() {
		r.pendingMu.Lock()
		delete(r.pending, id)
		r.pendingMu.Unlock()
	}()
	if err := r.writeRPCWithin(map[string]any{"id": id, "method": method, "params": params}, time.Until(deadline)); err != nil {
		return err
	}
	remaining := time.Until(deadline)
	if remaining <= 0 {
		return fmt.Errorf("%s timed out after %s", method, timeout)
	}
	timer := time.NewTimer(remaining)
	defer timer.Stop()
	select {
	case response := <-responseCh:
		if response.Error != nil {
			return &rpcResponseError{code: response.Error.Code, message: response.Error.Message}
		}
		if target != nil && len(response.Result) > 0 {
			if err := json.Unmarshal(response.Result, target); err != nil {
				return err
			}
		}
		return nil
	case <-timer.C:
		return fmt.Errorf("%s timed out after %s", method, timeout)
	case <-r.sessionDone:
		state := r.store.snapshot()
		return fmt.Errorf("session ended while waiting for %s: %s", method, state.Status)
	}
}

func (r *controller) notify(method string, params any) error {
	return r.writeRPC(map[string]any{"method": method, "params": params})
}

func (r *controller) writeRPC(message any) error {
	return r.writeRPCWithin(message, defaultRPCWriteTimeout)
}

func (r *controller) writeRPCWithin(message any, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	raw, err := json.Marshal(message)
	if err != nil {
		return err
	}
	r.writeOnce.Do(func() { r.writeGate = make(chan struct{}, 1) })
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	select {
	case r.writeGate <- struct{}{}:
		defer func() { <-r.writeGate }()
	case <-timer.C:
		return errors.New("app-server stdin write timed out")
	case <-r.sessionDone:
		return errors.New("session ended before app-server stdin write")
	}
	return writeAllWithTimeout(r.childIn, append(raw, '\n'), time.Until(deadline))
}

func writeAllWithTimeout(writer io.WriteCloser, data []byte, timeout time.Duration) error {
	if timeout <= 0 {
		return errors.New("app-server stdin write timed out")
	}
	deadline := time.Now().Add(timeout)
	if deadlineWriter, ok := writer.(interface{ SetWriteDeadline(time.Time) error }); ok {
		if err := deadlineWriter.SetWriteDeadline(deadline); err == nil {
			defer deadlineWriter.SetWriteDeadline(time.Time{})
			if _, err := writer.Write(data); err != nil {
				if errors.Is(err, os.ErrDeadlineExceeded) {
					_ = writer.Close()
					return fmt.Errorf("app-server stdin write timed out after %s", timeout)
				}
				return err
			}
			return nil
		}
	}
	type writeResult struct {
		n   int
		err error
	}
	resultCh := make(chan writeResult, 1)
	go func() {
		n, err := writer.Write(data)
		resultCh <- writeResult{n: n, err: err}
	}()
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	select {
	case result := <-resultCh:
		if result.err != nil {
			return result.err
		}
		if result.n != len(data) {
			return io.ErrShortWrite
		}
		return nil
	case <-timer.C:
		_ = writer.Close()
		return fmt.Errorf("app-server stdin write timed out after %s", timeout)
	}
}

func (r *controller) readChild(reader io.Reader) {
	defer close(r.readDone)
	scanner := bufio.NewScanner(reader)
	scanner.Buffer(make([]byte, 64*1024), maxRPCLineBytes)
	for scanner.Scan() {
		raw := append([]byte(nil), scanner.Bytes()...)
		r.writeEventLine(append(raw, '\n'))
		var message rpcEnvelope
		if err := json.Unmarshal(raw, &message); err != nil {
			r.tracef("[warn] invalid provider JSON: %v", err)
			continue
		}
		if message.Method != "" {
			r.handleServerMessage(message)
			continue
		}
		if id, ok := rpcID(message.ID); ok {
			r.pendingMu.Lock()
			ch := r.pending[id]
			r.pendingMu.Unlock()
			if ch != nil {
				ch <- message
			}
		}
	}
	if err := scanner.Err(); err != nil {
		r.fail(fmt.Errorf("read provider output: %w", err))
		return
	}
	if r.cfg.Idle {
		r.endSession("failed", "provider output closed while the idle session was expected to remain available")
	} else if !terminalStatus(r.store.snapshot().Status) {
		r.fail(errors.New("provider output closed before turn completed"))
	}
	r.sessionOnce.Do(func() { close(r.sessionDone) })
}

func rpcID(raw json.RawMessage) (string, bool) {
	if len(raw) == 0 {
		return "", false
	}
	var value any
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	if err := decoder.Decode(&value); err != nil {
		return "", false
	}
	switch typed := value.(type) {
	case string:
		return typed, typed != ""
	case json.Number:
		if _, err := strconv.ParseInt(typed.String(), 10, 64); err != nil {
			return "", false
		}
		return typed.String(), true
	default:
		return "", false
	}
}

func (r *controller) handleServerMessage(message rpcEnvelope) {
	if len(message.ID) > 0 {
		r.rejectServerRequest(message)
		return
	}
	switch message.Method {
	case "turn/started":
		var params struct {
			ThreadID string `json:"threadId"`
			Turn     struct {
				ID string `json:"id"`
			} `json:"turn"`
		}
		_ = json.Unmarshal(message.Params, &params)
		if !r.isRootTurnLifecycle(params.ThreadID, params.Turn.ID) {
			r.tracef("[turn] nested started thread=%s turn=%s", params.ThreadID, params.Turn.ID)
			return
		}
		r.turnMu.Lock()
		err := r.store.update(func(state *runState) {
			state.TurnID = params.Turn.ID
			if !r.turnEnded && !r.sessionEnded {
				state.Status = "active"
			}
		})
		r.turnMu.Unlock()
		if err != nil {
			r.stopChild.Store(true)
			r.fail(fmt.Errorf("persist started turn: %w", err))
			terminateProcessTree(r.child, false)
			return
		}
		r.tracef("[turn] started %s", params.Turn.ID)
	case "turn/completed":
		r.handleTurnCompleted(message.Params)
	case "item/started", "item/updated", "item/completed":
		r.handleItem(message.Method, message.Params)
	case "error":
		var params struct {
			Error struct {
				Message string `json:"message"`
			} `json:"error"`
		}
		_ = json.Unmarshal(message.Params, &params)
		r.tracef("[warn] %s", params.Error.Message)
	case "thread/tokenUsage/updated":
		r.handleTokenUsage(message.Params)
	}
}

func (r *controller) handleTokenUsage(raw json.RawMessage) {
	var params struct {
		ThreadID   string `json:"threadId"`
		TokenUsage struct {
			Total struct {
				TotalTokens       int64 `json:"totalTokens"`
				InputTokens       int64 `json:"inputTokens"`
				CachedInputTokens int64 `json:"cachedInputTokens"`
				OutputTokens      int64 `json:"outputTokens"`
			} `json:"total"`
			Last *struct {
				TotalTokens *int64 `json:"totalTokens"`
			} `json:"last"`
			ModelContextWindow int64 `json:"modelContextWindow"`
			ContextWindow      int64 `json:"contextWindow"`
		} `json:"tokenUsage"`
		CostUSD float64 `json:"costUsd"`
	}
	if err := json.Unmarshal(raw, &params); err != nil {
		r.tracef("[warn] invalid token usage payload: %v", err)
		return
	}
	state := r.store.snapshot()
	if params.ThreadID != "" && state.ThreadID != "" && params.ThreadID != state.ThreadID {
		return
	}
	total := params.TokenUsage.Total
	contextWindow := params.TokenUsage.ModelContextWindow
	if contextWindow == 0 {
		contextWindow = params.TokenUsage.ContextWindow
	}
	if total.TotalTokens == 0 && total.InputTokens == 0 && total.OutputTokens == 0 && params.CostUSD == 0 && params.TokenUsage.Last == nil {
		return
	}
	usage := &tokenUsage{
		InputTokens:       total.InputTokens,
		CachedInputTokens: total.CachedInputTokens,
		OutputTokens:      total.OutputTokens,
		TotalTokens:       total.TotalTokens,
		ContextWindow:     contextWindow,
		CostUSD:           params.CostUSD,
	}
	if last := params.TokenUsage.Last; last != nil && last.TotalTokens != nil && *last.TotalTokens >= 0 {
		usage.ContextTokens = last.TotalTokens
	}
	if usage.CostUSD == 0 && state.TokenUsage != nil {
		usage.CostUSD = state.TokenUsage.CostUSD
	}
	if usage.ContextWindow == 0 && state.TokenUsage != nil {
		usage.ContextWindow = state.TokenUsage.ContextWindow
	}
	if err := r.store.update(func(current *runState) { current.TokenUsage = usage }); err != nil {
		r.tracef("[warn] persist token usage: %v", err)
		return
	}
	if usage.CostUSD > 0 {
		r.tracef("[usage] in=%d cached=%d out=%d total=%d cost=$%.4f", usage.InputTokens, usage.CachedInputTokens, usage.OutputTokens, usage.TotalTokens, usage.CostUSD)
	} else {
		r.tracef("[usage] in=%d cached=%d out=%d total=%d", usage.InputTokens, usage.CachedInputTokens, usage.OutputTokens, usage.TotalTokens)
	}
}

func (r *controller) rejectServerRequest(message rpcEnvelope) {
	var id any
	if err := json.Unmarshal(message.ID, &id); err != nil {
		return
	}
	r.tracef("[warn] unsupported server request %s", message.Method)
	_ = r.writeRPC(map[string]any{
		"id": id,
		"error": map[string]any{
			"code":    -32601,
			"message": "Ruddr cannot answer this interactive request; run with approvalPolicy=never",
		},
	})
}

func (r *controller) handleTurnCompleted(raw json.RawMessage) {
	var params struct {
		ThreadID string `json:"threadId"`
		Turn     struct {
			ID     string    `json:"id"`
			Status string    `json:"status"`
			Error  *rpcError `json:"error"`
		} `json:"turn"`
	}
	_ = json.Unmarshal(raw, &params)
	if !r.isRootTurnLifecycle(params.ThreadID, params.Turn.ID) {
		r.tracef("[turn] nested completed thread=%s turn=%s", params.ThreadID, params.Turn.ID)
		return
	}
	status := params.Turn.Status
	if status == "inProgress" || status == "" {
		status = "failed"
	}
	errText := ""
	if params.Turn.Error != nil {
		errText = params.Turn.Error.Message
	}
	r.finishTurn(status, errText)
}

func (r *controller) isRootTurnLifecycle(threadID, turnID string) bool {
	state := r.store.snapshot()
	if threadID != "" && threadID != state.ThreadID {
		return false
	}
	if turnID == "" {
		return false
	}
	return state.TurnID == "" || turnID == state.TurnID
}

func (r *controller) handleItem(method string, raw json.RawMessage) {
	var params struct {
		Item map[string]any `json:"item"`
	}
	if err := json.Unmarshal(raw, &params); err != nil {
		return
	}
	itemType, _ := params.Item["type"].(string)
	status, _ := params.Item["status"].(string)
	if method == "item/updated" {
		return
	}
	prefix := "[item]"
	if method == "item/started" {
		prefix = "[in_progress]"
	} else if status == "failed" {
		prefix = "[failed]"
	} else {
		prefix = "[completed]"
	}
	switch itemType {
	case "commandExecution":
		command, _ := params.Item["command"].(string)
		r.tracef("%s $ %s", prefix, oneLine(command, 240))
	case "fileChange":
		command, _ := params.Item["command"].(string)
		if command == "" {
			command = "file changes"
		}
		r.tracef("%s %s", prefix, oneLine(command, 240))
	case "webSearch", "toolCall":
		command, _ := params.Item["command"].(string)
		if command == "" {
			command, _ = params.Item["toolName"].(string)
		}
		r.tracef("%s %s", prefix, oneLine(command, 240))
	case "reasoning":
		if method == "item/completed" {
			r.tracef("[think] %s", oneLine(flattenStrings(params.Item["summary"]), 240))
		}
	case "agentMessage":
		if method == "item/completed" {
			text, _ := params.Item["text"].(string)
			if text != "" {
				if err := r.recordAgentMessage(text); err != nil {
					r.stopChild.Store(true)
					r.fail(fmt.Errorf("persist agent output: %w", err))
					terminateProcessTree(r.child, false)
					return
				}
				r.tracef("[say] %s", singleLine(text))
			}
		}
	default:
		if method == "item/completed" && itemType != "userMessage" {
			r.tracef("%s %s", prefix, itemType)
		}
	}
}

// finishTurn ends the open turn with a terminal status. Returns false when no
// turn was open (the terminal state was not persisted).
func (r *controller) finishTurn(status, errText string) bool {
	r.turnMu.Lock()
	defer r.turnMu.Unlock()
	if r.turnEnded || r.sessionEnded {
		return false
	}
	r.turnEnded = true
	r.lastTurn = status
	turnDone := r.turnDone
	r.persistTerminal(status, errText)
	close(turnDone)
	return true
}

// endSession ends the whole run: it terminates any open turn, persists the
// terminal status even when the session was idle, and releases every waiter.
func (r *controller) endSession(status, errText string) {
	r.turnMu.Lock()
	if r.sessionEnded {
		r.turnMu.Unlock()
		return
	}
	r.sessionEnded = true
	turnWasOpen := !r.turnEnded
	if turnWasOpen {
		r.turnEnded = true
		r.lastTurn = status
	}
	turnDone := r.turnDone
	r.persistTerminal(status, errText)
	if turnWasOpen {
		close(turnDone)
	}
	r.turnMu.Unlock()
	r.sessionOnce.Do(func() { close(r.sessionDone) })
}

// endSessionIfTurnOpen claims an active turn and ends its session atomically.
// It returns false when another lifecycle event settled the turn first.
func (r *controller) endSessionIfTurnOpen(status, errText string) bool {
	r.turnMu.Lock()
	if r.turnEnded || r.sessionEnded {
		r.turnMu.Unlock()
		return false
	}
	r.sessionEnded = true
	r.turnEnded = true
	r.lastTurn = status
	turnDone := r.turnDone
	r.persistTerminal(status, errText)
	close(turnDone)
	r.turnMu.Unlock()
	r.sessionOnce.Do(func() { close(r.sessionDone) })
	return true
}

func (r *controller) cancelSession() {
	r.cancelOnce.Do(func() {
		r.stopChild.Store(true)
		r.tracef("[interrupt] controller context canceled")
		r.endSession("interrupted", "")
		terminateProcessTree(r.child, false)
	})
}

func (r *controller) persistTerminal(status, errText string) {
	if errText != "" {
		r.appendResultError(errText)
		r.tracef("[error] %s", oneLine(errText, 500))
	}
	if err := r.store.update(func(state *runState) {
		state.Status = status
		state.Error = redactedStateError(status, errText)
		state.CompletedAt = time.Now().UTC()
		// A completed turn is not a completed idle session. Never expose a
		// terminal status to waiters or deletion while the session stays open.
		if r.cfg.Idle && !r.sessionEnded {
			state.Status = "idle"
			state.Error = ""
			state.CompletedAt = time.Time{}
		}
	}); err != nil {
		persistErr := fmt.Sprintf("persist terminal state: %v", err)
		r.appendResultError(persistErr)
		r.tracef("[error] %s", oneLine(persistErr, 500))
	}
	r.tracef("[turn] %s", status)
}

func (r *controller) writeEventLine(line []byte) {
	_ = r.appendEventLine(line)
}

func (r *controller) appendEventLine(line []byte) error {
	r.eventsMu.Lock()
	defer r.eventsMu.Unlock()
	if r.events == nil {
		return errors.New("events log is not open")
	}
	_, err := r.events.Write(line)
	return err
}

func (r *controller) recordPromptAttempt(id, text string) error {
	state := r.store.snapshot()
	raw, err := json.Marshal(map[string]any{
		"method": "item/completed",
		"params": map[string]any{
			"threadId": state.ThreadID,
			"item": map[string]any{
				"id":     id,
				"type":   "userMessage",
				"text":   text,
				"origin": "ruddr",
				"status": "pending",
			},
		},
	})
	if err != nil {
		return err
	}
	return r.appendEventLine(append(raw, '\n'))
}

func (r *controller) recordPromptDecision(id, decision string) error {
	raw, err := json.Marshal(map[string]any{
		"method": "ruddr/prompt/" + decision,
		"params": map[string]any{"promptId": id},
	})
	if err != nil {
		return err
	}
	return r.appendEventLine(append(raw, '\n'))
}

func (r *controller) appendOutputSeparator() {
	r.outputMu.Lock()
	defer r.outputMu.Unlock()
	if len(r.outputParts) == 0 {
		return
	}
	r.outputParts = append(r.outputParts, "---")
}

func (r *controller) recordAgentMessage(text string) error {
	r.outputMu.Lock()
	defer r.outputMu.Unlock()
	r.outputParts = append(r.outputParts, text)
	content := strings.Join(r.outputParts, "\n\n") + "\n"
	return writePrivateFile(r.store.snapshot().OutputPath, []byte(content))
}

func (r *controller) privateResultError() string {
	r.resultMu.Lock()
	defer r.resultMu.Unlock()
	return r.resultError
}

func (r *controller) appendResultError(errText string) {
	r.resultMu.Lock()
	defer r.resultMu.Unlock()
	if r.resultError == "" {
		r.resultError = errText
		return
	}
	r.resultError += "; " + errText
}

func redactedStateError(status, errText string) string {
	if errText == "" {
		return ""
	}
	switch status {
	case "interrupted":
		return "turn interrupted; see trace.log and provider.stderr.log"
	default:
		return "turn failed; see trace.log and provider.stderr.log"
	}
}

// fail ends the whole session as failed; every caller treats its error as
// fatal for the run, not just the current turn.
func (r *controller) fail(err error) {
	if err == nil {
		return
	}
	r.endSession("failed", err.Error())
}

func (r *controller) shutdownChild() {
	if r.stopChild.Load() {
		terminateProcessTree(r.child, false)
	}
	if r.childIn != nil {
		_ = r.childIn.Close()
	}
	select {
	case <-r.waitCh:
	case <-time.After(3 * time.Second):
		if r.child != nil && r.child.Process != nil {
			terminateProcessTree(r.child, true)
		}
		<-r.waitCh
	}
	if r.stopChild.Load() {
		// The parent exiting does not prove that its descendants honored TERM.
		terminateProcessTree(r.child, true)
	}
	if r.readDone != nil {
		select {
		case <-r.readDone:
		case <-time.After(time.Second):
		}
	}
}

func (r *controller) closeControlServer() {
	state := r.store.snapshot()
	if r.listener != nil {
		_ = r.listener.Close()
		_ = os.Remove(state.SocketPath)
	}
	if state.SocketDir != "" {
		_ = os.Remove(state.SocketDir)
	}
}

func (r *controller) tracef(format string, args ...any) {
	r.traceMu.Lock()
	defer r.traceMu.Unlock()
	if r.trace == nil {
		return
	}
	stamp := time.Now().UTC().Format(time.RFC3339)
	_, _ = fmt.Fprintf(r.trace, "%s %s\n", stamp, fmt.Sprintf(format, args...))
}

func oneLine(value string, limit int) string {
	value = singleLine(value)
	if len(value) <= limit {
		return value
	}
	return value[:limit] + "…"
}

func singleLine(value string) string {
	return strings.Join(strings.Fields(value), " ")
}

func flattenStrings(value any) string {
	switch typed := value.(type) {
	case string:
		return typed
	case []any:
		parts := make([]string, 0, len(typed))
		for _, item := range typed {
			if part := flattenStrings(item); part != "" {
				parts = append(parts, part)
			}
		}
		return strings.Join(parts, " ")
	case map[string]any:
		for _, key := range []string{"text", "content"} {
			if part := flattenStrings(typed[key]); part != "" {
				return part
			}
		}
	}
	return ""
}
