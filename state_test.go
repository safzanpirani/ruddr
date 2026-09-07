package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"testing"
)

func TestContextUsageIsSeparateFromSessionTotals(t *testing.T) {
	store := &stateStore{path: filepath.Join(t.TempDir(), "state.json"), state: runState{ThreadID: "root"}}
	r := &controller{store: store}
	update := func(thread string, last any) {
		raw, err := json.Marshal(map[string]any{
			"threadId": thread,
			"tokenUsage": map[string]any{
				"total": map[string]any{"totalTokens": 2500000, "inputTokens": 2400000, "outputTokens": 100000},
				"last":  last, "modelContextWindow": 200000,
			},
		})
		if err != nil {
			t.Fatal(err)
		}
		r.handleTokenUsage(raw)
	}
	for _, tokens := range []int64{50000, 1000, 0} {
		update("root", map[string]any{"totalTokens": tokens})
		state, err := readState(filepath.Dir(store.path))
		if err != nil {
			t.Fatal(err)
		}
		usage := state.TokenUsage
		if usage == nil || usage.TotalTokens != 2500000 || usage.ContextTokens == nil || *usage.ContextTokens != tokens {
			t.Fatalf("context %d was not preserved separately: %+v", tokens, usage)
		}
		update("subagent", map[string]any{"totalTokens": 199000})
		if *store.snapshot().TokenUsage.ContextTokens != tokens {
			t.Fatal("subagent usage replaced the root context")
		}
	}
	update("root", nil)
	if store.snapshot().TokenUsage.ContextTokens != nil {
		t.Fatal("unknown context retained a stale measurement")
	}
}

func TestConcurrentPrivateWritesAreAtomic(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "state.json")
	const writers = 32
	start := make(chan struct{})
	var wg sync.WaitGroup
	for i := range writers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			data := bytes.Repeat([]byte(fmt.Sprintf("%02d", i)), 32*1024)
			if err := writePrivateFile(path, data); err != nil {
				t.Errorf("write %d: %v", i, err)
			}
		}()
	}
	close(start)
	wg.Wait()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(data) != 64*1024 || !bytes.Equal(data, bytes.Repeat(data[:2], 32*1024)) {
		t.Fatal("final file contains a partial or mixed write")
	}
	entries, err := os.ReadDir(dir)
	if err != nil || len(entries) != 1 {
		t.Fatalf("temporary files left behind: %v, %v", entries, err)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if runtime.GOOS != "windows" && info.Mode().Perm() != 0o600 {
		t.Fatalf("file mode = %o, want 600", info.Mode().Perm())
	}
}

func TestPrivateWriteDoesNotFollowLegacyTemporarySymlink(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("symlink creation requires privileges on Windows")
	}
	dir := t.TempDir()
	path := filepath.Join(dir, "output.md")
	other := filepath.Join(dir, "other")
	if err := os.WriteFile(other, []byte("keep"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(other, path+".tmp"); err != nil {
		t.Fatal(err)
	}
	if err := writePrivateFile(path, []byte("new")); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(other)
	if err != nil || string(data) != "keep" {
		t.Fatalf("unrelated file changed: %q, %v", data, err)
	}
}
