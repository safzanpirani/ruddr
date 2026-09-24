package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"
)

const (
	updateRepository              = "safzanpirani/ruddr"
	updateCheckInterval           = 24 * time.Hour
	updateCheckTimeout            = 3 * time.Second
	updateCheckDisableEnvironment = "RUDDR_NO_UPDATE_CHECK"
	updateAvailableEnvironment    = "RUDDR_UPDATE_AVAILABLE"
)

// updateCheck is the cached result of the last release lookup. It lives beside
// the run registry so the check happens at most once a day across every
// command.
type updateCheck struct {
	CheckedAt time.Time `json:"checkedAt"`
	Latest    string    `json:"latest"`
	Current   string    `json:"current"`
}

// installChannel describes how this executable reached the machine, which
// decides how it can be replaced.
type installChannel struct {
	Kind        string // npm, bun, binary, or source
	PackageRoot string // npm and bun: the global package directory
}

var updateHTTPClient = &http.Client{Timeout: 30 * time.Second}

func updateCommand(args []string) error {
	fs := flag.NewFlagSet("update", flag.ContinueOnError)
	checkOnly := fs.Bool("check", false, "report whether a newer release exists without installing it")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if len(fs.Args()) > 0 {
		return fmt.Errorf("unexpected update arguments %q", strings.Join(fs.Args(), " "))
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	latest, err := fetchLatestVersion(ctx)
	if err != nil {
		return fmt.Errorf("look up the latest release: %w", err)
	}
	_ = writeUpdateCheck(updateCheck{CheckedAt: time.Now(), Latest: latest, Current: version})
	if compareVersions(latest, version) <= 0 {
		fmt.Printf("ruddr %s is up to date\n", version)
		// Still sync the skill: it may predate this binary or have been edited.
		refreshInstalledSkill("")
		return nil
	}
	if *checkOnly {
		fmt.Printf("ruddr %s is available (installed %s); run `ruddr update` to install it\n", latest, version)
		return nil
	}
	executable, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate Ruddr executable: %w", err)
	}
	channel := detectInstallChannel(executable)
	fmt.Printf("updating ruddr %s -> %s via %s\n", version, latest, channel.Kind)
	switch channel.Kind {
	case "npm":
		err = runPackageManagerUpdate("npm", []string{"install", "-g", "ruddr@" + latest})
	case "bun":
		err = runPackageManagerUpdate("bun", []string{"add", "-g", "ruddr@" + latest})
	case "source":
		refreshInstalledSkill("")
		return errors.New("this Ruddr was installed from a source checkout; run `git pull` there and rerun scripts/install-local.sh")
	default:
		err = replaceExecutable(ctx, executable, latest)
	}
	if err != nil {
		return err
	}
	// The new binary carries the current skill text. Package-manager
	// postinstall hooks can be disabled, so do not rely on them.
	refreshInstalledSkill(executable)
	return nil
}

// refreshInstalledSkill reinstalls the delegate skill into the default agent
// skill directories. With an executable it asks that binary to do it, so an
// update installs the new release's skill rather than this binary's copy. A
// failure is reported but does not fail the update.
func refreshInstalledSkill(executable string) {
	if executable == "" {
		targets, err := defaultSkillDirectories()
		if err == nil {
			err = installDelegateSkillInto(targets)
		}
		if err != nil {
			fmt.Fprintf(os.Stderr, "ruddr: skill install failed: %v\n", err)
		}
		return
	}
	refresh := exec.Command(executable, "skill", "install")
	refresh.Stdout = os.Stdout
	refresh.Stderr = os.Stderr
	if err := refresh.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "ruddr: skill install after update failed: %v\n", err)
	}
}

func runPackageManagerUpdate(tool string, args []string) error {
	path, err := exec.LookPath(tool)
	if err != nil {
		return fmt.Errorf("%s is not on PATH; install the update with `%s %s`", tool, tool, strings.Join(args, " "))
	}
	cmd := exec.Command(path, args...)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%s %s: %w", tool, strings.Join(args, " "), err)
	}
	fmt.Println("ruddr updated; restart any running `ruddr tui` to use it")
	return nil
}

