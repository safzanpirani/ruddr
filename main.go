package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

const version = "0.4.0"

func main() {
	ctx := context.Background()
	stop := func() {}
	if len(os.Args) > 1 && os.Args[1] == "run" {
		ctx, stop = signal.NotifyContext(ctx, os.Interrupt, syscall.SIGTERM)
	}
	defer stop()
	if err := runCLIContext(ctx, os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "ruddr:", err)
		os.Exit(1)
	}
}

func runCLI(args []string) error {
	return runCLIContext(context.Background(), args)
}

func runCLIContext(ctx context.Context, args []string) error {
	if len(args) == 0 {
		printUsage()
		return errors.New("a command is required")
	}
	switch args[0] {
	case "run":
		return runCommandContext(ctx, args[1:])
	case "thread":
		return threadCommand(args[1:])
	case "tui":
		return tuiCommand(args[1:])
	case "steer":
		return steerCommand(args[1:])
	case "prompt":
		return promptCommand(args[1:])
	case "stop":
		return stopCommand(args[1:])
	case "models":
		return modelsCommand(args[1:])
	case "status":
		return statusCommand(args[1:])
	case "peek":
		return peekCommand(args[1:])
	case "interrupt":
		return interruptCommand(args[1:])
	case "wait":
		return waitCommand(args[1:])
	case "update":
		return updateCommand(args[1:])
	case "skill":
		return skillCommand(args[1:])
	case "version", "--version", "-version":
		fmt.Println("ruddr", version)
		refreshUpdateCheck(ctx)
		printUpdateNotice()
		return nil
	case "help", "--help", "-h":
		printUsage()
		return nil
	default:
		printUsage()
		return fmt.Errorf("unknown command %q", args[0])
	}
}

func runCommand(args []string) error {
	return runCommandContext(context.Background(), args)
}

func runCommandContext(ctx context.Context, args []string) error {
	fs := flag.NewFlagSet("run", flag.ContinueOnError)
	cwd, _ := os.Getwd()
	var cfg runConfig
	fs.StringVar(&cfg.Provider, "provider", providerCodex, "provider: codex, claude, opencode, or pi")
	fs.StringVar(&cfg.CWD, "cwd", cwd, "working directory for the provider session")
	fs.StringVar(&cfg.PromptFile, "prompt-file", "", "file containing the initial task")
	fs.StringVar(&cfg.StateDir, "state-dir", "", "directory for state, trace, and output")
	fs.StringVar(&cfg.Model, "model", "", "provider model; Codex defaults to gpt-6-astra")
	fs.StringVar(&cfg.Effort, "effort", "", "reasoning effort override")
	fs.StringVar(&cfg.Sandbox, "sandbox", "workspace-write", "read-only, workspace-write, or danger-full-access")
	fs.StringVar(&cfg.ApprovalPolicy, "approval-policy", "never", "Codex approval policy; adapters require never")
	fs.StringVar(&cfg.ClaudePath, "claude-path", "", "Claude Code executable for --provider claude")
	fs.StringVar(&cfg.OpenCodePath, "opencode-path", "", "OpenCode 2 executable for --provider opencode")
	fs.StringVar(&cfg.PiPath, "pi-path", "", "Pi executable for --provider pi")
	fs.BoolVar(&cfg.Ephemeral, "ephemeral", false, "do not persist the provider session")
	fs.StringVar(&cfg.ResumeThreadID, "resume-thread", "", "resume this provider thread/session before starting the turn")
	fs.StringVar(&cfg.ForkThreadID, "fork-thread", "", "fork this thread before starting the turn")
	fs.StringVar(&cfg.ForkBeforeTurnID, "fork-before-turn", "", "when forking, exclude this turn and everything after it")
	fs.StringVar(&cfg.ForkThroughTurnID, "fork-through-turn", "", "when forking, include history through this turn")
	fs.DurationVar(&cfg.TurnTimeout, "turn-timeout", time.Hour, "maximum active turn duration, applied per turn; zero disables the watchdog")
	fs.BoolVar(&cfg.Idle, "idle", false, "stay alive after a turn completes and accept prompt commands on the control socket")
	fs.DurationVar(&cfg.IdleTimeout, "idle-timeout", 4*time.Hour, "exit after this long idle; zero disables")
	cfg.RegisterRun = true
	flagArgs := args
	var childArgs []string
	if marker := indexOf(args, "--"); marker >= 0 {
		flagArgs = args[:marker]
		childArgs = args[marker+1:]
		if len(childArgs) == 0 {
			return errors.New("app-server command after -- is empty")
		}
	}
	if err := fs.Parse(flagArgs); err != nil {
		return err
	}
	if len(fs.Args()) > 0 {
		return fmt.Errorf("unexpected run arguments %q; put a custom Codex app-server command after --", strings.Join(fs.Args(), " "))
	}
	if cfg.PromptFile == "" {
		return errors.New("--prompt-file is required")
	}
	if cfg.StateDir == "" {
		return errors.New("--state-dir is required")
	}
	if err := configureProviderDefaults(&cfg, childArgs); err != nil {
		return err
	}
	return runControllerContext(ctx, cfg)
}

