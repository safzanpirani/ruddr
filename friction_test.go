package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"
)

func writeModelsJSON(t *testing.T, body string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "models.json")
	if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv(modelsFileEnvironment, path)
	return path
}

func TestCodexModelConfigAndConfigFlagReachTheChildCommand(t *testing.T) {
	writeModelsJSON(t, `{"models":[{"provider":"codex","id":"gpt-6-sol","config":{"features.b":"false","features.a":"1"}}]}`)
	cfg := runConfig{Provider: providerCodex, Model: "gpt-6-sol", CodexConfig: []string{"model_verbosity=low"}}
	if err := configureProviderDefaults(&cfg, nil); err != nil {
		t.Fatal(err)
	}
	want := []string{"codex", "app-server", "--listen", "stdio://",
		"-c", "features.a=1", "-c", "features.b=false", "-c", "model_verbosity=low"}
	if !reflect.DeepEqual(cfg.ChildCommand, want) {
		t.Fatalf("child command = %q, want %q", cfg.ChildCommand, want)
	}

	other := runConfig{Provider: providerCodex, Model: "gpt-6-astra"}
	if err := configureProviderDefaults(&other, nil); err != nil {
		t.Fatal(err)
	}
	if len(other.ChildCommand) != 4 {
		t.Fatalf("a model without config got overrides: %q", other.ChildCommand)
	}
}

func TestConfigFlagIsRejectedWhereItCannotApply(t *testing.T) {
	writeModelsJSON(t, `{"models":[]}`)
	custom := runConfig{Provider: providerCodex, CodexConfig: []string{"a=b"}}
	if err := configureProviderDefaults(&custom, []string{"broker", "app-server-bridge"}); err == nil || !strings.Contains(err.Error(), "command after --") {
		t.Fatalf("custom child command with --config = %v", err)
	}
	claude := runConfig{Provider: providerClaude, CodexConfig: []string{"a=b"}}
	if err := configureProviderDefaults(&claude, nil); err == nil || !strings.Contains(err.Error(), "only to --provider codex") {
		t.Fatalf("claude with --config = %v", err)
	}
	var overrides configOverrides
	if err := overrides.Set("no-equals-sign"); err == nil {
		t.Fatal("config override without = was accepted")
	}
}

func TestModelsFileRejectsConfigOnNonCodexModels(t *testing.T) {
	writeModelsJSON(t, `{"models":[{"provider":"claude","id":"claude-opus-5-5","config":{"a":"b"}}]}`)
	if _, err := loadModelCatalog(); err == nil || !strings.Contains(err.Error(), "only to codex") {
		t.Fatalf("claude config error = %v", err)
	}
}

func TestModelsAddSetsAndUnsetsConfig(t *testing.T) {
	writeModelsJSON(t, `{"models":[]}`)
	if _, err := captureStdout(func() error {
		return modelsCommand([]string{"add", "codex", "gpt-6-sol", "--config", "features.x=false", "--config", "y=1"})
	}); err != nil {
		t.Fatal(err)
	}
	overrides, err := modelCodexConfig("gpt-6-sol")
	if err != nil || !reflect.DeepEqual(overrides, []string{"features.x=false", "y=1"}) {
		t.Fatalf("after add: %q, %v", overrides, err)
	}
	if _, err := captureStdout(func() error {
		return modelsCommand([]string{"add", "codex", "gpt-6-sol", "--unset-config", "y"})
	}); err != nil {
		t.Fatal(err)
	}
	if overrides, _ := modelCodexConfig("gpt-6-sol"); !reflect.DeepEqual(overrides, []string{"features.x=false"}) {
		t.Fatalf("after unset: %q", overrides)
	}
	err = modelsCommand([]string{"add", "claude", "claude-opus-5-5", "--config", "a=b"})
	if exitCodeFor(err) != exitUsage {
		t.Fatalf("claude --config = %v (exit %d)", err, exitCodeFor(err))
	}
}

func TestDefaultStateDirIsFreshAndIgnoredByGit(t *testing.T) {
	cwd := t.TempDir()
	now := time.Date(2026, 9, 28, 13, 5, 9, 0, time.UTC)
	first, err := defaultStateDir(cwd, now)
	if err != nil {
		t.Fatal(err)
	}
	second, err := defaultStateDir(cwd, now)
	if err != nil {
		t.Fatal(err)
	}
	base := filepath.Join(cwd, ".scratch", "ruddr")
	if filepath.Dir(first) != base || !strings.HasPrefix(filepath.Base(first), "20260928-130509-") || first == second {
		t.Fatalf("state dirs = %s, %s", first, second)
	}
	ignore, err := os.ReadFile(filepath.Join(base, ".gitignore"))
	if err != nil || string(ignore) != "*\n" {
		t.Fatalf(".gitignore = %q, %v", ignore, err)
	}
	if info, err := os.Stat(base); err != nil || info.Mode().Perm() != 0o700 {
		t.Fatalf("base dir mode = %v, %v", info.Mode(), err)
	}
}

