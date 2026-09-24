package main

import (
	"bytes"
	"encoding/json"
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
	remoteSSHEnvironment   = "RUDDR_SSH"
	remoteRuddrEnvironment = "RUDDR_REMOTE_RUDDR"
	remoteShellEnvironment = "RUDDR_REMOTE_SHELL"
	remoteShellPOSIX       = "posix"
	remoteShellPowerShell  = "powershell"
	// Prints "Core" or "Desktop" in PowerShell, ".PSEdition" in a POSIX shell,
	// and the text unchanged in cmd.exe.
	remoteShellProbe = "echo $PSVersionTable.PSEdition"
	// Non-interactive SSH shells often skip the profile that puts user-level
	// installs on PATH.
	remotePathPrefix = `PATH="$HOME/.local/bin:$HOME/.bun/bin:$PATH"; export PATH; `
)

// exitStatusError carries a remote command's exit status back to main without
// printing a second error; the remote ruddr already wrote its own.
type exitStatusError struct{ code int }

func (e exitStatusError) Error() string {
	return fmt.Sprintf("remote command exited with status %d", e.code)
}

// splitRemoteFlag removes a leading --remote TARGET (or --remote=TARGET).
func splitRemoteFlag(args []string) (target string, rest []string, found bool, err error) {
	if len(args) == 0 {
		return "", args, false, nil
	}
	name, value, hasValue := splitFlagArg(args[0])
	if name != "remote" {
		return "", args, false, nil
	}
	rest = args[1:]
	if !hasValue {
		if len(rest) == 0 {
			return "", nil, true, errors.New("--remote requires an SSH target")
		}
		value, rest = rest[0], rest[1:]
	}
	if value == "" || strings.HasPrefix(value, "-") {
		return "", nil, true, fmt.Errorf("invalid --remote SSH target %q", value)
	}
	return value, rest, true, nil
}

type remotePlan struct {
	args  []string
	stdin []byte
	tty   bool
}

// planRemote adapts a command for a remote ruddr: local files travel over
// stdin, run starts detached so it outlives the SSH connection, and the TUI
// gets a terminal.
func planRemote(args []string, localStdin io.Reader) (remotePlan, error) {
	if len(args) == 0 {
		return remotePlan{}, errors.New("a command is required after --remote TARGET")
	}
	plan := remotePlan{args: append([]string(nil), args...)}
	command, rest := args[0], args[1:]
	switch command {
	case "run":
		if !hasFlag(rest, "cwd") {
			return remotePlan{}, errors.New("--cwd is required with --remote; the local directory does not exist on the remote host")
		}
		if !hasFlag(rest, "prompt-file") {
			return remotePlan{}, errors.New("--prompt-file is required")
		}
		rewritten, promptFile, _ := replaceFlagValue(rest, "prompt-file", "-")
		payload, err := readLocalPayload(promptFile, localStdin)
		if err != nil {
			return remotePlan{}, err
		}
		plan.stdin = payload
		if !hasFlag(rewritten, "detach") {
			rewritten = append([]string{"--detach"}, rewritten...)
		}
		plan.args = append([]string{"run"}, rewritten...)
	case "steer", "prompt":
		if !hasFlag(rest, "message-file") {
			break
		}
		rewritten, messageFile, _ := replaceFlagValue(rest, "message-file", "-")
		payload, err := readLocalPayload(messageFile, localStdin)
		if err != nil {
			return remotePlan{}, err
		}
		plan.stdin = payload
		plan.args = append([]string{command}, rewritten...)
	case "tui":
		plan.tty = true
	}
	return plan, nil
}

func readLocalPayload(path string, localStdin io.Reader) ([]byte, error) {
	if path == "" {
		return nil, errors.New("file flag requires a path")
	}
	if path == "-" {
		return io.ReadAll(localStdin)
	}
	return os.ReadFile(path)
}

// remoteShellCommand renders argv for the remote POSIX shell. Every argument
// is single-quoted, except that a leading ~/ stays bare so the remote shell
// expands it to the remote home.
func remoteShellCommand(ruddr string, args []string) string {
	var builder strings.Builder
	builder.WriteString(remotePathPrefix)
	builder.WriteString("exec ")
	builder.WriteString(remoteShellWord(ruddr))
	for _, arg := range args {
		builder.WriteByte(' ')
		builder.WriteString(remoteShellWord(arg))
	}
	return builder.String()
}

func remoteShellWord(word string) string {
	if rest, ok := strings.CutPrefix(word, "~/"); ok {
		if rest == "" {
			return "~/"
		}
		return "~/" + shellQuote(rest)
	}
	return shellQuote(word)
}

func shellQuote(word string) string {
	return "'" + strings.ReplaceAll(word, "'", `'\''`) + "'"
}

// remotePowerShellCommand renders argv for a remote PowerShell, the default
// OpenSSH shell on many Windows hosts. It uses only single quotes, because
// Windows OpenSSH does not preserve double quotes in the command string.
// Stop turns a missing ruddr into a nonzero exit.
func remotePowerShellCommand(ruddr string, args []string) string {
	var builder strings.Builder
	builder.WriteString("$ErrorActionPreference = 'Stop'; & ")
	builder.WriteString(powerShellWord(ruddr))
	for _, arg := range args {
		builder.WriteByte(' ')
		builder.WriteString(powerShellWord(arg))
	}
	builder.WriteString("; exit $LASTEXITCODE")
	return builder.String()
}