func steerCommand(args []string) error {
	fs := flag.NewFlagSet("steer", flag.ContinueOnError)
	var stateDir, messageFile, expectedTurnID string
	var timeout time.Duration
	fs.StringVar(&stateDir, "state-dir", "", "Ruddr run state directory")
	fs.StringVar(&messageFile, "message-file", "", "read steering text from this file")
	fs.StringVar(&expectedTurnID, "expected-turn-id", "", "reject the steer if the active turn changed")
	fs.DurationVar(&timeout, "timeout", 30*time.Second, "control request timeout")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if stateDir == "" {
		return errors.New("--state-dir is required")
	}
	var message string
	if messageFile != "" {
		raw, err := os.ReadFile(messageFile)
		if err != nil {
			return err
		}
		message = strings.TrimSpace(string(raw))
	} else {
		message = strings.TrimSpace(strings.Join(fs.Args(), " "))
	}
	if message == "" {
		return errors.New("steering text is required")
	}
	state, err := readState(stateDir)
	if err != nil {
		return err
	}
	state = displayedState(state)
	if state.Status != "active" {
		return fmt.Errorf("turn is not steerable: status=%s", state.Status)
	}
	if expectedTurnID != "" && state.TurnID != expectedTurnID {
		return fmt.Errorf("active turn changed from %s to %s; steer was not sent", expectedTurnID, state.TurnID)
	}
	response, err := sendControl(stateDir, controlRequest{
		Command:        "steer",
		Text:           message,
		ExpectedTurnID: expectedTurnID,
	}, timeout)
	if err != nil {
		return err
	}
	if !response.OK {
		return errors.New(response.Error)
	}
	fmt.Printf("steered turn %s\n", response.State.TurnID)
	return nil
}

func promptCommand(args []string) error {
	fs := flag.NewFlagSet("prompt", flag.ContinueOnError)
	var stateDir, messageFile string
	var timeout time.Duration
	fs.StringVar(&stateDir, "state-dir", "", "Ruddr run state directory")
	fs.StringVar(&messageFile, "message-file", "", "read prompt text from this file")
	fs.DurationVar(&timeout, "timeout", 60*time.Second, "control request timeout")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if stateDir == "" {
		return errors.New("--state-dir is required")
	}
	var message string
	if messageFile != "" {
		raw, err := os.ReadFile(messageFile)
		if err != nil {
			return err
		}
		message = strings.TrimSpace(string(raw))
	} else {
		message = strings.TrimSpace(strings.Join(fs.Args(), " "))
	}
	if message == "" {
		return errors.New("prompt text is required")
	}
	state, err := readState(stateDir)
	if err != nil {
		return err
	}
	state = displayedState(state)
	if state.Status != "idle" {
		if state.Status == "active" {
			return errors.New("a turn is active; use steer")
		}
		return fmt.Errorf("session is not idle: status=%s", state.Status)
	}
	response, err := sendControl(stateDir, controlRequest{Command: "prompt", Text: message}, timeout)
	if err != nil {
		return err
	}
	if !response.OK {
		return errors.New(response.Error)
	}
	fmt.Printf("started turn %s\n", response.State.TurnID)
	return nil
}

func stopCommand(args []string) error {
	fs := flag.NewFlagSet("stop", flag.ContinueOnError)
	var stateDir string
	var timeout time.Duration
	fs.StringVar(&stateDir, "state-dir", "", "Ruddr run state directory")
	fs.DurationVar(&timeout, "timeout", 30*time.Second, "control request timeout")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if stateDir == "" {
		return errors.New("--state-dir is required")
	}
	response, err := sendControl(stateDir, controlRequest{Command: "shutdown"}, timeout)
	if err != nil {
		return err
	}
	if !response.OK {
		return errors.New(response.Error)
	}
	fmt.Println("shutdown requested")
	return nil
}

func statusCommand(args []string) error {
	fs := flag.NewFlagSet("status", flag.ContinueOnError)
	var stateDir string
	var asJSON bool
	fs.StringVar(&stateDir, "state-dir", "", "Ruddr run state directory")
	fs.BoolVar(&asJSON, "json", false, "print full state as JSON")
	if err := fs.Parse(args); err != nil {
		return err
	}
	state, err := readState(stateDir)
	if err != nil {
		return err
	}
	state = displayedState(state)
	if asJSON {
		return printJSON(state)
	}
	fmt.Printf("%s provider=%s thread=%s turn=%s pid=%d steers=%d\n", state.Status, state.Provider, state.ThreadID, state.TurnID, state.PID, state.Steers)
	if state.Error != "" {
		fmt.Println("error:", state.Error)
	}
	return nil
}

