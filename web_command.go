package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

const webEntryEnvironment = "RUDDR_WEB_ENTRY"

// webCommand serves the sessions dashboard to a browser. Like the TUI, it is a
// Bun program; Ruddr only finds the entry point and hands it this binary.
func webCommand(args []string) error {
	if len(args) == 1 && (args[0] == "--help" || args[0] == "-h" || args[0] == "help") {
		printWebUsage()
		return nil
	}
	bunPath, err := exec.LookPath("bun")
	if err != nil {
		return errors.New("ruddr web requires Bun 1.4 or newer; install Bun and run again")
	}
	if err := checkWebBunVersion(bunPath); err != nil {
		return err
	}
	registerRunningRuddrRuns()
	entryPath, err := findWebEntry()
	if err != nil {
		return err
	}
	ruddrPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate Ruddr executable: %w", err)
	}
	cmd := newWebProcess(bunPath, entryPath, ruddrPath, args)
	cmd.Env = os.Environ()
	if latest, ok := availableUpdate(); ok {
		cmd.Env = append(cmd.Env, updateAvailableEnvironment+"="+latest)
	}
	go refreshUpdateCheck(context.Background())
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) && exitErr.ExitCode() > 0 {
			return withExitCode(exitErr.ExitCode(), fmt.Errorf("web server exited: %w", err))
		}
		return fmt.Errorf("web server exited: %w", err)
	}
	return nil
}

func checkWebBunVersion(bunPath string) error {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	output, err := exec.CommandContext(ctx, bunPath, "--version").Output()
	if err != nil {
		return errors.New("ruddr web requires Bun 1.4 or newer; could not check Bun version")
	}
	parts := strings.Split(strings.TrimSpace(string(output)), ".")
	if len(parts) >= 2 {
		major, majorErr := strconv.Atoi(parts[0])
		minor, minorErr := strconv.Atoi(parts[1])
		if majorErr == nil && minorErr == nil && (major > 1 || major == 1 && minor >= 4) {
			return nil
		}
	}
	return errors.New("ruddr web requires Bun 1.4 or newer; upgrade Bun and run again")
}

func newWebProcess(bunPath, entryPath, ruddrPath string, args []string) *exec.Cmd {
	childArgs := []string{"run", entryPath, "--ruddr", ruddrPath}
	childArgs = append(childArgs, args...)
	return exec.Command(bunPath, childArgs...)
}

func findWebEntry() (string, error) {
	var candidates []string
	if configured := os.Getenv(webEntryEnvironment); configured != "" {
		candidates = append(candidates, configured)
	}
	if cwd, err := os.Getwd(); err == nil {
		candidates = append(candidates, filepath.Join(cwd, "web", "server.ts"))
	}
	if executable, err := os.Executable(); err == nil {
		candidates = appendRuntimeSiblingCandidates(candidates, executable, "web", "server.ts")
	}
	if dataHome, err := ruddrDataHome(); err == nil {
		for _, name := range installDirectoryNames {
			candidates = append(candidates, filepath.Join(dataHome, name, "web", "server.ts"))
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
	return "", fmt.Errorf("cannot locate web/server.ts; run the installer or set %s", webEntryEnvironment)
}

func printWebUsage() {
	name := filepath.Base(os.Args[0])
	fmt.Fprintf(os.Stderr, `Ruddr web dashboard

Usage:
  %s web [--host 127.0.0.1] [--port 4519] [--root DIR]... [--state-dir DIR]...
         [--interval 1s] [--token-file FILE] [--open]

Serves the TUI's sessions dashboard to a browser: live chat with streaming
tool calls, inline diffs for available edit patches, activity, output, and the working
tree diff with a file tree. It can steer, prompt, continue, interrupt, and
start sessions, and it shares the TUI's theme.

Every API call needs the access token in ~/.config/ruddr/web-token, created
on first use. Open the printed link once; it stores the token in a cookie.
The server listens on 127.0.0.1 by default. To reach it from a phone, pass a
private address such as a Tailscale IP with --host. Anyone with the token can
steer your agents, so do not expose it on a public interface.

Sessions come from the global registry plus .scratch below the current
directory; --root and --state-dir add more. RUDDR_WEB_HOST and RUDDR_WEB_PORT
set the defaults.
`, name)
}
