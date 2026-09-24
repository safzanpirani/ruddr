package main

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
)

const (
	remoteSSHEnvironment   = "RUDDR_SSH"
	remoteRuddrEnvironment = "RUDDR_REMOTE_RUDDR"
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

func remoteSSHArgs(target string, plan remotePlan, ruddr string) []string {
	ttyFlag := "-T"
	if plan.tty {
		ttyFlag = "-t"
	}
	return []string{ttyFlag, "--", target, remoteShellCommand(ruddr, plan.args)}
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
	cmd := exec.Command(sshPath, remoteSSHArgs(target, plan, ruddr)...)
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