func peekCommand(args []string) error {
	fs := flag.NewFlagSet("peek", flag.ContinueOnError)
	var stateDir string
	var count int
	fs.StringVar(&stateDir, "state-dir", "", "Ruddr run state directory")
	fs.IntVar(&count, "n", 25, "number of trace lines")
	if err := fs.Parse(args); err != nil {
		return err
	}
	state, err := readState(stateDir)
	if err != nil {
		return err
	}
	lines, err := tailLines(state.TracePath, count)
	if err != nil {
		return err
	}
	for _, line := range lines {
		fmt.Println(line)
	}
	return nil
}

func interruptCommand(args []string) error {
	fs := flag.NewFlagSet("interrupt", flag.ContinueOnError)
	var stateDir string
	var timeout time.Duration
	fs.StringVar(&stateDir, "state-dir", "", "Ruddr run state directory")
	fs.DurationVar(&timeout, "timeout", defaultInterruptOperationTimeout+5*time.Second, "control request timeout")
	if err := fs.Parse(args); err != nil {
		return err
	}
	state, err := readState(stateDir)
	if err != nil {
		return err
	}
	state = displayedState(state)
	if state.Status != "active" {
		return fmt.Errorf("turn is not active: status=%s", state.Status)
	}
	response, err := sendControl(stateDir, controlRequest{Command: "interrupt"}, timeout)
	if err != nil {
		return err
	}
	if !response.OK {
		return errors.New(response.Error)
	}
	fmt.Printf("interrupt requested for turn %s\n", response.State.TurnID)
	return nil
}

func waitCommand(args []string) error {
	fs := flag.NewFlagSet("wait", flag.ContinueOnError)
	var stateDir string
	var timeout time.Duration
	fs.StringVar(&stateDir, "state-dir", "", "Ruddr run state directory")
	fs.DurationVar(&timeout, "timeout", 0, "maximum wait; zero means no limit")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if timeout < 0 {
		return errors.New("--timeout must be zero or positive")
	}
	deadline := time.Time{}
	if timeout > 0 {
		deadline = time.Now().Add(timeout)
	}
	return waitForTerminalState(stateDir, deadline, processAlive, 250*time.Millisecond)
}

// waitForTerminalState polls the persisted state until it is terminal, the
// controller disappears, or the deadline passes. The liveness probe and tick
// are seams so lifecycle tests stay deterministic.
func waitForTerminalState(stateDir string, deadline time.Time, alive func(int) bool, tick time.Duration) error {
	ticker := time.NewTicker(tick)
	defer ticker.Stop()
	for {
		state, err := readState(stateDir)
		if err != nil {
			return err
		}
		if terminalStatus(state.Status) {
			return reportWaitResult(state)
		}
		if !alive(state.PID) {
			// The controller persists its terminal state and only then exits,
			// so a dead pid observed after a non-terminal read may simply mean
			// the run finished between the two checks. Re-read before calling
			// the state stale.
			final, finalErr := readState(stateDir)
			if finalErr == nil && terminalStatus(final.Status) {
				return reportWaitResult(final)
			}
			return fmt.Errorf("Ruddr pid %d is not running; state is stale at status=%s", state.PID, state.Status)
		}
		if !deadline.IsZero() && time.Now().After(deadline) {
			return errors.New("wait timed out")
		}
		<-ticker.C
	}
}

// reportWaitResult prints the terminal status and maps it to an exit error.
func reportWaitResult(state runState) error {
	fmt.Println(state.Status)
	if state.Status == "completed" {
		return nil
	}
	if state.Error != "" {
		return errors.New(state.Error)
	}
	return fmt.Errorf("turn ended with status %s", state.Status)
}

func printUsage() {
	name := filepath.Base(os.Args[0])
	fmt.Fprintf(os.Stderr, `Ruddr - live steering for coding agents

Usage:
  %[1]s run [--provider codex|claude|opencode|pi] --prompt-file FILE --state-dir DIR [options]
         [-- APP_SERVER_COMMAND...]
  %[1]s thread list|search|read|turns|fork|name|archive|unarchive [options]
  %[1]s tui [--root DIR] [--state-dir DIR] [--all] [--theme NAME]
  %[1]s steer --state-dir DIR "new direction"
  %[1]s prompt --state-dir DIR "next task"      (idle sessions started with --idle)
  %[1]s stop --state-dir DIR                    (gracefully end an idle session)
  %[1]s models [--json]
  %[1]s status --state-dir DIR [--json]
  %[1]s peek --state-dir DIR [-n 25]
  %[1]s interrupt --state-dir DIR
  %[1]s wait --state-dir DIR [--timeout 10m]
  %[1]s update [--check]                        (install the latest release)
  %[1]s skill install [--dir DIR]               (install the ruddr-delegate agent skill)
  %[1]s version

Ruddr checks GitHub for a newer release at most once a day and mentions it in
the TUI and after version; set RUDDR_NO_UPDATE_CHECK=1 to disable the check.
`, name)
}
