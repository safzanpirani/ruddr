package main

import (
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"testing"
)

func TestSplitRemoteFlag(t *testing.T) {
	for _, test := range []struct {
		args   []string
		target string
		rest   []string
		found  bool
		err    bool
	}{
		{args: []string{"status", "--state-dir", "x"}, rest: []string{"status", "--state-dir", "x"}},
		{args: []string{"--remote", "ampere", "status"}, target: "ampere", rest: []string{"status"}, found: true},
		{args: []string{"--remote=user@host", "tui"}, target: "user@host", rest: []string{"tui"}, found: true},
		{args: []string{"-remote", "ampere", "peek"}, target: "ampere", rest: []string{"peek"}, found: true},
		{args: []string{"--remote"}, found: true, err: true},
		{args: []string{"--remote", "-oProxyCommand=evil", "status"}, found: true, err: true},
		{args: []string{"--remote="}, found: true, err: true},
	} {
		target, rest, found, err := splitRemoteFlag(test.args)
		if found != test.found || (err != nil) != test.err {
			t.Fatalf("splitRemoteFlag(%q) = found %v err %v", test.args, found, err)
		}
		if test.err {
			continue
		}
		if target != test.target || !reflect.DeepEqual(rest, test.rest) {
			t.Fatalf("splitRemoteFlag(%q) = %q %q", test.args, target, rest)
		}
	}
}

func TestPlanRemoteRunSendsPromptOverStdinAndDetaches(t *testing.T) {
	promptPath := filepath.Join(t.TempDir(), "brief.md")
	if err := os.WriteFile(promptPath, []byte("fix the bug\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	plan, err := planRemote([]string{
		"run", "--provider", "codex", "--cwd", "~/proj", "--prompt-file=" + promptPath,
		"--state-dir", "runs/x", "--", "codex", "--prompt-file", "keep",
	}, strings.NewReader(""))
	if err != nil {
		t.Fatal(err)
	}
	want := []string{
		"run", "--detach", "--provider", "codex", "--cwd", "~/proj", "--prompt-file=-",
		"--state-dir", "runs/x", "--", "codex", "--prompt-file", "keep",
	}
	if !reflect.DeepEqual(plan.args, want) {
		t.Fatalf("args = %q, want %q", plan.args, want)
	}
	if string(plan.stdin) != "fix the bug\n" || plan.tty {
		t.Fatalf("stdin = %q tty = %v", plan.stdin, plan.tty)
	}
}

func TestPlanRemoteRunRequiresCWD(t *testing.T) {
	_, err := planRemote([]string{"run", "--prompt-file", "brief.md", "--state-dir", "x"}, strings.NewReader(""))
	if err == nil || !strings.Contains(err.Error(), "--cwd") {
		t.Fatalf("err = %v, want a --cwd error", err)
	}
}

func TestPlanRemoteForwardsMessageFileAndLocalStdin(t *testing.T) {
	plan, err := planRemote([]string{"steer", "--state-dir", "x", "--message-file", "-"}, strings.NewReader("new direction"))
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{"steer", "--state-dir", "x", "--message-file", "-"}; !reflect.DeepEqual(plan.args, want) {
		t.Fatalf("args = %q", plan.args)
	}
	if string(plan.stdin) != "new direction" {
		t.Fatalf("stdin = %q", plan.stdin)
	}

	plan, err = planRemote([]string{"prompt", "--state-dir", "x", "next", "task"}, strings.NewReader(""))
	if err != nil || plan.stdin != nil || plan.tty {
		t.Fatalf("plain prompt plan = %+v, %v", plan, err)
	}
	plan, err = planRemote([]string{"tui", "--mobile"}, strings.NewReader(""))
	if err != nil || !plan.tty {
		t.Fatalf("tui plan = %+v, %v", plan, err)
	}
}

func TestRemoteShellCommandQuotesArguments(t *testing.T) {
	got := remoteShellCommand("ruddr", []string{"steer", "--state-dir", "~/runs/it's", "a b; rm -rf /", "~/"})
	want := remotePathPrefix + `exec 'ruddr' 'steer' '--state-dir' ~/'runs/it'\''s' 'a b; rm -rf /' ~/`
	if got != want {
		t.Fatalf("command =\n%s\nwant\n%s", got, want)
	}
	args := remoteSSHArgs("ampere", remotePlan{args: []string{"status"}}, "ruddr")
	if args[0] != "-T" || args[1] != "--" || args[2] != "ampere" {
		t.Fatalf("ssh args = %q", args)
	}
	if remoteSSHArgs("ampere", remotePlan{args: []string{"tui"}, tty: true}, "ruddr")[0] != "-t" {
		t.Fatal("tui must request a remote terminal")
	}
}

// The fake ssh runs the rendered command through sh, so this covers quoting,
// stdin forwarding, output passthrough, and the remote exit status together.
func TestRemoteCommandRunsThroughSSHAndPropagatesExitStatus(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("uses a POSIX shell script as the fake ssh")
	}
	dir := t.TempDir()
	record := filepath.Join(dir, "record")
	fakeRuddr := filepath.Join(dir, "fake-ruddr")
	writeExecutable(t, fakeRuddr, `#!/bin/sh
printf '%s\n' "$@" > "`+record+`.args"
cat > "`+record+`.stdin"
exit 3
`)
	fakeSSH := filepath.Join(dir, "ssh")
	writeExecutable(t, fakeSSH, `#!/bin/sh
printf '%s\n' "$1" "$2" "$3" > "`+record+`.ssh"
exec sh -c "$4"
`)
	t.Setenv(remoteSSHEnvironment, fakeSSH)
	t.Setenv(remoteRuddrEnvironment, fakeRuddr)

	messagePath := filepath.Join(dir, "message.md")
	if err := os.WriteFile(messagePath, []byte("don't touch main.go"), 0o600); err != nil {
		t.Fatal(err)
	}
	err := remoteCommand("ampere", []string{"steer", "--state-dir", "run dir", "--message-file", messagePath})
	var exitErr exitStatusError
	if !errors.As(err, &exitErr) || exitErr.code != 3 {
		t.Fatalf("err = %v, want remote exit status 3", err)
	}
	assertFileEquals(t, record+".ssh", "-T\n--\nampere\n")
	assertFileEquals(t, record+".args", "steer\n--state-dir\nrun dir\n--message-file\n-\n")
	assertFileEquals(t, record+".stdin", "don't touch main.go")
}

func writeExecutable(t *testing.T, path, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o700); err != nil {
		t.Fatal(err)
	}
}

func assertFileEquals(t *testing.T, path, want string) {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(raw) != want {
		t.Fatalf("%s = %q, want %q", filepath.Base(path), raw, want)
	}
}
