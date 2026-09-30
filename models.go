package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

const modelsFileEnvironment = "RUDDR_MODELS_FILE"

// providerModel is one selectable entry in the model catalog. The catalog is
// the single source of truth for provider defaults and for the TUI's picker.
type providerModel struct {
	Provider      string   `json:"provider"`
	ID            string   `json:"id,omitempty"`
	Label         string   `json:"label,omitempty"`
	Efforts       []string `json:"efforts,omitempty"`
	ContextWindow int64    `json:"contextWindow,omitempty"`
	Default       bool     `json:"default,omitempty"`
	Available     bool     `json:"available"`
	// Config holds Codex config overrides applied when a run uses this model.
	Config map[string]string `json:"config,omitempty"`
	Note   string            `json:"note,omitempty"`
	// Source is "config" for entries added or changed by models.json.
	Source string `json:"source,omitempty"`
}

// modelEntry is one models.json entry. It adds a model, changes a built-in
// one, makes one the provider default, or hides a built-in model.
type modelEntry struct {
	Provider      string   `json:"provider"`
	ID            string   `json:"id"`
	Label         string   `json:"label,omitempty"`
	Efforts       []string `json:"efforts,omitempty"`
	ContextWindow int64    `json:"contextWindow,omitempty"`
	Default       bool     `json:"default,omitempty"`
	Hidden        bool     `json:"hidden,omitempty"`
	// Config is passed to codex app-server as -c KEY=VALUE for runs that use
	// this model, for settings in ~/.codex/config.toml the model rejects.
	Config map[string]string `json:"config,omitempty"`
}

type modelsFile struct {
	Models []modelEntry `json:"models"`
}

var codexEfforts = []string{"none", "low", "medium", "high", "xhigh", "max"}
var piEfforts = []string{"off", "minimal", "low", "medium", "high", "xhigh", "max"}

var modelCatalog = []providerModel{
	{Provider: providerCodex, ID: "gpt-6-astra", Label: "GPT-6-Astra", Efforts: codexEfforts, Default: true, Available: true},
	{Provider: providerCodex, ID: "gpt-6.1-sol", Label: "GPT-6.1-Sol", Efforts: []string{"low", "medium", "high", "xhigh", "max", "ultra"}, Available: true},
	{Provider: providerCodex, ID: "gpt-6-sol", Label: "GPT-6-Sol", Efforts: []string{"low", "medium", "high", "xhigh", "max", "ultra"}, Available: true},
	{Provider: providerCodex, ID: "gpt-6-luna", Label: "GPT-6-Luna", Efforts: []string{"low", "medium", "high", "xhigh", "max"}, Available: true},
	{Provider: providerCodex, ID: "gpt-5.6-sol", Label: "GPT-5.6-Sol", Efforts: codexEfforts, Available: true},
	{Provider: providerCodex, ID: "gpt-5.6-terra", Label: "GPT-5.6-Terra", Efforts: codexEfforts, Available: true},
	{Provider: providerCodex, ID: "gpt-5.6-luna", Label: "GPT-5.6-Luna", Efforts: codexEfforts, Available: true},
	{Provider: providerClaude, ID: "claude-fable-5-1", Label: "Claude Fable 5.1", Available: true},
	{Provider: providerClaude, ID: "claude-fable-5", Label: "Claude Fable 5", Available: true},
	{Provider: providerClaude, ID: "claude-opus-5-5", Label: "Claude Opus 5.5", Default: true, Available: true},
	{Provider: providerClaude, ID: "claude-opus-5", Label: "Claude Opus 5", Available: true},
	{Provider: providerClaude, ID: "claude-sonnet-5", Label: "Claude Sonnet 5", Available: true},
	{Provider: providerClaude, ID: "claude-haiku-4-5-20251001", Label: "Claude Haiku 4.5", Available: true},
	{Provider: providerOpenCode, ID: "openrouter/deepseek/deepseek-v4-flash-vision-exp", Label: "DeepSeek V4 Flash Vision Exp", Default: true, Available: true},
	{Provider: providerPi, ID: "openrouter/deepseek/deepseek-v4-flash-vision-exp", Label: "DeepSeek V4 Flash Vision Exp", Efforts: piEfforts, Default: true, Available: true},
	{Provider: providerDroid, ID: "glm-5.3-flash", Label: "GLM-5.3-Flash", Efforts: []string{"low", "high", "max"}, Default: true, Available: true},
}

