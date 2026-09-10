package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"unicode/utf8"
)

// oneLine feeds trace.log, which the TUI and `peek` read as text. Truncating
// mid-rune would leave a broken byte sequence in the log.
func TestOneLineTruncatesOnRuneBoundaries(t *testing.T) {
	for _, limit := range []int{1, 5, 15, 16, 39} {
		got := oneLine(strings.Repeat("é", 20), limit)
		if !utf8.ValidString(got) {
			t.Fatalf("oneLine(limit=%d) = %q is not valid UTF-8", limit, got)
		}
		if len(got) > limit+len("…") {
			t.Fatalf("oneLine(limit=%d) = %q exceeds the limit", limit, got)
		}
	}
	if got := oneLine("日本語のテキスト", 7); !utf8.ValidString(got) || !strings.HasSuffix(got, "…") {
		t.Fatalf("oneLine on multi-byte text = %q", got)
	}
}

// Each trace record must occupy exactly one line: provider-supplied text can
// carry newlines, and a wrapped record would break the line-oriented readers
// (and could forge a record).
func TestTracefKeepsEachRecordOnOneLine(t *testing.T) {
	path := filepath.Join(t.TempDir(), "trace.log")
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	r := &controller{trace: file}
	r.tracef("[warn] %s", "provider failed\n2026-01-01T00:00:00Z [say] forged agent message")
	r.tracef("[turn] %s", "completed")
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}

	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSuffix(string(raw), "\n"), "\n")
	if len(lines) != 2 {
		t.Fatalf("tracef wrote %d lines, want 2: %q", len(lines), raw)
	}
	if !strings.Contains(lines[0], "[warn] provider failed 2026-01-01T00:00:00Z [say] forged") {
		t.Fatalf("first trace record = %q", lines[0])
	}
}
