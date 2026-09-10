package main

import (
	"bufio"
	"encoding/json"
	"os"
	"strings"
	"testing"
)

// runVoidResultAppServer answers every request with a JSON-RPC success that
// omits `result`, the shape a server uses for a void operation.
func runVoidResultAppServer() {
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 64*1024), maxRPCLineBytes)
	encoder := json.NewEncoder(os.Stdout)
	for scanner.Scan() {
		var request struct {
			ID     string `json:"id"`
			Method string `json:"method"`
		}
		if err := json.Unmarshal(scanner.Bytes(), &request); err != nil || request.ID == "" {
			continue
		}
		_ = encoder.Encode(map[string]any{"id": request.ID})
	}
}

// A response without a `result` member reports success, so thread commands
// such as archive must not fail while decoding it.
func TestThreadCommandAcceptsVoidResult(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_VOID_RESULT_SERVER") == "1" {
		runVoidResultAppServer()
		os.Exit(0)
	}
	t.Setenv("GO_WANT_RUDDR_VOID_RESULT_SERVER", "1")
	child := []string{"--", os.Args[0], "-test.run=TestThreadCommandAcceptsVoidResult"}

	output, err := captureStdout(func() error {
		return threadCommand(append([]string{"archive", "source-thread"}, child...))
	})
	if err != nil {
		t.Fatalf("thread archive with a void result failed: %v", err)
	}
	if strings.TrimSpace(string(output)) != "null" {
		t.Fatalf("thread archive printed %q; want null", output)
	}
}
