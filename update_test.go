package main

import (
	"context"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"testing"
	"time"
)

func TestCompareVersions(t *testing.T) {
	cases := []struct {
		a, b string
		want int
	}{
		{"0.3.0", "0.3.0", 0},
		{"v0.3.1", "0.3.0", 1},
		{"0.3.0", "0.10.0", -1},
		{"1.0.0", "0.99.99", 1},
		{"0.4.0-rc1", "0.4.0", 0},
		{"garbage", "0.1.0", -1},
		{"0.1.0", "garbage", 1},
	}
	for _, tc := range cases {
		if got := compareVersions(tc.a, tc.b); got != tc.want {
			t.Errorf("compareVersions(%q, %q) = %d, want %d", tc.a, tc.b, got, tc.want)
		}
	}
}

func TestVersionFromReleaseURL(t *testing.T) {
	got, err := versionFromReleaseURL("https://github.com/safzanpirani/ruddr/releases/tag/v0.4.2")
	if err != nil || got != "0.4.2" {
		t.Fatalf("got %q, %v", got, err)
	}
	if _, err := versionFromReleaseURL("https://github.com/safzanpirani/ruddr/releases"); err == nil {
		t.Fatal("expected an error without a tag")
	}
}

func TestChecksumFor(t *testing.T) {
	checksums := "abc  ruddr-darwin-arm64\nDEF *ruddr-windows-amd64.exe\n"
	if got, ok := checksumFor(checksums, "ruddr-darwin-arm64"); !ok || got != "abc" {
		t.Fatalf("got %q %v", got, ok)
	}
	if got, ok := checksumFor(checksums, "ruddr-windows-amd64.exe"); !ok || got != "def" {
		t.Fatalf("got %q %v", got, ok)
	}
	if _, ok := checksumFor(checksums, "ruddr-linux-amd64"); ok {
		t.Fatal("unexpected match")
	}
}

func TestDetectInstallChannel(t *testing.T) {
	root := t.TempDir()
	t.Setenv("XDG_DATA_HOME", filepath.Join(root, "data"))

	if got := detectInstallChannel(filepath.Join(root, "bin", "ruddr")); got.Kind != "binary" {
		t.Fatalf("bare binary detected as %q", got.Kind)
	}

	npmRoot := filepath.Join(root, "lib", "node_modules", "ruddr")
	writeFile(t, filepath.Join(npmRoot, "package.json"), `{"name":"ruddr"}`)
	writeFile(t, filepath.Join(npmRoot, "scripts", "npm-binary.cjs"), "")
	if got := detectInstallChannel(filepath.Join(npmRoot, "ruddr")); got.Kind != "npm" || got.PackageRoot != npmRoot {
		t.Fatalf("npm package detected as %+v", got)
	}

	bunRoot := filepath.Join(root, ".bun", "install", "global", "node_modules", "ruddr")
	writeFile(t, filepath.Join(bunRoot, "package.json"), `{"name":"ruddr"}`)
	writeFile(t, filepath.Join(bunRoot, "scripts", "npm-binary.cjs"), "")
	if got := detectInstallChannel(filepath.Join(bunRoot, "ruddr")); got.Kind != "bun" {
		t.Fatalf("bun package detected as %q", got.Kind)
	}

	writeFile(t, filepath.Join(root, "data", "ruddr", "tui", "index.ts"), "")
	if got := detectInstallChannel(filepath.Join(root, "bin", "ruddr")); got.Kind != "source" {
		t.Fatalf("source install detected as %q", got.Kind)
	}
}

func TestNpmUpdateArgsPinPrefix(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix global layout")
	}
	// The README's user-prefix install: npm install -g --prefix "$HOME/.local" ruddr.
	root := filepath.Join(string(filepath.Separator), "home", "me", ".local", "lib", "node_modules", "ruddr")
	want := []string{"install", "-g", "--prefix", filepath.Join(string(filepath.Separator), "home", "me", ".local"), "ruddr@0.4.2"}
	if got := npmUpdateArgs(root, "0.4.2"); !reflect.DeepEqual(got, want) {
		t.Fatalf("npmUpdateArgs = %q, want %q", got, want)
	}
	if got := npmPrefixFromPackageRoot(filepath.Join(string(filepath.Separator), "usr", "lib", "node_modules", "ruddr")); got != "/usr" {
		t.Fatalf("system prefix = %q, want /usr", got)
	}
}

func TestNpmUpdateArgsWithoutPackageRoot(t *testing.T) {
	want := []string{"install", "-g", "ruddr@0.4.2"}
	for _, root := range []string{"", "/opt/ruddr", "/opt/node_modules/ruddr", "/lib/node_modules/ruddr"} {
		if got := npmUpdateArgs(root, "0.4.2"); !reflect.DeepEqual(got, want) {
			t.Fatalf("npmUpdateArgs(%q) = %q, want %q", root, got, want)
		}
	}
}

