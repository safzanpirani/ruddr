package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

const stateFileName = "state.json"
const stateClaimFileName = ".ruddr.claim"

// Darwin allows 104 bytes including the trailing NUL; this conservative limit
// also works on Linux and leaves room for platform bookkeeping.
const maxControlSocketPathBytes = 100

type runState struct {
	Version     int         `json:"version"`
	Provider    string      `json:"provider"`
	PID         int         `json:"pid"`
	ChildPID    int         `json:"childPid,omitempty"`
	Status      string      `json:"status"`
	ThreadID    string      `json:"threadId,omitempty"`
	TurnID      string      `json:"turnId,omitempty"`
	Model       string      `json:"model"`
	Effort      string      `json:"effort,omitempty"`
	CWD         string      `json:"cwd"`
	Sandbox     string      `json:"sandbox"`
	StateDir    string      `json:"stateDir"`
	SocketPath  string      `json:"socketPath"`
	SocketDir   string      `json:"socketDir,omitempty"`
	EventsPath  string      `json:"eventsPath"`
	TracePath   string      `json:"tracePath"`
	OutputPath  string      `json:"outputPath"`
	StderrPath  string      `json:"stderrPath"`
	Steers      int         `json:"steers"`
	Idle        bool        `json:"idle,omitempty"`
	Turns       int         `json:"turns,omitempty"`
	TokenUsage  *tokenUsage `json:"tokenUsage,omitempty"`
	StartedAt   time.Time   `json:"startedAt"`
	UpdatedAt   time.Time   `json:"updatedAt"`
	CompletedAt time.Time   `json:"completedAt,omitempty"`
	Error       string      `json:"error,omitempty"`
}

// tokenUsage keeps cumulative counters and the latest context estimate separate.
// Counts and cost are redaction-safe metadata for state.json.
type tokenUsage struct {
	InputTokens       int64   `json:"inputTokens,omitempty"`
	CachedInputTokens int64   `json:"cachedInputTokens,omitempty"`
	OutputTokens      int64   `json:"outputTokens,omitempty"`
	TotalTokens       int64   `json:"totalTokens,omitempty"`
	ContextTokens     *int64  `json:"contextTokens,omitempty"`
	ContextWindow     int64   `json:"contextWindow,omitempty"`
	CostUSD           float64 `json:"costUsd,omitempty"`
}

type stateStore struct {
	mu    sync.Mutex
	path  string
	state runState
}

func newStateStore(cfg runConfig) (*stateStore, error) {
	provider, err := normalizeProvider(cfg.Provider)
	if err != nil {
		return nil, err
	}
	cfg.Provider = provider
	stateDir, err := filepath.Abs(cfg.StateDir)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(stateDir, 0o700); err != nil {
		return nil, err
	}
	if err := os.Chmod(stateDir, 0o700); err != nil {
		return nil, err
	}
	existing, err := readState(stateDir)
	if err == nil {
		return nil, fmt.Errorf("state directory already contains a Ruddr run with status %s; use a new --state-dir", existing.Status)
	}
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("read existing state: %w", err)
	}
	for _, name := range []string{
		stateClaimFileName,
		stateFileName + ".tmp",
		"events.jsonl",
		"trace.log",
		"output.md",
		"output.md.tmp",
		"provider.stderr.log",
		".ruddr.sock",
	} {
		artifactPath := filepath.Join(stateDir, name)
		if _, err := os.Lstat(artifactPath); err == nil {
			return nil, fmt.Errorf("state directory already contains Ruddr artifact %s; use a new --state-dir", name)
		} else if !errors.Is(err, os.ErrNotExist) {
			return nil, fmt.Errorf("inspect state directory artifact %s: %w", name, err)
		}
	}
	now := time.Now().UTC()
	socketPath, socketDir, err := controlSocketLocation(stateDir)
	if err != nil {
		return nil, err
	}
	store := &stateStore{
		path: filepath.Join(stateDir, stateFileName),
		state: runState{
			Version:    2,
			Provider:   cfg.Provider,
			PID:        os.Getpid(),
			Status:     "starting",
			Model:      cfg.Model,
			Effort:     cfg.Effort,
			CWD:        cfg.CWD,
			Sandbox:    cfg.Sandbox,
			Idle:       cfg.Idle,
			StateDir:   stateDir,
			SocketPath: socketPath,
			SocketDir:  socketDir,
			EventsPath: filepath.Join(stateDir, "events.jsonl"),
			TracePath:  filepath.Join(stateDir, "trace.log"),
			OutputPath: filepath.Join(stateDir, "output.md"),
			StderrPath: filepath.Join(stateDir, "provider.stderr.log"),
			StartedAt:  now,
			UpdatedAt:  now,
		},
	}
	if cfg.BeforeStateReserve != nil {
		cfg.BeforeStateReserve()
	}
	if err := persistInitialState(store.path, store.state); err != nil {
		if socketDir != "" {
			_ = os.RemoveAll(socketDir)
		}
		if errors.Is(err, os.ErrExist) {
			return nil, errors.New("state directory was claimed by another Ruddr run; use a new --state-dir")
		}
		return nil, fmt.Errorf("reserve state directory: %w", err)
	}
	if cfg.RegisterRun {
		_ = registerRunStateDir(stateDir)
	}
	return store, nil
}