// defaultModel returns the provider's default after applying models.json.
func defaultModel(provider string) (string, error) {
	catalog, err := loadModelCatalog()
	if err != nil {
		return "", err
	}
	for _, model := range catalog {
		if model.Provider == provider && model.Default {
			return model.ID, nil
		}
	}
	return "", nil
}

func modelsFilePath() (string, error) {
	if configured := os.Getenv(modelsFileEnvironment); configured != "" {
		return filepath.Abs(configured)
	}
	configHome := os.Getenv("XDG_CONFIG_HOME")
	if configHome == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		configHome = filepath.Join(home, ".config")
	}
	return filepath.Join(configHome, "ruddr", "models.json"), nil
}

// readModelsFile returns the user's entries; a missing file means none. An
// invalid file is an error, because silently ignoring it would run the wrong
// default model.
func readModelsFile() (modelsFile, string, error) {
	path, err := modelsFilePath()
	if err != nil {
		return modelsFile{}, "", err
	}
	raw, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return modelsFile{}, path, nil
	}
	if err != nil {
		return modelsFile{}, path, err
	}
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	var file modelsFile
	if err := decoder.Decode(&file); err != nil {
		return modelsFile{}, path, fmt.Errorf("%s: %w", path, err)
	}
	defaults := map[string]string{}
	for index, entry := range file.Models {
		if _, err := normalizeProvider(entry.Provider); err != nil || entry.Provider == "" {
			return modelsFile{}, path, fmt.Errorf("%s: models[%d]: unsupported provider %q", path, index, entry.Provider)
		}
		if strings.TrimSpace(entry.ID) == "" {
			return modelsFile{}, path, fmt.Errorf("%s: models[%d]: id is required", path, index)
		}
		if len(entry.Config) > 0 && entry.Provider != providerCodex {
			return modelsFile{}, path, fmt.Errorf("%s: models[%d]: config applies only to codex models", path, index)
		}
		for key := range entry.Config {
			if strings.TrimSpace(key) == "" || strings.Contains(key, "=") {
				return modelsFile{}, path, fmt.Errorf("%s: models[%d]: invalid config key %q", path, index, key)
			}
		}
		if entry.Default && entry.Hidden {
			return modelsFile{}, path, fmt.Errorf("%s: models[%d]: a hidden model cannot be the default", path, index)
		}
		if entry.Default {
			if previous, ok := defaults[entry.Provider]; ok {
				return modelsFile{}, path, fmt.Errorf("%s: %s has two defaults: %s and %s", path, entry.Provider, previous, entry.ID)
			}
			defaults[entry.Provider] = entry.ID
		}
	}
	return file, path, nil
}

// loadModelCatalog merges models.json over the built-in catalog.
func loadModelCatalog() ([]providerModel, error) {
	file, _, err := readModelsFile()
	if err != nil {
		return nil, err
	}
	return mergeModelCatalog(modelCatalog, file.Models), nil
}

func mergeModelCatalog(builtin []providerModel, entries []modelEntry) []providerModel {
	catalog := make([]providerModel, 0, len(builtin)+len(entries))
	for _, model := range builtin {
		model.Efforts = append([]string(nil), model.Efforts...)
		catalog = append(catalog, model)
	}
	for _, entry := range entries {
		index := -1
		for candidate, model := range catalog {
			if model.Provider == entry.Provider && model.ID == entry.ID {
				index = candidate
				break
			}
		}
		if entry.Hidden {
			if index >= 0 {
				catalog = append(catalog[:index], catalog[index+1:]...)
			}
			continue
		}
		if index < 0 {
			catalog = append(catalog, providerModel{Provider: entry.Provider, ID: entry.ID, Available: true})
			index = len(catalog) - 1
		}
		model := &catalog[index]
		model.Source = "config"
		if entry.Label != "" {
			model.Label = entry.Label
		}
		if len(entry.Efforts) > 0 {
			model.Efforts = append([]string(nil), entry.Efforts...)
		}
		if entry.ContextWindow > 0 {
			model.ContextWindow = entry.ContextWindow
		}
		if len(entry.Config) > 0 {
			model.Config = map[string]string{}
			for key, value := range entry.Config {
				model.Config[key] = value
			}
		}
		if entry.Default {
			for other := range catalog {
				if catalog[other].Provider == entry.Provider {
					catalog[other].Default = false
				}
			}
			model.Default = true
		}
	}
	return catalog
}

