package main

import (
	"flag"
	"fmt"
)

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
	Note          string   `json:"note,omitempty"`
}

var codexEfforts = []string{"none", "low", "medium", "high", "xhigh", "max"}
var piEfforts = []string{"off", "minimal", "low", "medium", "high", "xhigh", "max"}

var modelCatalog = []providerModel{
	{Provider: providerCodex, ID: "gpt-6-astra", Label: "GPT-6-Astra", Efforts: codexEfforts, Default: true, Available: true},
	{Provider: providerCodex, ID: "gpt-5.6-sol", Label: "GPT-5.6-Sol", Efforts: codexEfforts, Available: true},
	{Provider: providerCodex, ID: "gpt-5.6-terra", Label: "GPT-5.6-Terra", Efforts: codexEfforts, Available: true},
	{Provider: providerCodex, ID: "gpt-5.6-luna", Label: "GPT-5.6-Luna", Efforts: codexEfforts, Available: true},
	{Provider: providerClaude, ID: "claude-fable-5-1", Label: "Claude Fable 5.1", Available: true},
	{Provider: providerClaude, ID: "claude-fable-5", Label: "Claude Fable 5", Available: true},
	{Provider: providerClaude, ID: "claude-opus-5", Label: "Claude Opus 5", Default: true, Available: true},
	{Provider: providerClaude, ID: "claude-sonnet-5", Label: "Claude Sonnet 5", Available: true},
	{Provider: providerClaude, ID: "claude-haiku-4-5-20251001", Label: "Claude Haiku 4.5", Available: true},
	{Provider: providerOpenCode, ID: "openrouter/deepseek/deepseek-v4-flash-vision-exp", Label: "DeepSeek V4 Flash Vision Exp", Default: true, Available: true},
	{Provider: providerPi, ID: "openrouter/deepseek/deepseek-v4-flash-vision-exp", Label: "DeepSeek V4 Flash Vision Exp", Efforts: piEfforts, Default: true, Available: true},
}

func defaultModel(provider string) string {
	for _, model := range modelCatalog {
		if model.Provider == provider && model.Default {
			return model.ID
		}
	}
	return ""
}

func modelsCommand(args []string) error {
	fs := flag.NewFlagSet("models", flag.ContinueOnError)
	var asJSON bool
	fs.BoolVar(&asJSON, "json", false, "print the catalog as JSON")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if asJSON {
		return printJSON(modelCatalog)
	}
	for _, model := range modelCatalog {
		if !model.Available {
			fmt.Printf("%s (%s)\n", model.Provider, model.Note)
			continue
		}
		marker := " "
		if model.Default {
			marker = "*"
		}
		fmt.Printf("%s %s %s\n", marker, model.Provider, model.ID)
	}
	return nil
}
