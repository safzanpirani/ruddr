package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
)

const (
	tuiEntryEnvironment         = "RUDDR_TUI_ENTRY"
	previousTUIEntryEnvironment = "RUDDER_TUI_ENTRY"
	legacyTUIEntryEnvironment   = "CODEX_RUDDER_TUI_ENTRY"
)

func tuiCommand(args []string) error {
	if len(args) == 1 && (args[0] == "--help" || args[0] == "-h" || args[0] == "help") {
		printTUIUsage()
		return nil
	}
	stdinInfo, stdinErr := os.Stdin.Stat()
	stdoutInfo, stdoutErr := os.Stdout.Stat()
	if stdinErr != nil || stdoutErr != nil || stdinInfo.Mode()&os.ModeCharDevice == 0 || stdoutInfo.Mode()&os.ModeCharDevice == 0 {
		return errors.New("the TUI requires an interactive terminal")
	}
	if useRustTUI(args) {
		return runRustTUI(args)
	}
	bunPath, err := exec.LookPath("bun")
	if err != nil {
		return errors.New("the optional TUI requires Bun 1.4 or newer; install Bun and run again")
	}
	registerRunningRuddrRuns()
	entryPath, err := findTUIEntry()
	if err != nil {
		return err
	}
	ruddrPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate Ruddr executable: %w", err)
	}
	cmd := newTUIProcess(bunPath, entryPath, ruddrPath, args)
	if latest, ok := availableUpdate(); ok {
		cmd.Env = append(os.Environ(), updateAvailableEnvironment+"="+latest)
	}
	// Refresh the cached release check while the TUI runs; the next launch
	// shows the result.
	go refreshUpdateCheck(context.Background())
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("TUI exited: %w", err)
	}
	return nil
}

// useRustTUI selects the experimental ratatui front end.
func useRustTUI(args []string) bool {
	for _, arg := range args {
		if arg == "--rs" {
			return true
		}
	}
	return os.Getenv("RUDDR_TUI_IMPL") == "rust"
}

func runRustTUI(args []string) error {
	binary, err := findRustTUI()
	if err != nil {
		return err
	}
	registerRunningRuddrRuns()
	ruddrPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate Ruddr executable: %w", err)
	}
	cmd := exec.Command(binary, append([]string{"--ruddr", ruddrPath}, args...)...)
	if latest, ok := availableUpdate(); ok {
		cmd.Env = append(os.Environ(), updateAvailableEnvironment+"="+latest)
	}
	go refreshUpdateCheck(context.Background())
	cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("TUI exited: %w", err)
	}
	return nil
}

func findRustTUI() (string, error) {
	var candidates []string
	if configured := os.Getenv("RUDDR_TUI_BIN"); configured != "" {
		candidates = append(candidates, configured)
	}
	if executable, err := os.Executable(); err == nil {
		dir := filepath.Dir(executable)
		candidates = append(candidates,
			filepath.Join(dir, "ruddr-tui"),
			filepath.Join(dir, "tui-rs", "target", "release", "ruddr-tui"))
	}
	if cwd, err := os.Getwd(); err == nil {
		candidates = append(candidates, filepath.Join(cwd, "tui-rs", "target", "release", "ruddr-tui"))
	}
	if path, err := exec.LookPath("ruddr-tui"); err == nil {
		candidates = append(candidates, path)
	}
	for _, candidate := range candidates {
		if info, err := os.Stat(candidate); err == nil && !info.IsDir() {
			return candidate, nil
		}
	}
	return "", errors.New("cannot locate the ruddr-tui binary; build it with `cargo build --release` in tui-rs or set RUDDR_TUI_BIN")
}

func newTUIProcess(bunPath, entryPath, ruddrPath string, args []string) *exec.Cmd {
	childArgs := []string{"run", entryPath, "--ruddr", ruddrPath}
	childArgs = append(childArgs, args...)
	return exec.Command(bunPath, childArgs...)
}

func printTUIUsage() {
	name := filepath.Base(os.Args[0])
	fmt.Fprintf(os.Stderr, `Ruddr live sessions TUI

Usage:
  %s tui [--root DIR]... [--state-dir DIR]... [--all] [--interval 500ms] [--theme NAME] [--beta] [--mobile] [--rs]

Shows live runs first, then every finished run from the global registry plus
.scratch below the current directory. --root and --state-dir may be repeated;
--all is accepted for compatibility and has no effect.
The refresh interval accepts milliseconds or seconds and must be at least 100ms.
Press t inside the TUI to preview and save a theme. --theme overrides the saved
theme for one launch; RUDDR_TUI_THEME provides the same environment override.
--beta enables the chat-first layout; RUDDR_TUI_BETA=1 provides the same
override. The default layout keeps the sessions dashboard visible. Terminals
64 columns wide or narrower get the single-column mobile layout with a tappable
action bar; --mobile or RUDDR_TUI_MOBILE=1 forces it, and mobileWidthThreshold
in tui.json changes the width.
--rs (or RUDDR_TUI_IMPL=rust) launches the experimental ratatui front end
from tui-rs instead; RUDDR_TUI_BIN points at its binary. It accepts the same
flags, themes, and tui.json settings.
`, name)
}

func findTUIEntry() (string, error) {
	var candidates []string
	for _, environment := range []string{tuiEntryEnvironment, previousTUIEntryEnvironment, legacyTUIEntryEnvironment} {
		if configured := os.Getenv(environment); configured != "" {
			candidates = append(candidates, configured)
		}
	}
	if cwd, err := os.Getwd(); err == nil {
		candidates = append(candidates, filepath.Join(cwd, "tui", "index.ts"))
	}
	if executable, err := os.Executable(); err == nil {
		candidates = appendRuntimeSiblingCandidates(candidates, executable, "tui", "index.ts")
	}
	if dataHome, err := ruddrDataHome(); err == nil {
		for _, name := range installDirectoryNames {
			candidates = append(candidates, filepath.Join(dataHome, name, "tui", "index.ts"))
		}
	}
	for _, candidate := range candidates {
		absolute, err := filepath.Abs(candidate)
		if err != nil {
			continue
		}
		info, err := os.Stat(absolute)
		if err == nil && !info.IsDir() {
			return absolute, nil
		}
	}
	return "", fmt.Errorf("cannot locate tui/index.ts; run the installer or set %s", tuiEntryEnvironment)
}

func ruddrDataHome() (string, error) {
	if configured := os.Getenv("XDG_DATA_HOME"); configured != "" {
		return configured, nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".local", "share"), nil
}