// detectInstallChannel inspects the directories around the executable. The npm
// package keeps the binary beside its package.json and launcher scripts; a
// source install keeps the TUI assets under the data home instead.
func detectInstallChannel(executable string) installChannel {
	dir := filepath.Dir(executable)
	if isRuddrPackageRoot(dir) {
		if _, err := os.Stat(filepath.Join(dir, "scripts", "install-local.sh")); err == nil {
			return installChannel{Kind: "source"}
		}
		kind := "npm"
		normalized := filepath.ToSlash(dir)
		if strings.Contains(normalized, "/.bun/") {
			kind = "bun"
		}
		return installChannel{Kind: kind, PackageRoot: dir}
	}
	if dataHome, err := ruddrDataHome(); err == nil {
		if _, err := os.Stat(filepath.Join(dataHome, "ruddr", "tui", "index.ts")); err == nil {
			return installChannel{Kind: "source"}
		}
	}
	return installChannel{Kind: "binary"}
}

func isRuddrPackageRoot(dir string) bool {
	data, err := os.ReadFile(filepath.Join(dir, "package.json"))
	if err != nil {
		return false
	}
	var manifest struct {
		Name string `json:"name"`
	}
	if json.Unmarshal(data, &manifest) != nil || manifest.Name != "ruddr" {
		return false
	}
	_, err = os.Stat(filepath.Join(dir, "scripts", "npm-binary.cjs"))
	return err == nil
}

func releaseAssetName() string {
	name := "ruddr-" + runtime.GOOS + "-" + runtime.GOARCH
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	return name
}

func releaseDownloadURL(tag, asset string) string {
	return "https://github.com/" + updateRepository + "/releases/download/" + tag + "/" + asset
}

// replaceExecutable downloads the release binary for this platform, verifies
// it against the published checksums, and swaps it into place atomically.
func replaceExecutable(ctx context.Context, executable, latest string) error {
	tag := "v" + latest
	asset := releaseAssetName()
	checksums, err := fetchBytes(ctx, releaseDownloadURL(tag, "checksums.txt"))
	if err != nil {
		return fmt.Errorf("download checksums: %w", err)
	}
	expected, ok := checksumFor(string(checksums), asset)
	if !ok {
		return fmt.Errorf("release %s has no prebuilt binary for %s/%s", tag, runtime.GOOS, runtime.GOARCH)
	}
	fmt.Printf("downloading %s\n", asset)
	binary, err := fetchBytes(ctx, releaseDownloadURL(tag, asset))
	if err != nil {
		return fmt.Errorf("download %s: %w", asset, err)
	}
	digest := sha256.Sum256(binary)
	if actual := hex.EncodeToString(digest[:]); actual != expected {
		return fmt.Errorf("%s checksum mismatch: expected %s, got %s", asset, expected, actual)
	}
	if err := swapExecutable(executable, binary); err != nil {
		return err
	}
	fmt.Printf("ruddr %s installed at %s\n", latest, executable)
	return nil
}

func checksumFor(checksums, asset string) (string, bool) {
	for _, line := range strings.Split(checksums, "\n") {
		fields := strings.Fields(line)
		if len(fields) != 2 {
			continue
		}
		if strings.TrimPrefix(fields[1], "*") == asset {
			return strings.ToLower(fields[0]), true
		}
	}
	return "", false
}

func swapExecutable(executable string, binary []byte) error {
	dir := filepath.Dir(executable)
	tmp, err := os.CreateTemp(dir, ".ruddr-update-*")
	if err != nil {
		return fmt.Errorf("cannot write next to %s: %w", executable, err)
	}
	tmpPath := tmp.Name()
	if _, err := tmp.Write(binary); err != nil {
		tmp.Close()
		os.Remove(tmpPath)
		return err
	}
	if err := tmp.Close(); err != nil {
		os.Remove(tmpPath)
		return err
	}
	if err := os.Chmod(tmpPath, 0o755); err != nil {
		os.Remove(tmpPath)
		return err
	}
	if runtime.GOOS == "windows" {
		// A running Windows executable cannot be overwritten, but it can be
		// renamed away first.
		old := executable + ".old"
		os.Remove(old)
		if err := os.Rename(executable, old); err != nil {
			os.Remove(tmpPath)
			return fmt.Errorf("move the current binary aside: %w", err)
		}
	}
	if err := os.Rename(tmpPath, executable); err != nil {
		os.Remove(tmpPath)
		return fmt.Errorf("replace %s: %w", executable, err)
	}
	return nil
}

func fetchBytes(ctx context.Context, url string) ([]byte, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	request.Header.Set("User-Agent", "ruddr/"+version)
	response, err := updateHTTPClient.Do(request)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("%s returned %s", url, response.Status)
	}
	return io.ReadAll(io.LimitReader(response.Body, 256<<20))
}

