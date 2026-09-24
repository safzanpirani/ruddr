package main

import (
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

func TestDelegateSkillIsEmbedded(t *testing.T) {
	if !strings.HasPrefix(delegateSkill, "---\nname: ruddr-delegate\n") {
		t.Fatalf("unexpected skill header: %q", delegateSkill[:40])
	}
}

func TestInstallDelegateSkill(t *testing.T) {
	dir := t.TempDir()
	path, err := installDelegateSkill(dir)
	if err != nil {
		t.Fatal(err)
	}
	if path != filepath.Join(dir, "ruddr-delegate", "SKILL.md") {
		t.Fatalf("installed at %s", path)
	}
	data, err := os.ReadFile(path)
	if err != nil || string(data) != delegateSkill {
		t.Fatalf("skill content mismatch: %v", err)
	}
	if err := os.WriteFile(path, []byte("stale"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := installDelegateSkill(dir); err != nil {
		t.Fatal(err)
	}
	data, _ = os.ReadFile(path)
	if string(data) != delegateSkill {
		t.Fatal("stale copy was not replaced")
	}
	entries, _ := os.ReadDir(filepath.Dir(path))
	if len(entries) != 1 {
		t.Fatalf("temp file left behind: %d entries", len(entries))
	}
}

func TestSkillInstallCommandUsesDirFlags(t *testing.T) {
	a, b := t.TempDir(), t.TempDir()
	if err := runCLI([]string{"skill", "install", "--dir", a, "--dir", b}); err != nil {
		t.Fatal(err)
	}
	for _, dir := range []string{a, b} {
		if _, err := os.Stat(filepath.Join(dir, "ruddr-delegate", "SKILL.md")); err != nil {
			t.Fatal(err)
		}
	}
}

func TestConcurrentSkillInstall(t *testing.T) {
	dir := t.TempDir()
	// A leftover temporary path from an older installer must not block installs.
	legacy := filepath.Join(dir, delegateSkillName, "SKILL.md.tmp")
	if err := os.MkdirAll(legacy, 0o755); err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	start := make(chan struct{})
	for i := 0; i < 32; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			if _, err := installDelegateSkill(dir); err != nil {
				t.Error(err)
			}
		}()
	}
	close(start)
	wg.Wait()
	data, err := os.ReadFile(filepath.Join(dir, delegateSkillName, "SKILL.md"))
	if err != nil || string(data) != delegateSkill {
		t.Fatalf("content mismatch: %v", err)
	}
	entries, err := os.ReadDir(filepath.Join(dir, delegateSkillName))
	if err != nil || len(entries) != 2 {
		t.Fatalf("temporary files leaked: %v, %v", entries, err)
	}
}

func TestDefaultSkillDirectoriesIncludeCodexOnlyWhenInstalled(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("USERPROFILE", home)
	dirs, err := defaultSkillDirectories()
	if err != nil {
		t.Fatal(err)
	}
	if len(dirs) != 2 {
		t.Fatalf("dirs without Codex = %q", dirs)
	}
	if err := os.Mkdir(filepath.Join(home, ".codex"), 0o755); err != nil {
		t.Fatal(err)
	}
	dirs, err = defaultSkillDirectories()
	if err != nil {
		t.Fatal(err)
	}
	if len(dirs) != 3 || dirs[2] != filepath.Join(home, ".codex", "skills") {
		t.Fatalf("dirs with Codex = %q", dirs)
	}
}