func writeModelsFile(path string, file modelsFile) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	raw, err := json.MarshalIndent(file, "", "  ")
	if err != nil {
		return err
	}
	return writePrivateFile(path, append(raw, '\n'))
}

func modelsCommand(args []string) error {
	if len(args) > 0 {
		switch args[0] {
		case "add", "default", "remove":
			return modelsEditCommand(args[0], args[1:])
		case "path":
			path, err := modelsFilePath()
			if err != nil {
				return err
			}
			fmt.Println(path)
			return nil
		}
	}
	fs := flag.NewFlagSet("models", flag.ContinueOnError)
	var asJSON bool
	fs.BoolVar(&asJSON, "json", false, "print the catalog as JSON")
	if err := fs.Parse(args); err != nil {
		return usageError(err)
	}
	if len(fs.Args()) > 0 {
		return fmt.Errorf("unknown models subcommand %q; expected add, default, remove, or path", fs.Arg(0))
	}
	catalog, err := loadModelCatalog()
	if err != nil {
		return err
	}
	if asJSON {
		return printJSON(catalog)
	}
	for _, model := range catalog {
		if !model.Available {
			fmt.Printf("%s (%s)\n", model.Provider, model.Note)
			continue
		}
		marker := " "
		if model.Default {
			marker = "*"
		}
		suffix := ""
		if model.Source == "config" {
			suffix = "  (models.json)"
		}
		fmt.Printf("%s %s %s%s\n", marker, model.Provider, model.ID, suffix)
	}
	return nil
}