// fetchLatestVersion asks GitHub for the newest release tag. It goes through
// the redirecting latest URL rather than the API so unauthenticated checks are
// not rate-limited.
func fetchLatestVersion(ctx context.Context) (string, error) {
	url := "https://github.com/" + updateRepository + "/releases/latest"
	request, err := http.NewRequestWithContext(ctx, http.MethodHead, url, nil)
	if err != nil {
		return "", err
	}
	request.Header.Set("User-Agent", "ruddr/"+version)
	client := &http.Client{
		Timeout:   updateHTTPClient.Timeout,
		Transport: updateHTTPClient.Transport,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	response, err := client.Do(request)
	if err != nil {
		return "", err
	}
	response.Body.Close()
	location := response.Header.Get("Location")
	if location == "" {
		return "", fmt.Errorf("%s returned %s without a release redirect", url, response.Status)
	}
	return versionFromReleaseURL(location)
}

func versionFromReleaseURL(location string) (string, error) {
	marker := "/releases/tag/"
	index := strings.LastIndex(location, marker)
	if index < 0 {
		return "", fmt.Errorf("unexpected release location %q", location)
	}
	tag := strings.TrimPrefix(location[index+len(marker):], "v")
	if _, ok := parseVersion(tag); !ok {
		return "", fmt.Errorf("unexpected release tag %q", tag)
	}
	return tag, nil
}

func parseVersion(value string) ([3]int, bool) {
	var parsed [3]int
	value = strings.TrimPrefix(strings.TrimSpace(value), "v")
	if cut := strings.IndexAny(value, "-+"); cut >= 0 {
		value = value[:cut]
	}
	parts := strings.Split(value, ".")
	if len(parts) == 0 || len(parts) > 3 {
		return parsed, false
	}
	for index, part := range parts {
		number, err := strconv.Atoi(part)
		if err != nil || number < 0 {
			return parsed, false
		}
		parsed[index] = number
	}
	return parsed, true
}

// compareVersions orders two dotted versions; unparsable values sort lowest.
func compareVersions(a, b string) int {
	left, leftOK := parseVersion(a)
	right, rightOK := parseVersion(b)
	if !leftOK || !rightOK {
		switch {
		case leftOK:
			return 1
		case rightOK:
			return -1
		default:
			return 0
		}
	}
	for index := range left {
		if left[index] != right[index] {
			if left[index] < right[index] {
				return -1
			}
			return 1
		}
	}
	return 0
}

func updateChecksDisabled() bool {
	return os.Getenv(updateCheckDisableEnvironment) == "1"
}

func updateCheckPath() (string, error) {
	runs, err := runRegistryDirectory()
	if err != nil {
		return "", err
	}
	return filepath.Join(filepath.Dir(runs), "update-check.json"), nil
}

func readUpdateCheck() (updateCheck, bool) {
	path, err := updateCheckPath()
	if err != nil {
		return updateCheck{}, false
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return updateCheck{}, false
	}
	var check updateCheck
	if json.Unmarshal(data, &check) != nil || check.CheckedAt.IsZero() {
		return updateCheck{}, false
	}
	return check, true
}

func writeUpdateCheck(check updateCheck) error {
	path, err := updateCheckPath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	data, err := json.Marshal(check)
	if err != nil {
		return err
	}
	return writePrivateFile(path, data)
}

// availableUpdate reports a newer cached release, if the last check found one.
func availableUpdate() (string, bool) {
	if updateChecksDisabled() {
		return "", false
	}
	check, ok := readUpdateCheck()
	if !ok || compareVersions(check.Latest, version) <= 0 {
		return "", false
	}
	return check.Latest, true
}

func updateCheckIsStale() bool {
	check, ok := readUpdateCheck()
	age := time.Since(check.CheckedAt)
	return !ok || check.Current != version || age < 0 || age > updateCheckInterval
}

// refreshUpdateCheck performs a bounded network lookup when the cached result
// is older than a day. Failed attempts also count toward the daily interval.
func refreshUpdateCheck(ctx context.Context) {
	if updateChecksDisabled() || !updateCheckIsStale() {
		return
	}
	ctx, cancel := context.WithTimeout(ctx, updateCheckTimeout)
	defer cancel()
	latest, err := fetchLatestVersion(ctx)
	if err != nil {
		// Keep the last known release available during an outage.
		previous, _ := readUpdateCheck()
		latest = previous.Latest
	}
	_ = writeUpdateCheck(updateCheck{CheckedAt: time.Now(), Latest: latest, Current: version})
}

func printUpdateNotice() {
	if latest, ok := availableUpdate(); ok {
		fmt.Fprintf(os.Stderr, "ruddr %s is available (installed %s); run `ruddr update` to install it\n", latest, version)
	}
}