func (s *stateStore) update(fn func(*runState)) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	next := s.state
	fn(&next)
	next.UpdatedAt = time.Now().UTC()
	if err := persistState(s.path, next); err != nil {
		return err
	}
	s.state = next
	return nil
}

func (s *stateStore) snapshot() runState {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.state
}

func marshalState(state runState) ([]byte, error) {
	raw, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return nil, err
	}
	return append(raw, '\n'), nil
}

func persistInitialState(path string, state runState) error {
	claimPath := filepath.Join(filepath.Dir(path), stateClaimFileName)
	file, err := os.OpenFile(claimPath, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	if err := file.Chmod(0o600); err != nil {
		_ = file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if err := persistState(path, state); err != nil {
		return err
	}
	return nil
}

func persistState(path string, state runState) error {
	raw, err := marshalState(state)
	if err != nil {
		return err
	}
	return writePrivateFile(path, raw)
}

func readState(stateDir string) (runState, error) {
	if stateDir == "" {
		return runState{}, errors.New("--state-dir is required")
	}
	raw, err := os.ReadFile(filepath.Join(stateDir, stateFileName))
	if err != nil {
		return runState{}, err
	}
	var state runState
	if err := json.Unmarshal(raw, &state); err != nil {
		return runState{}, err
	}
	if state.Provider == "" {
		state.Provider = providerCodex
	}
	return state, nil
}

func controlSocketLocation(stateDir string) (string, string, error) {
	insideState := filepath.Join(stateDir, ".ruddr.sock")
	if len([]byte(insideState)) <= maxControlSocketPathBytes {
		return insideState, "", nil
	}

	roots := []string{os.TempDir()}
	if os.TempDir() != "/tmp" {
		roots = append(roots, "/tmp")
	}
	for _, root := range roots {
		privateDir, err := os.MkdirTemp(root, "ruddr-")
		if err != nil {
			continue
		}
		if err := os.Chmod(privateDir, 0o700); err != nil {
			_ = os.RemoveAll(privateDir)
			continue
		}
		path := filepath.Join(privateDir, "control.sock")
		if len([]byte(path)) <= maxControlSocketPathBytes {
			return path, privateDir, nil
		}
		_ = os.RemoveAll(privateDir)
	}
	return "", "", fmt.Errorf("cannot create a private Unix socket path within %d bytes", maxControlSocketPathBytes)
}

func terminalStatus(status string) bool {
	switch status {
	case "completed", "failed", "interrupted":
		return true
	default:
		return false
	}
}

func displayedState(state runState) runState {
	if !terminalStatus(state.Status) && !processAlive(state.PID) {
		state.Status = "stale"
		state.Error = fmt.Sprintf("Ruddr pid %d is not running; persisted state is stale", state.PID)
	}
	return state
}

func writePrivateFile(path string, data []byte) error {
	tmp, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+"-*")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), path)
}

func printJSON(value any) error {
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	return enc.Encode(value)
}

func tailLines(path string, count int) ([]string, error) {
	if count <= 0 {
		return nil, nil
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	ring := make([]string, count)
	total := 0
	scanner := bufio.NewScanner(f)
	scanner.Buffer(make([]byte, 64*1024), 8*1024*1024)
	for scanner.Scan() {
		ring[total%count] = scanner.Text()
		total++
	}
	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("read trace: %w", err)
	}
	if total < count {
		return ring[:total], nil
	}
	lines := make([]string, count)
	start := total % count
	for i := range count {
		lines[i] = ring[(start+i)%count]
	}
	return lines, nil
}