// modelsEditCommand implements add, default, and remove. Flags may appear
// before or after the PROVIDER and ID arguments.
func modelsEditCommand(action string, args []string) error {
	fs := flag.NewFlagSet("models "+action, flag.ContinueOnError)
	var label, efforts string
	var makeDefault bool
	var setConfig configOverrides
	var unsetConfig stringList
	if action == "add" {
		fs.StringVar(&label, "label", "", "display name")
		fs.StringVar(&efforts, "efforts", "", "comma-separated reasoning efforts the model accepts")
		fs.BoolVar(&makeDefault, "default", false, "make it the provider default")
		fs.Var(&setConfig, "config", "Codex config override KEY=VALUE for runs on this model (repeatable)")
		fs.Var(&unsetConfig, "unset-config", "remove a Codex config override KEY (repeatable)")
	}
	var positional []string
	for {
		if err := fs.Parse(args); err != nil {
			return usageError(err)
		}
		if fs.NArg() == 0 {
			break
		}
		positional = append(positional, fs.Arg(0))
		args = fs.Args()[1:]
	}
	if len(positional) != 2 {
		return fmt.Errorf("usage: ruddr models %s PROVIDER MODEL_ID", action)
	}
	provider, id := positional[0], positional[1]
	if _, err := normalizeProvider(provider); err != nil || provider == "" {
		return fmt.Errorf("unsupported provider %q; expected codex, claude, opencode, or pi", provider)
	}
	if (len(setConfig) > 0 || len(unsetConfig) > 0) && provider != providerCodex {
		return usageError(errors.New("--config applies only to codex models"))
	}
	file, path, err := readModelsFile()
	if err != nil {
		return err
	}
	find := func() int {
		for index, entry := range file.Models {
			if entry.Provider == provider && entry.ID == id {
				return index
			}
		}
		return -1
	}
	builtin := false
	for _, model := range modelCatalog {
		if model.Provider == provider && model.ID == id {
			builtin = true
		}
	}
	clearDefaults := func() {
		for index := range file.Models {
			if file.Models[index].Provider == provider {
				file.Models[index].Default = false
			}
		}
	}
	index := find()
	var message string
	switch action {
	case "add":
		if index < 0 {
			file.Models = append(file.Models, modelEntry{Provider: provider, ID: id})
			index = len(file.Models) - 1
		}
		entry := &file.Models[index]
		entry.Hidden = false
		if label != "" {
			entry.Label = label
		}
		if efforts != "" {
			entry.Efforts = nil
			for _, effort := range strings.Split(efforts, ",") {
				if effort = strings.TrimSpace(effort); effort != "" {
					entry.Efforts = append(entry.Efforts, effort)
				}
			}
		}
		for _, override := range setConfig {
			key, value, _ := strings.Cut(override, "=")
			if entry.Config == nil {
				entry.Config = map[string]string{}
			}
			entry.Config[key] = value
		}
		for _, key := range unsetConfig {
			delete(entry.Config, key)
		}
		if len(entry.Config) == 0 {
			entry.Config = nil
		}
		if makeDefault {
			clearDefaults()
			file.Models[index].Default = true
		}
		message = fmt.Sprintf("added %s %s", provider, id)
		if makeDefault {
			message += " as the default"
		}
	case "default":
		catalog := mergeModelCatalog(modelCatalog, file.Models)
		known := false
		for _, model := range catalog {
			known = known || (model.Provider == provider && model.ID == id)
		}
		if !known {
			return fmt.Errorf("%s %s is not in the catalog; add it with `ruddr models add %s %s --default`", provider, id, provider, id)
		}
		clearDefaults()
		if index < 0 {
			file.Models = append(file.Models, modelEntry{Provider: provider, ID: id})
			index = len(file.Models) - 1
		}
		file.Models[index].Default = true
		message = fmt.Sprintf("%s now defaults to %s", provider, id)
	case "remove":
		switch {
		case builtin:
			// Built-in models cannot be deleted, only hidden.
			if index < 0 {
				file.Models = append(file.Models, modelEntry{Provider: provider, ID: id})
				index = len(file.Models) - 1
			}
			file.Models[index] = modelEntry{Provider: provider, ID: id, Hidden: true}
			message = fmt.Sprintf("hid built-in %s %s", provider, id)
		case index >= 0:
			file.Models = append(file.Models[:index], file.Models[index+1:]...)
			message = fmt.Sprintf("removed %s %s", provider, id)
		default:
			return fmt.Errorf("%s %s is not in %s", provider, id, path)
		}
	}
	if err := writeModelsFile(path, file); err != nil {
		return err
	}
	fmt.Printf("%s (%s)\n", message, path)
	return nil
}

// configOverrides is a repeatable KEY=VALUE flag for Codex config overrides.
type configOverrides []string

func (c *configOverrides) String() string { return strings.Join(*c, ",") }

func (c *configOverrides) Set(value string) error {
	key, _, found := strings.Cut(value, "=")
	if !found || strings.TrimSpace(key) == "" {
		return fmt.Errorf("config override %q must be KEY=VALUE", value)
	}
	*c = append(*c, value)
	return nil
}

// modelCodexConfig returns the catalog's config overrides for a Codex model
// as sorted KEY=VALUE strings, so the child command is deterministic.
func modelCodexConfig(model string) ([]string, error) {
	catalog, err := loadModelCatalog()
	if err != nil {
		return nil, err
	}
	for _, entry := range catalog {
		if entry.Provider != providerCodex || entry.ID != model || len(entry.Config) == 0 {
			continue
		}
		keys := make([]string, 0, len(entry.Config))
		for key := range entry.Config {
			keys = append(keys, key)
		}
		sort.Strings(keys)
		overrides := make([]string, len(keys))
		for i, key := range keys {
			overrides[i] = key + "=" + entry.Config[key]
		}
		return overrides, nil
	}
	return nil, nil
}