func TestSetFlagValueAddsTheFlagBeforeTheChildCommand(t *testing.T) {
	got := setFlagValue([]string{"--prompt-file", "p.md", "--", "broker", "--state-dir", "x"}, "state-dir", "/s")
	want := []string{"--prompt-file", "p.md", "--state-dir", "/s", "--", "broker", "--state-dir", "x"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("setFlagValue = %q", got)
	}
	got = setFlagValue([]string{"--state-dir=old"}, "state-dir", "/s")
	if !reflect.DeepEqual(got, []string{"--state-dir=/s"}) {
		t.Fatalf("setFlagValue replace = %q", got)
	}
}

func TestExitCodes(t *testing.T) {
	cases := []struct {
		err  error
		want int
	}{
		{nil, 0},
		{flag.ErrHelp, 0},
		{usageError(flag.ErrHelp), 0},
		{errors.New("plain"), exitFailed},
		{usageError(errors.New("bad flag")), exitUsage},
		{fmt.Errorf("wrapped: %w", withExitCode(exitStale, errors.New("dead"))), exitStale},
	}
	for _, c := range cases {
		if got := exitCodeFor(c.err); got != c.want {
			t.Errorf("exitCodeFor(%v) = %d, want %d", c.err, got, c.want)
		}
	}
	if code := exitCodeFor(statusCommand([]string{"--bogus"})); code != exitUsage {
		t.Errorf("unknown flag exit = %d", code)
	}
	if code := exitCodeFor(statusCommand(nil)); code != exitUsage {
		t.Errorf("missing --state-dir exit = %d", code)
	}
}

func TestWaitExitCodesSeparateTimeoutStaleAndFailure(t *testing.T) {
	stateDir := t.TempDir()
	writeWaitState(t, stateDir, runState{Version: 2, PID: 1, Status: "active"})
	err := waitForRunState(stateDir, time.Now().Add(5*time.Millisecond), false, func(int) bool { return true }, time.Millisecond)
	if exitCodeFor(err) != exitRunning {
		t.Errorf("timed-out wait = %v (exit %d)", err, exitCodeFor(err))
	}
	err = waitForRunState(stateDir, time.Time{}, false, func(int) bool { return false }, time.Millisecond)
	if exitCodeFor(err) != exitStale {
		t.Errorf("stale wait = %v (exit %d)", err, exitCodeFor(err))
	}
	writeWaitState(t, stateDir, runState{Version: 2, PID: 1, Status: "failed", Error: "turn failed"})
	err = waitForRunState(stateDir, time.Time{}, false, func(int) bool { return true }, time.Millisecond)
	if exitCodeFor(err) != exitFailed {
		t.Errorf("failed wait = %v (exit %d)", err, exitCodeFor(err))
	}

	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "busy"), runState{Version: 2, PID: 1, Status: "active"})
	refs, _ := discoverRuns(root)
	err = waitForRuns(&strings.Builder{}, refs, time.Now().Add(5*time.Millisecond), waitOptions{}, func(int) bool { return true }, time.Millisecond)
	if exitCodeFor(err) != exitRunning {
		t.Errorf("group timeout = %v (exit %d)", err, exitCodeFor(err))
	}
	makeGroupRun(t, filepath.Join(root, "failed"), runState{Version: 2, PID: 2, Status: "failed"})
	err = waitForRuns(&strings.Builder{}, refs, time.Time{}, waitOptions{}, func(int) bool { return false }, time.Millisecond)
	if exitCodeFor(err) != exitStale {
		t.Errorf("group with a dead run = %v (exit %d)", err, exitCodeFor(err))
	}
}

func TestResultExitCodes(t *testing.T) {
	root := t.TempDir()
	makeGroupRun(t, filepath.Join(root, "failed"), runState{Version: 2, PID: 1, Status: "failed", Error: "turn failed"})
	if code := exitCodeFor(resultCommandQuiet(t, "--root", root)); code != exitFailed {
		t.Errorf("failed run exit = %d", code)
	}
	// processAlive(os.Getpid()) is true, so this run reads as still active.
	makeGroupRun(t, filepath.Join(root, "busy"), runState{Version: 2, PID: os.Getpid(), Status: "active"})
	if code := exitCodeFor(resultCommandQuiet(t, "--root", root)); code != exitRunning {
		t.Errorf("running run exit = %d", code)
	}
}

func resultCommandQuiet(t *testing.T, args ...string) error {
	t.Helper()
	_, err := captureStdout(func() error { return resultCommand(args) })
	return err
}