// powerShellWord quotes one argument. PowerShell does not expand ~ in native
// command arguments, so a leading ~/ or ~\ becomes $HOME.
func powerShellWord(word string) string {
	for _, prefix := range []string{"~/", "~\\"} {
		if rest, ok := strings.CutPrefix(word, prefix); ok {
			if rest == "" {
				return "$HOME"
			}
			return "($HOME + " + powerShellQuote("\\"+rest) + ")"
		}
	}
	return powerShellQuote(word)
}

func powerShellQuote(word string) string {
	return "'" + strings.ReplaceAll(word, "'", "''") + "'"
}

func remoteSSHArgs(target string, plan remotePlan, ruddr, shell string) []string {
	ttyFlag := "-T"
	if plan.tty {
		ttyFlag = "-t"
	}
	command := remoteShellCommand(ruddr, plan.args)
	if shell == remoteShellPowerShell {
		command = remotePowerShellCommand(ruddr, plan.args)
	}
	return []string{ttyFlag, "--", target, command}
}

// classifyRemoteShell maps the probe's output to a supported shell.
func classifyRemoteShell(output string) (string, error) {
	switch strings.TrimSpace(output) {
	case "Core", "Desktop":
		return remoteShellPowerShell, nil
	case "", ".PSEdition":
		return remoteShellPOSIX, nil
	case remoteShellProbe[len("echo "):]:
		return "", errors.New("the remote default shell is cmd.exe; set the OpenSSH DefaultShell to PowerShell, or set " + remoteShellEnvironment + "=powershell if commands run under PowerShell anyway")
	default:
		return "", fmt.Errorf("cannot tell the remote shell from %q; set %s to posix or powershell", strings.TrimSpace(output), remoteShellEnvironment)
	}
}

func remoteShellCachePath() (string, error) {
	runs, err := runRegistryDirectory()
	if err != nil {
		return "", err
	}
	return filepath.Join(filepath.Dir(runs), "remote-shells.json"), nil
}

// resolveRemoteShell returns the target's shell from the environment override,
// the per-target cache, or one probe over ssh whose answer is cached.
func resolveRemoteShell(sshPath, target string) (string, error) {
	if configured := os.Getenv(remoteShellEnvironment); configured != "" {
		if configured != remoteShellPOSIX && configured != remoteShellPowerShell {
			return "", fmt.Errorf("%s must be posix or powershell, not %q", remoteShellEnvironment, configured)
		}
		return configured, nil
	}
	cache := map[string]string{}
	cachePath, cacheErr := remoteShellCachePath()
	if cacheErr == nil {
		if raw, err := os.ReadFile(cachePath); err == nil {
			_ = json.Unmarshal(raw, &cache)
		}
		if shell := cache[target]; shell == remoteShellPOSIX || shell == remoteShellPowerShell {
			return shell, nil
		}
	}
	probe := exec.Command(sshPath, "-T", "--", target, remoteShellProbe)
	probe.Stderr = os.Stderr
	output, err := runWithTimeout(probe, 30*time.Second)
	if err != nil {
		return "", fmt.Errorf("probe the remote shell on %s: %w", target, err)
	}
	shell, err := classifyRemoteShell(string(output))
	if err != nil {
		return "", err
	}
	if cacheErr == nil {
		cache[target] = shell
		if raw, err := json.MarshalIndent(cache, "", "  "); err == nil {
			if err := os.MkdirAll(filepath.Dir(cachePath), 0o700); err == nil {
				_ = writePrivateFile(cachePath, append(raw, '\n'))
			}
		}
	}
	return shell, nil
}

func runWithTimeout(cmd *exec.Cmd, timeout time.Duration) ([]byte, error) {
	var stdout bytes.Buffer
	cmd.Stdout = &stdout
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case err := <-done:
		return stdout.Bytes(), err
	case <-time.After(timeout):
		_ = cmd.Process.Kill()
		<-done
		return nil, fmt.Errorf("timed out after %s", timeout)
	}
}

func remoteCommand(target string, args []string) error {
	plan, err := planRemote(args, os.Stdin)
	if err != nil {
		return err
	}
	if plan.tty {
		stdinInfo, stdinErr := os.Stdin.Stat()
		stdoutInfo, stdoutErr := os.Stdout.Stat()
		if stdinErr != nil || stdoutErr != nil || stdinInfo.Mode()&os.ModeCharDevice == 0 || stdoutInfo.Mode()&os.ModeCharDevice == 0 {
			return errors.New("the TUI requires an interactive terminal")
		}
	}
	sshPath := os.Getenv(remoteSSHEnvironment)
	if sshPath == "" {
		sshPath = "ssh"
	}
	ruddr := os.Getenv(remoteRuddrEnvironment)
	if ruddr == "" {
		ruddr = "ruddr"
	}
	shell, err := resolveRemoteShell(sshPath, target)
	if err != nil {
		return err
	}
	cmd := exec.Command(sshPath, remoteSSHArgs(target, plan, ruddr, shell)...)
	switch {
	case plan.tty:
		cmd.Stdin = os.Stdin
	case plan.stdin != nil:
		cmd.Stdin = bytes.NewReader(plan.stdin)
	}
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) && exitErr.ExitCode() > 0 {
			return exitStatusError{code: exitErr.ExitCode()}
		}
		return fmt.Errorf("ssh %s: %w", target, err)
	}
	return nil
}
