package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

const (
	stdinPromptFileName  = "prompt.md"
	launchStderrFileName = "launch.stderr.log"
	detachStartupWindow  = 15 * time.Second
)

// detachedRunCommand builds the background controller process. Tests replace
// it with a helper process.
var detachedRunCommand = func(args []string) (*exec.Cmd, error) {
	executable, err := os.Executable()
	if err != nil {
		return nil, fmt.Errorf("locate Ruddr executable: %w", err)
	}
	return exec.Command(executable, append([]string{"run"}, args...)...), nil
}

// writeStdinPrompt stores the prompt read from stdin as a private file inside
// the state directory, so the controller reads it like any other prompt file.
func writeStdinPrompt(stateDir string, stdin io.Reader) (string, error) {
	stateDir, err := filepath.Abs(stateDir)
	if err != nil {
		return "", err
	}
	if err := os.MkdirAll(stateDir, 0o700); err != nil {
		return "", err
	}
	if err := os.Chmod(stateDir, 0o700); err != nil {
		return "", err
	}
	if existing, err := readState(stateDir); err == nil {
		return "", fmt.Errorf("state directory already contains a Ruddr run with status %s; use a new --state-dir", existing.Status)
	}
	raw, err := io.ReadAll(stdin)
	if err != nil {
		return "", fmt.Errorf("read prompt from stdin: %w", err)
	}
	if strings.TrimSpace(string(raw)) == "" {
		return "", errors.New("prompt from stdin is empty")
	}
	promptPath := filepath.Join(stateDir, stdinPromptFileName)
	file, err := os.OpenFile(promptPath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		if errors.Is(err, os.ErrExist) {
			return "", fmt.Errorf("state directory already contains %s; use a new --state-dir", stdinPromptFileName)
		}
		return "", err
	}
	if _, err := file.Write(raw); err != nil {
		file.Close()
		return "", err
	}
	if err := file.Close(); err != nil {
		return "", err
	}
	return promptPath, nil
}

// startDetachedRun launches the controller in its own session and returns once
// it reports a live or finished state. A controller that is still starting
// when the window closes is left running: retrying could create a second run.
func startDetachedRun(stateDir string, childArgs []string, window time.Duration) (runState, error) {
	stateDir, err := filepath.Abs(stateDir)
	if err != nil {
		return runState{}, err
	}
	if err := os.MkdirAll(stateDir, 0o700); err != nil {
		return runState{}, err
	}
	// Append: the TUI creates this log before launching `run --detach` and
	// captures the launcher's own stderr in it. The controller still refuses
	// a state directory that holds an earlier run.
	stderrPath := filepath.Join(stateDir, launchStderrFileName)
	stderr, err := os.OpenFile(stderrPath, os.O_WRONLY|os.O_CREATE|os.O_APPEND, 0o600)
	if err != nil {
		return runState{}, err
	}
	start := func(breakaway bool) (*exec.Cmd, error) {
		cmd, err := detachedRunCommand(childArgs)
		if err != nil {
			return nil, err
		}
		cmd.Stdin = nil
		cmd.Stdout = nil
		cmd.Stderr = stderr
		configureDetachedProcess(cmd, breakaway)
		return cmd, cmd.Start()
	}
	cmd, startErr := start(detachSupportsBreakaway)
	if startErr != nil && detachSupportsBreakaway {
		// The launching job forbids breakaway. The run then survives a closed
		// console but not the end of an SSH session.
		cmd, startErr = start(false)
	}
	// The child owns its duplicate of the log handle now.
	stderr.Close()
	if startErr != nil {
		return runState{}, fmt.Errorf("start detached run: %w", startErr)
	}
	exited := make(chan struct{})
	go func() {
		_ = cmd.Wait()
		close(exited)
	}()
	return waitForDetachedStartup(stateDir, stderrPath, exited, time.Now().Add(window), 25*time.Millisecond)
}

func waitForDetachedStartup(stateDir, stderrPath string, exited <-chan struct{}, deadline time.Time, tick time.Duration) (runState, error) {
	for {
		childExited := false
		select {
		case <-exited:
			childExited = true
		default:
		}
		state, err := readState(stateDir)
		if err == nil {
			switch state.Status {
			case "active", "idle", "completed":
				return state, nil
			case "failed", "interrupted":
				return state, detachedStartupError(stateDir, stderrPath, state.Error)
			}
		}
		if childExited {
			// Re-read once: the controller may have persisted state just
			// before exiting.
			if state, err := readState(stateDir); err == nil && state.Status == "completed" {
				return state, nil
			}
			return runState{}, detachedStartupError(stateDir, stderrPath, "")
		}
		if !time.Now().Before(deadline) {
			if err != nil {
				return runState{StateDir: stateDir, Status: "starting"}, nil
			}
			return state, nil
		}
		time.Sleep(tick)
	}
}

func detachedStartupError(stateDir, stderrPath, stateError string) error {
	raw, _ := os.ReadFile(stderrPath)
	diagnostic := strings.TrimSpace(string(raw))
	if len(diagnostic) > 4096 {
		diagnostic = diagnostic[len(diagnostic)-4096:]
	}
	if diagnostic == "" {
		diagnostic = stateError
	}
	if diagnostic == "" {
		diagnostic = "run exited during startup"
	}
	return fmt.Errorf("%s (state dir %s)", diagnostic, stateDir)
}

// detachedChildArgs drops --detach and points --prompt-file at the stored
// prompt, keeping every other argument, including a custom command after --.
func detachedChildArgs(args []string, promptFile string) []string {
	flagArgs, tail := args, []string(nil)
	if marker := indexOf(args, "--"); marker >= 0 {
		flagArgs, tail = args[:marker], args[marker:]
	}
	var out []string
	for _, arg := range flagArgs {
		if name, _, _ := splitFlagArg(arg); name == "detach" {
			continue
		}
		out = append(out, arg)
	}
	out, _, _ = replaceFlagValue(out, "prompt-file", promptFile)
	return append(out, tail...)
}

// splitFlagArg recognizes -name, --name, -name=value, and --name=value.
func splitFlagArg(arg string) (name, value string, hasValue bool) {
	if !strings.HasPrefix(arg, "-") || arg == "-" || arg == "--" {
		return "", "", false
	}
	trimmed := strings.TrimPrefix(strings.TrimPrefix(arg, "-"), "-")
	if index := strings.IndexByte(trimmed, '='); index >= 0 {
		return trimmed[:index], trimmed[index+1:], true
	}
	return trimmed, "", false
}

// replaceFlagValue rewrites the value of a string flag in flag-package syntax
// and reports the previous value. Arguments after a -- marker are untouched.
func replaceFlagValue(args []string, name, replacement string) ([]string, string, bool) {
	out := append([]string(nil), args...)
	for index := 0; index < len(out); index++ {
		if out[index] == "--" {
			break
		}
		flagName, value, hasValue := splitFlagArg(out[index])
		if flagName != name {
			continue
		}
		if hasValue {
			out[index] = "--" + name + "=" + replacement
			return out, value, true
		}
		if index+1 < len(out) {
			previous := out[index+1]
			out[index+1] = replacement
			return out, previous, true
		}
		return out, "", false
	}
	return out, "", false
}

func hasFlag(args []string, name string) bool {
	for _, arg := range args {
		if arg == "--" {
			return false
		}
		if flagName, _, _ := splitFlagArg(arg); flagName == name {
			return true
		}
	}
	return false
}
