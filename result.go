package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
)

// runResult is one run's entry in `result --json`.
type runResult struct {
	Name           string `json:"name"`
	StateDir       string `json:"stateDir"`
	Status         string `json:"status"`
	LastTurnStatus string `json:"lastTurnStatus,omitempty"`
	Error          string `json:"error,omitempty"`
	Message        string `json:"message,omitempty"`
}

func resultCommand(args []string) error {
	fs := flag.NewFlagSet("result", flag.ContinueOnError)
	var group groupSelection
	var asJSON bool
	group.register(fs)
	fs.BoolVar(&asJSON, "json", false, "print a JSON array of results")
	if err := fs.Parse(args); err != nil {
		return err
	}
	stateDir, single := group.single()
	var refs []runRef
	if single {
		if stateDir == "" {
			return errors.New("--state-dir is required")
		}
		refs = []runRef{{Name: stateDir, StateDir: stateDir}}
	} else {
		var err error
		if refs, err = group.resolve(); err != nil {
			return err
		}
	}
	results := make([]runResult, len(refs))
	failed := 0
	for i, ref := range refs {
		results[i] = collectResult(ref)
		if results[i].Error != "" {
			failed++
		}
	}
	if asJSON {
		if err := printJSON(results); err != nil {
			return err
		}
	} else if single {
		if results[0].Error == "" {
			fmt.Println(results[0].Message)
		}
	} else {
		printResults(os.Stdout, results)
	}
	switch {
	case single && results[0].Error != "":
		return errors.New(results[0].Error)
	case failed > 0:
		return fmt.Errorf("%d of %d runs did not complete", failed, len(results))
	}
	return nil
}

// collectResult reads a run's final answer: the last agent message of its
// latest turn. A run that did not finish its work reports why instead.
func collectResult(ref runRef) runResult {
	state := readGroupState(ref, processAlive)
	result := runResult{Name: ref.Name, StateDir: ref.StateDir, Status: state.Status, LastTurnStatus: state.LastTurn}
	switch {
	case state.Status == "active" || state.Status == "starting" || state.Status == "stopping":
		result.Error = "run is still " + state.Status
		return result
	case !runSucceeded(state):
		result.Error = rowError(state)
		if result.Error == "" {
			result.Error = "run ended with status " + state.Status
		}
		return result
	}
	message, err := lastAgentMessage(state.EventsPath)
	switch {
	case err != nil:
		result.Error = err.Error()
	case message == "":
		result.Error = "the latest turn produced no agent message"
	default:
		result.Message = message
	}
	return result
}

// lastAgentMessage scans events.jsonl for the last completed agentMessage
// after the latest turn/started. Earlier turns' answers are not reported.
func lastAgentMessage(eventsPath string) (string, error) {
	if eventsPath == "" {
		return "", errors.New("state has no events path")
	}
	file, err := os.Open(eventsPath)
	if err != nil {
		return "", err
	}
	defer file.Close()
	reader := bufio.NewReader(file)
	message := ""
	for {
		line, readErr := reader.ReadBytes('\n')
		if bytes.Contains(line, []byte(`"turn/started"`)) || bytes.Contains(line, []byte(`"agentMessage"`)) {
			var event struct {
				Method string `json:"method"`
				Params struct {
					Item struct {
						Type string `json:"type"`
						Text string `json:"text"`
					} `json:"item"`
				} `json:"params"`
			}
			if json.Unmarshal(line, &event) == nil {
				switch {
				case event.Method == "turn/started":
					message = ""
				case event.Method == "item/completed" && event.Params.Item.Type == "agentMessage" && event.Params.Item.Text != "":
					message = event.Params.Item.Text
				}
			}
		}
		if readErr == io.EOF {
			return message, nil
		}
		if readErr != nil {
			return "", readErr
		}
	}
}

func printResults(w io.Writer, results []runResult) {
	for i, result := range results {
		if i > 0 {
			fmt.Fprintln(w)
		}
		fmt.Fprintf(w, "== %s: %s ==\n", result.Name, result.Status)
		if result.Error != "" {
			fmt.Fprintf(w, "error: %s\n", result.Error)
			continue
		}
		fmt.Fprintln(w, result.Message)
	}
}
