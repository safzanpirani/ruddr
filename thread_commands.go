package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

type appServerSession struct {
	cmd     *exec.Cmd
	ctx     context.Context
	stdin   io.WriteCloser
	scanner *bufio.Scanner
	waitCh  chan error
	nextID  int
}

func threadCommand(args []string) error {
	if len(args) == 0 {
		return errors.New("thread action is required: list, search, read, turns, fork, name, archive, or unarchive")
	}
	action := args[0]
	flagArgs := args[1:]
	childCommand := []string{"codex", "app-server", "--listen", "stdio://"}
	if marker := indexOf(flagArgs, "--"); marker >= 0 {
		childCommand = flagArgs[marker+1:]
		flagArgs = flagArgs[:marker]
	}
	if len(childCommand) == 0 {
		return errors.New("app-server command after -- is empty")
	}
	fs := flag.NewFlagSet("thread "+action, flag.ContinueOnError)
	cwd, _ := os.Getwd()
	var cursor, cwdFilter, beforeTurn, throughTurn string
	var limit int
	var includeTurns, archived bool
	fs.StringVar(&cwd, "cwd", cwd, "working directory for app-server")
	fs.StringVar(&cwdFilter, "cwd-filter", "", "filter threads by exact cwd")
	fs.StringVar(&cursor, "cursor", "", "opaque pagination cursor")
	fs.IntVar(&limit, "limit", 0, "page size; zero uses the server default")
	fs.BoolVar(&archived, "archived", false, "operate on archived thread history")
	fs.BoolVar(&includeTurns, "include-turns", false, "include turns when reading a thread")
	fs.StringVar(&beforeTurn, "before-turn", "", "fork before this turn")
	fs.StringVar(&throughTurn, "through-turn", "", "fork through this turn")
	if err := fs.Parse(flagArgs); err != nil {
		return err
	}
	positionals := fs.Args()

	params := map[string]any{}
	if cursor != "" {
		params["cursor"] = cursor
	}
	if limit > 0 {
		params["limit"] = limit
	}
	var method string
	switch action {
	case "list":
		method = "thread/list"
		if cwdFilter != "" {
			params["cwd"] = cwdFilter
		}
		if archived {
			params["archived"] = true
		}
	case "search":
		method = "thread/search"
		if len(positionals) == 0 {
			return errors.New("thread search requires a search term")
		}
		params["searchTerm"] = strings.Join(positionals, " ")
		if archived {
			params["archived"] = true
		}
	case "read":
		method = "thread/read"
		id, err := requireThreadID(action, positionals)
		if err != nil {
			return err
		}
		params = map[string]any{"threadId": id, "includeTurns": includeTurns}
	case "turns":
		method = "thread/turns/list"
		id, err := requireThreadID(action, positionals)
		if err != nil {
			return err
		}
		params["threadId"] = id
	case "fork":
		method = "thread/fork"
		id, err := requireThreadID(action, positionals)
		if err != nil {
			return err
		}
		if beforeTurn != "" && throughTurn != "" {
			return errors.New("--before-turn and --through-turn are mutually exclusive")
		}
		params = map[string]any{"threadId": id, "excludeTurns": true}
		if beforeTurn != "" {
			params["beforeTurnId"] = beforeTurn
		}
		if throughTurn != "" {
			params["lastTurnId"] = throughTurn
		}
	case "name":
		method = "thread/name/set"
		if len(positionals) < 2 {
			return errors.New("thread name requires THREAD_ID and NAME")
		}
		params = map[string]any{"threadId": positionals[0], "name": strings.Join(positionals[1:], " ")}
	case "archive", "unarchive":
		method = "thread/" + action
		id, err := requireThreadID(action, positionals)
		if err != nil {
			return err
		}
		params = map[string]any{"threadId": id}
	default:
		return fmt.Errorf("unknown thread action %q", action)
	}

	raw, err := invokeAppServer(cwd, childCommand, method, params)
	if err != nil {
		return err
	}
	return printRawJSON(raw)
}

func requireThreadID(action string, args []string) (string, error) {
	if len(args) != 1 || strings.TrimSpace(args[0]) == "" {
		return "", fmt.Errorf("thread %s requires exactly one THREAD_ID", action)
	}
	return args[0], nil
}

func indexOf(values []string, target string) int {
	for i, value := range values {
		if value == target {
			return i
		}
	}
	return -1
}

