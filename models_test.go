package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestModelCatalogDefaults(t *testing.T) {
	t.Setenv(modelsFileEnvironment, filepath.Join(t.TempDir(), "models.json"))
	if got, _ := defaultModel(providerCodex); got != "gpt-6-astra" {
		t.Fatalf("codex default = %q", got)
	}
	if got, _ := defaultModel(providerClaude); got != "claude-opus-5-5" {
		t.Fatalf("claude default = %q", got)
	}
	defaults := map[string]int{}
	sawClaudeFable51 := false
	sawOpencode := false
	sawPi := false
	for _, model := range modelCatalog {
		if model.Default {
			defaults[model.Provider]++
		}
		if model.Available && model.ID == "" {
			t.Fatalf("available model without id: %#v", model)
		}
		if model.Provider == providerClaude && model.ID == "claude-fable-5-1" {
			sawClaudeFable51 = model.Available
		}
		if model.Provider == "opencode" {
			sawOpencode = true
			if !model.Available || model.ID == "" {
				t.Fatal("opencode adapter needs an available model")
			}
		}
		if model.Provider == "pi" {
			sawPi = true
			if !model.Available || model.ID == "" {
				t.Fatal("Pi adapter needs an available model")
			}
			if len(model.Efforts) != 7 || model.Efforts[0] != "off" || model.Efforts[1] != "minimal" {
				t.Fatalf("Pi efforts = %#v", model.Efforts)
			}
		}
	}
	if defaults[providerCodex] != 1 || defaults[providerClaude] != 1 || defaults[providerOpenCode] != 1 || defaults[providerPi] != 1 {
		t.Fatalf("defaults per provider = %#v, want exactly one each", defaults)
	}
	if !sawOpencode || !sawPi {
		t.Fatal("catalog is missing an external provider")
	}
	if !sawClaudeFable51 {
		t.Fatal("catalog is missing Claude Fable 5.1")
	}
}

func TestModelsFileAddsOverridesAndHidesModels(t *testing.T) {
	path := filepath.Join(t.TempDir(), "models.json")
	t.Setenv(modelsFileEnvironment, path)
	for _, args := range [][]string{
		{"add", "opencode", "opencode/deepseek-v4-flash", "--label", "Zen Flash", "--default"},
		{"add", "--efforts", "low,high", "codex", "gpt-7-preview"},
		{"remove", "codex", "gpt-5.6-luna"},
		{"default", "claude", "claude-sonnet-5"},
	} {
		if err := modelsCommand(args); err != nil {
			t.Fatalf("models %q: %v", args, err)
		}
	}
	catalog, err := loadModelCatalog()
	if err != nil {
		t.Fatal(err)
	}
	find := func(provider, id string) *providerModel {
		for index := range catalog {
			if catalog[index].Provider == provider && catalog[index].ID == id {
				return &catalog[index]
			}
		}
		return nil
	}
	if model := find("opencode", "opencode/deepseek-v4-flash"); model == nil || !model.Default || model.Label != "Zen Flash" || model.Source != "config" {
		t.Fatalf("added opencode model = %+v", model)
	}
	if got, _ := defaultModel(providerOpenCode); got != "opencode/deepseek-v4-flash" {
		t.Fatalf("opencode default = %q", got)
	}
	if model := find("codex", "gpt-7-preview"); model == nil || strings.Join(model.Efforts, ",") != "low,high" || model.Default {
		t.Fatalf("added codex model = %+v", model)
	}
	if find("codex", "gpt-5.6-luna") != nil {
		t.Fatal("removed built-in model is still listed")
	}
	if got, _ := defaultModel(providerClaude); got != "claude-sonnet-5" {
		t.Fatalf("claude default = %q", got)
	}
	defaults := map[string]int{}
	for _, model := range catalog {
		if model.Default {
			defaults[model.Provider]++
		}
	}
	for provider, count := range defaults {
		if count != 1 {
			t.Fatalf("%s has %d defaults", provider, count)
		}
	}
	if err := modelsCommand([]string{"remove", "codex", "gpt-7-preview"}); err != nil {
		t.Fatal(err)
	}
	if err := modelsCommand([]string{"default", "codex", "gpt-7-preview"}); err == nil {
		t.Fatal("default accepted a model that is not in the catalog")
	}
}

func TestInvalidModelsFileFailsLoudly(t *testing.T) {
	path := filepath.Join(t.TempDir(), "models.json")
	t.Setenv(modelsFileEnvironment, path)
	for _, content := range []string{
		`{"models":[{"provider":"codex","id":"x","defualt":true}]}`,
		`{"models":[{"provider":"nope","id":"x"}]}`,
		`{"models":[{"provider":"codex","id":""}]}`,
		`{"models":[{"provider":"codex","id":"a","default":true},{"provider":"codex","id":"b","default":true}]}`,
	} {
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
		if _, err := defaultModel(providerCodex); err == nil {
			t.Fatalf("accepted invalid models file %s", content)
		}
	}
}
