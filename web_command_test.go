package main

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestFindWebEntryHonorsConfiguredPath(t *testing.T) {
	entryPath := filepath.Join(t.TempDir(), "custom-web.ts")
	if err := os.WriteFile(entryPath, []byte("// test\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv(webEntryEnvironment, entryPath)
	got, err := findWebEntry()
	if err != nil {
		t.Fatal(err)
	}
	if want, _ := filepath.Abs(entryPath); got != want {
		t.Fatalf("findWebEntry() = %q, want %q", got, want)
	}
}

func TestFindWebEntryUsesInstalledDataDirectory(t *testing.T) {
	t.Chdir(t.TempDir())
	t.Setenv(webEntryEnvironment, "")
	dataHome := t.TempDir()
	t.Setenv("XDG_DATA_HOME", dataHome)
	entryPath := filepath.Join(dataHome, "ruddr", "web", "server.ts")
	if err := os.MkdirAll(filepath.Dir(entryPath), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(entryPath, []byte("// installed test\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	got, err := findWebEntry()
	if err != nil {
		t.Fatal(err)
	}
	if got != entryPath {
		t.Fatalf("findWebEntry() = %q, want installed entry %q", got, entryPath)
	}
}

func TestWebProcessPassesRuddrAndArguments(t *testing.T) {
	cmd := newWebProcess("bun", "/installed/web/server.ts", "/installed/ruddr", []string{"--port", "9000"})
	want := []string{"bun", "run", "/installed/web/server.ts", "--ruddr", "/installed/ruddr", "--port", "9000"}
	if !reflect.DeepEqual(cmd.Args, want) {
		t.Fatalf("web process args = %v, want %v", cmd.Args, want)
	}
	if cmd.Dir != "" {
		t.Fatalf("web process directory = %q, want inherited caller directory", cmd.Dir)
	}
}

func TestWebCommandPreservesUsageExitCode(t *testing.T) {
	if os.PathSeparator == '\\' {
		t.Skip("shell fixture requires a Unix host")
	}
	directory := t.TempDir()
	if err := os.WriteFile(filepath.Join(directory, "bun"), []byte("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 1.4.2; exit 0; fi\nexit 2\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", directory+string(os.PathListSeparator)+os.Getenv("PATH"))
	t.Setenv(webEntryEnvironment, filepath.Join(directory, "server.ts"))
	t.Setenv("RUDDR_REGISTRY_DIR", filepath.Join(directory, "registry"))
	t.Setenv("RUDDR_NO_UPDATE_CHECK", "1")
	if err := os.WriteFile(filepath.Join(directory, "server.ts"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := webCommand([]string{"--bogus"}); exitCodeFor(err) != exitUsage {
		t.Fatalf("webCommand error = %v, exit code = %d, want 2", err, exitCodeFor(err))
	}
}

func TestWebBunMinimumVersion(t *testing.T) {
	if os.PathSeparator == '\\' {
		t.Skip("shell fixture requires a Unix host")
	}
	for _, version := range []string{"1.3.99", "0.9.1", "invalid", "1.4.0", "1.4.2", "2.0.0"} {
		t.Run(version, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "bun")
			if err := os.WriteFile(path, []byte("#!/bin/sh\necho '"+version+"'\n"), 0o700); err != nil {
				t.Fatal(err)
			}
			err := checkWebBunVersion(path)
			wantOK := version == "1.4.0" || version == "1.4.2" || version == "2.0.0"
			if (err == nil) != wantOK {
				t.Fatalf("checkWebBunVersion(%q) = %v, want success %v", version, err, wantOK)
			}
		})
	}
}