func invokeAppServer(cwd string, childCommand []string, method string, params any) (json.RawMessage, error) {
	absCWD, err := filepath.Abs(cwd)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	session, err := startAppServerSession(ctx, absCWD, childCommand)
	if err != nil {
		return nil, err
	}
	defer session.close()
	if err := session.initialize(); err != nil {
		return nil, err
	}
	var result json.RawMessage
	if err := session.call(method, params, &result); err != nil {
		return nil, err
	}
	return result, nil
}

func startAppServerSession(ctx context.Context, cwd string, childCommand []string) (*appServerSession, error) {
	cmd := exec.CommandContext(ctx, childCommand[0], childCommand[1:]...)
	configureChildProcess(cmd)
	cmd.Cancel = func() error {
		terminateProcessTree(cmd, false)
		return nil
	}
	cmd.WaitDelay = 3 * time.Second
	cmd.Dir = cwd
	cmd.Stderr = os.Stderr
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	stdin, err := cmd.StdinPipe()
	if err != nil {
		_ = stdout.Close()
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		_ = stdout.Close()
		_ = stdin.Close()
		return nil, err
	}
	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 64*1024), maxRPCLineBytes)
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()
	return &appServerSession{cmd: cmd, ctx: ctx, stdin: stdin, scanner: scanner, waitCh: waitCh}, nil
}

func (s *appServerSession) initialize() error {
	var result map[string]any
	if err := s.call("initialize", map[string]any{
		"clientInfo":   map[string]any{"name": "ruddr", "title": "Ruddr", "version": version},
		"capabilities": map[string]any{"experimentalApi": true},
	}, &result); err != nil {
		return fmt.Errorf("initialize app-server: %w", err)
	}
	return s.write(map[string]any{"method": "initialized", "params": map[string]any{}})
}

func (s *appServerSession) call(method string, params any, target any) error {
	s.nextID++
	id := fmt.Sprintf("ruddr-query-%d", s.nextID)
	if err := s.write(map[string]any{"id": id, "method": method, "params": params}); err != nil {
		return err
	}
	for s.scanner.Scan() {
		var message rpcEnvelope
		if err := json.Unmarshal(s.scanner.Bytes(), &message); err != nil {
			return fmt.Errorf("parse app-server response: %w", err)
		}
		if message.Method != "" {
			if len(message.ID) > 0 {
				var requestID any
				if json.Unmarshal(message.ID, &requestID) == nil {
					_ = s.write(map[string]any{"id": requestID, "error": map[string]any{"code": -32601, "message": "Ruddr thread command cannot answer interactive requests"}})
				}
			}
			continue
		}
		responseID, ok := rpcID(message.ID)
		if !ok || responseID != id {
			continue
		}
		if message.Error != nil {
			return fmt.Errorf("%s (%d)", message.Error.Message, message.Error.Code)
		}
		// A void result arrives as an omitted or null `result`; that is a
		// successful response, not a decoding failure.
		if target != nil && len(message.Result) > 0 {
			return json.Unmarshal(message.Result, target)
		}
		return nil
	}
	if err := s.scanner.Err(); err != nil {
		return err
	}
	return errors.New("app-server closed before responding")
}

func (s *appServerSession) write(message any) error {
	raw, err := json.Marshal(message)
	if err != nil {
		return err
	}
	return writeAllWithTimeout(s.stdin, append(raw, '\n'), defaultRPCWriteTimeout)
}

func (s *appServerSession) close() {
	_ = s.stdin.Close()
	select {
	case <-s.waitCh:
		if s.ctx.Err() != nil {
			terminateProcessTree(s.cmd, true)
		}
		return
	case <-time.After(3 * time.Second):
	}
	if s.ctx.Err() == nil {
		terminateProcessTree(s.cmd, false)
		select {
		case <-s.waitCh:
			return
		case <-time.After(3 * time.Second):
		}
	}
	terminateProcessTree(s.cmd, true)
	select {
	case <-s.waitCh:
	case <-time.After(3 * time.Second):
	}
}

func printRawJSON(raw json.RawMessage) error {
	if len(raw) == 0 {
		_, err := fmt.Fprintln(os.Stdout, "null")
		return err
	}
	var formatted bytes.Buffer
	if err := json.Indent(&formatted, raw, "", "  "); err != nil {
		return err
	}
	formatted.WriteByte('\n')
	_, err := formatted.WriteTo(os.Stdout)
	return err
}
