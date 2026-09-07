package main

import (
	"errors"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestAgentOutputAppendsOrderedMessagesAndTurnBoundaries(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "output.md")
	if err := writePrivateFile(path, nil); err != nil {
		t.Fatal(err)
	}
	r := &controller{store: &stateStore{
		path:  filepath.Join(dir, "state.json"),
		state: runState{OutputPath: path, Turns: 2},
	}, turnDone: make(chan struct{}), turnCount: 2}
	write := func(text string) {
		t.Helper()
		if err := r.recordAgentMessage(text); err != nil {
			t.Fatal(err)
		}
	}
	r.appendOutputSeparator() // No leading boundary before any output.
	write("first")
	write("---") // A message can itself contain a Markdown rule.
	r.appendOutputSeparator()
	if err := r.rollbackRejectedTurn(2); err != nil {
		t.Fatal(err)
	}
	r.appendOutputSeparator()
	r.appendOutputSeparator() // Preserve an accepted turn with no messages.
	write("last\nline")
	got, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if want := "first\n\n---\n\n---\n\n---\n\nlast\nline\n"; string(got) != want {
		t.Fatalf("output = %q, want %q", got, want)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if runtime.GOOS != "windows" && info.Mode().Perm() != 0o600 {
		t.Fatalf("output mode = %o", info.Mode().Perm())
	}
}

func TestAgentOutputFailureKeepsPreviousMessagesAndPendingBoundary(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "output.md")
	if err := writePrivateFile(path, nil); err != nil {
		t.Fatal(err)
	}
	r := &controller{store: &stateStore{state: runState{OutputPath: path}}}
	if err := r.recordAgentMessage("first"); err != nil {
		t.Fatal(err)
	}
	r.appendOutputSeparator()
	if err := os.Rename(path, path+".saved"); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(path, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := r.recordAgentMessage("last"); err == nil {
		t.Fatal("expected output failure")
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(path+".saved", path); err != nil {
		t.Fatal(err)
	}
	if err := r.recordAgentMessage("last"); err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(path)
	if err != nil || string(got) != "first\n\n---\n\nlast\n" {
		t.Fatalf("output = %q, error = %v", got, err)
	}
}

func TestAppendOutputRejectsSymlink(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "target.md")
	if err := os.WriteFile(target, []byte("untouched"), 0o600); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "output.md")
	if err := os.Symlink(target, path); err != nil {
		t.Skipf("symlinks unavailable: %v", err)
	}
	if err := appendPrivateOutput(path, "private completion"); err == nil {
		t.Fatal("expected symlink rejection")
	}
	got, err := os.ReadFile(target)
	if err != nil || string(got) != "untouched" {
		t.Fatalf("target = %q, error = %v", got, err)
	}
}

type partialOutput struct {
	strings.Builder
	err error
}

func (p *partialOutput) WriteString(s string) (int, error) {
	p.Builder.WriteString(s[:2])
	return 2, p.err
}

func (p *partialOutput) Truncate(size int64) error {
	old := p.String()[:size]
	p.Reset()
	p.Builder.WriteString(old)
	return nil
}

func TestAppendOutputRollsBackPartialWrite(t *testing.T) {
	for _, writeErr := range []error{nil, errors.New("disk full")} {
		p := &partialOutput{err: writeErr}
		p.Builder.WriteString("first\n")
		wantErr := writeErr
		if wantErr == nil {
			wantErr = io.ErrShortWrite
		}
		if err := appendOutputChunk(p, 6, "\nsecond\n"); !errors.Is(err, wantErr) {
			t.Fatalf("error = %v, want %v", err, wantErr)
		}
		if p.String() != "first\n" {
			t.Fatalf("partial output retained: %q", p.String())
		}
	}
}