func TestUpdateCheckCache(t *testing.T) {
	root := t.TempDir()
	t.Setenv(registryDirectoryEnvironment, filepath.Join(root, "runs"))
	t.Setenv(updateCheckDisableEnvironment, "")

	if !updateCheckIsStale() {
		t.Fatal("missing cache should be stale")
	}
	if _, ok := availableUpdate(); ok {
		t.Fatal("no update expected without a cache")
	}
	if err := writeUpdateCheck(updateCheck{CheckedAt: time.Now(), Latest: "99.0.0", Current: version}); err != nil {
		t.Fatal(err)
	}
	path, _ := updateCheckPath()
	if filepath.Dir(path) != root {
		t.Fatalf("cache written to %s", path)
	}
	if info, err := os.Stat(path); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("cache permissions: %v %v", info, err)
	}
	if updateCheckIsStale() {
		t.Fatal("fresh cache reported stale")
	}
	if latest, ok := availableUpdate(); !ok || latest != "99.0.0" {
		t.Fatalf("availableUpdate = %q %v", latest, ok)
	}
	t.Setenv(updateCheckDisableEnvironment, "1")
	if _, ok := availableUpdate(); ok {
		t.Fatal("disabled checks must not report updates")
	}
	t.Setenv(updateCheckDisableEnvironment, "")
	if err := writeUpdateCheck(updateCheck{CheckedAt: time.Now().Add(-48 * time.Hour), Latest: "99.0.0", Current: version}); err != nil {
		t.Fatal(err)
	}
	if !updateCheckIsStale() {
		t.Fatal("two-day-old cache should be stale")
	}
	if err := writeUpdateCheck(updateCheck{CheckedAt: time.Now(), Latest: version, Current: version}); err != nil {
		t.Fatal(err)
	}
	if _, ok := availableUpdate(); ok {
		t.Fatal("same version is not an update")
	}
}

func TestSwapExecutable(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "ruddr")
	writeFile(t, target, "old")
	if err := swapExecutable(target, []byte("new")); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(target)
	if err != nil || string(data) != "new" {
		t.Fatalf("got %q, %v", data, err)
	}
	if info, _ := os.Stat(target); info.Mode().Perm()&0o111 == 0 {
		t.Fatal("replacement is not executable")
	}
	entries, _ := os.ReadDir(dir)
	if len(entries) != 1 {
		t.Fatalf("temp file left behind: %d entries", len(entries))
	}
}

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestDetectCheckoutInstallChannel(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "package.json"), `{"name":"ruddr"}`)
	writeFile(t, filepath.Join(root, "scripts", "npm-binary.cjs"), "")
	writeFile(t, filepath.Join(root, "scripts", "install-local.sh"), "")
	if got := detectInstallChannel(filepath.Join(root, "ruddr")); got.Kind != "source" {
		t.Fatalf("checkout detected as %+v", got)
	}
}

type updateTestTransport func(*http.Request) (*http.Response, error)

func (f updateTestTransport) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

func TestFailedUpdateCheckIsCached(t *testing.T) {
	for _, latest := range []string{"", "99.0.0"} {
		t.Run("previous="+latest, func(t *testing.T) {
			t.Setenv(registryDirectoryEnvironment, filepath.Join(t.TempDir(), "runs"))
			t.Setenv(updateCheckDisableEnvironment, "")
			if latest != "" {
				if err := writeUpdateCheck(updateCheck{CheckedAt: time.Now().Add(-48 * time.Hour), Latest: latest, Current: version}); err != nil {
					t.Fatal(err)
				}
			}
			original := updateHTTPClient
			t.Cleanup(func() { updateHTTPClient = original })
			calls := 0
			updateHTTPClient = &http.Client{Transport: updateTestTransport(func(*http.Request) (*http.Response, error) {
				calls++
				return nil, errors.New("offline")
			})}
			refreshUpdateCheck(context.Background())
			refreshUpdateCheck(context.Background())
			if calls != 1 {
				t.Fatalf("made %d network calls, want 1", calls)
			}
			cached, ok := readUpdateCheck()
			if !ok || cached.Latest != latest || updateCheckIsStale() {
				t.Fatalf("unexpected cache: %+v, valid=%v", cached, ok)
			}
			if got, available := availableUpdate(); available != (latest != "") || got != latest {
				t.Fatalf("availableUpdate = %q, %v", got, available)
			}
		})
	}
}

func TestFutureUpdateCheckIsStale(t *testing.T) {
	t.Setenv(registryDirectoryEnvironment, filepath.Join(t.TempDir(), "runs"))
	if err := writeUpdateCheck(updateCheck{CheckedAt: time.Now().Add(48 * time.Hour), Current: version}); err != nil {
		t.Fatal(err)
	}
	if !updateCheckIsStale() {
		t.Fatal("future cache must not suppress update checks")
	}
}
