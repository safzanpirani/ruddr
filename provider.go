package main

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

const (
	providerCodex    = "codex"
	providerClaude   = "claude"
	providerOpenCode = "opencode"
	providerPi       = "pi"

	claudeAdapterEntryEnvironment         = "RUDDR_CLAUDE_ADAPTER_ENTRY"
	previousClaudeAdapterEntryEnvironment = "RUDDER_CLAUDE_ADAPTER_ENTRY"
	legacyClaudeAdapterEntryEnvironment   = "CODEX_RUDDER_CLAUDE_ADAPTER_ENTRY"

	claudePathEnvironment           = "RUDDR_CLAUDE_PATH"
	opencodePathEnvironment         = "RUDDR_OPENCODE_PATH"
	piPathEnvironment               = "RUDDR_PI_PATH"
	opencodeAdapterEntryEnvironment = "RUDDR_OPENCODE_ADAPTER_ENTRY"
	piAdapterEntryEnvironment       = "RUDDR_PI_ADAPTER_ENTRY"
)

func normalizeProvider(provider string) (string, error) {
	if provider == "" {
		return providerCodex, nil
	}
	switch provider {
	case providerCodex, providerClaude, providerOpenCode, providerPi:
		return provider, nil
	default:
		return "", fmt.Errorf("unsupported provider %q; expected codex, claude, opencode, or pi", provider)
	}
}

func configureProviderDefaults(cfg *runConfig, childArgs []string) error {
	provider, err := normalizeProvider(cfg.Provider)
	if err != nil {
		return err
	}
	cfg.Provider = provider

	switch provider {
	case providerCodex:
		if cfg.Model == "" {
			if cfg.Model, err = defaultModel(providerCodex); err != nil {
				return err
			}
		}
		if len(childArgs) > 0 {
			cfg.ChildCommand = childArgs
		} else {
			cfg.ChildCommand = []string{"codex", "app-server", "--listen", "stdio://"}
		}
	case providerClaude:
		if len(childArgs) > 0 {
			return errors.New("a command after -- is supported only for Codex; use --claude-path for Claude Code")
		}
		if cfg.ClaudePath == "" {
			cfg.ClaudePath = os.Getenv(claudePathEnvironment)
		}
		if cfg.Model == "" {
			if cfg.Model, err = defaultModel(providerClaude); err != nil {
				return err
			}
		}
		bunPath, err := exec.LookPath("bun")
		if err != nil {
			return errors.New("Claude support requires Bun 1.4 or newer")
		}
		entry, err := findClaudeAdapterEntry()
		if err != nil {
			return err
		}
		cfg.ChildCommand = []string{bunPath, "run", entry}
	case providerOpenCode, providerPi:
		if len(childArgs) > 0 {
			return fmt.Errorf("a command after -- is supported only for Codex; use --%s-path for %s", provider, provider)
		}
		if cfg.Model == "" {
			if cfg.Model, err = defaultModel(provider); err != nil {
				return err
			}
		}
		pathEnvironment := opencodePathEnvironment
		entryEnvironment := opencodeAdapterEntryEnvironment
		executableNames := []string{"opencode2", "opencode-next"}
		entryParts := []string{"opencode", "app-server.ts"}
		cfg.ProviderPath = cfg.OpenCodePath
		if provider == providerPi {
			pathEnvironment = piPathEnvironment
			entryEnvironment = piAdapterEntryEnvironment
			executableNames = []string{"pi"}
			entryParts = []string{"pi", "app-server.ts"}
			cfg.ProviderPath = cfg.PiPath
		}
		if cfg.ProviderPath == "" {
			cfg.ProviderPath = getenvAny(pathEnvironment, previousName(pathEnvironment))
		}
		if cfg.ProviderPath == "" {
			for _, name := range executableNames {
				if executable, lookupErr := exec.LookPath(name); lookupErr == nil {
					cfg.ProviderPath = executable
					break
				}
			}
		}
		if cfg.ProviderPath == "" {
			return fmt.Errorf("%s support requires %s on PATH or %s", provider, strings.Join(executableNames, " or "), pathEnvironment)
		}
		bunPath, err := exec.LookPath("bun")
		if err != nil {
			return fmt.Errorf("%s support requires Bun 1.4 or newer", provider)
		}
		entry, err := findAdapterEntry(entryEnvironment, entryParts...)
		if err != nil {
			return err
		}
		cfg.ChildCommand = []string{bunPath, "run", entry}
	}
	return nil
}

func findAdapterEntry(environment string, parts ...string) (string, error) {
	var candidates []string
	if configured := getenvAny(environment, previousName(environment)); configured != "" {
		candidates = append(candidates, configured)
	}
	if executable, err := os.Executable(); err == nil {
		candidates = appendRuntimeSiblingCandidates(candidates, executable, parts...)
	}
	if dataHome, err := ruddrDataHome(); err == nil {
		for _, name := range installDirectoryNames {
			candidates = append(candidates, filepath.Join(append([]string{dataHome, name}, parts...)...))
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
	return "", fmt.Errorf("cannot locate %s; run the installer or set %s", filepath.Join(parts...), environment)
}

func findClaudeAdapterEntry() (string, error) {
	var candidates []string
	for _, environment := range []string{claudeAdapterEntryEnvironment, previousClaudeAdapterEntryEnvironment, legacyClaudeAdapterEntryEnvironment} {
		if configured := os.Getenv(environment); configured != "" {
			candidates = append(candidates, configured)
		}
	}
	if executable, err := os.Executable(); err == nil {
		candidates = appendRuntimeSiblingCandidates(candidates, executable, "claude", "app-server.ts")
	}
	if dataHome, err := ruddrDataHome(); err == nil {
		for _, name := range installDirectoryNames {
			candidates = append(candidates, filepath.Join(dataHome, name, "claude", "app-server.ts"))
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
	return "", fmt.Errorf("cannot locate claude/app-server.ts; run the installer or set %s", claudeAdapterEntryEnvironment)
}

// installDirectoryNames lists install roots newest first: the current name,
// then the names earlier releases used.
var installDirectoryNames = []string{"ruddr", "rudder", "codex-rudder"}

// previousName maps a RUDDR_* variable to the RUDDER_* spelling that earlier
// releases read, so existing shell configuration keeps working.
func previousName(environment string) string {
	return "RUDDER_" + strings.TrimPrefix(environment, "RUDDR_")
}

func appendRuntimeSiblingCandidates(candidates []string, executable string, parts ...string) []string {
	candidates = append(candidates, filepath.Join(append([]string{filepath.Dir(executable)}, parts...)...))
	if resolved, err := filepath.EvalSymlinks(executable); err == nil && resolved != executable {
		candidates = append(candidates, filepath.Join(append([]string{filepath.Dir(resolved)}, parts...)...))
	}
	return candidates
}
