package main

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"text/tabwriter"
	"time"
)

// maxRootDepth bounds how far below a --root directory run discovery looks.
// A swarm layout such as ROOT/<agent>/run sits well inside it.
const maxRootDepth = 4

// runRef names one run in a group: its state directory and the label shown
// in tables (the path relative to its --root, or the --state-dir as given).
type runRef struct {
	Name     string
	StateDir string
}

// groupSelection holds the repeatable --state-dir and --root flags that let
// status, wait, stop, and interrupt act on several runs at once.
type groupSelection struct {
	stateDirs stringList
	roots     stringList
}

func (g *groupSelection) register(fs *flag.FlagSet) {
	fs.Var(&g.stateDirs, "state-dir", "Ruddr run state directory (repeatable)")
	fs.Var(&g.roots, "root", "act on every run below DIR (repeatable)")
}

// single reports whether the flags name exactly one run the old way, so the
// command keeps its original single-run output and exit behavior.
func (g *groupSelection) single() (string, bool) {
	if len(g.roots) == 0 && len(g.stateDirs) <= 1 {
		if len(g.stateDirs) == 0 {
			return "", true
		}
		return g.stateDirs[0], true
	}
	return "", false
}

// resolve returns every selected run, de-duplicated by absolute path, with
// explicit state directories first and each root's runs sorted by name.
func (g *groupSelection) resolve() ([]runRef, error) {
	var refs []runRef
	seen := map[string]bool{}
	add := func(ref runRef) error {
		absolute, err := filepath.Abs(ref.StateDir)
		if err != nil {
			return err
		}
		if !seen[absolute] {
			seen[absolute] = true
			refs = append(refs, ref)
		}
		return nil
	}
	for _, stateDir := range g.stateDirs {
		if err := add(runRef{Name: stateDir, StateDir: stateDir}); err != nil {
			return nil, err
		}
	}
	for _, root := range g.roots {
		found, err := discoverRuns(root)
		if err != nil {
			return nil, err
		}
		if len(found) == 0 {
			return nil, fmt.Errorf("no runs found below %s", root)
		}
		for _, ref := range found {
			if err := add(ref); err != nil {
				return nil, err
			}
		}
	}
	return refs, nil
}

// discoverRuns finds state directories below root. It does not follow
// symlinks or descend into a run's own state directory.
func discoverRuns(root string) ([]runRef, error) {
	info, err := os.Stat(root)
	if err != nil {
		return nil, err
	}
	if !info.IsDir() {
		return nil, fmt.Errorf("--root %s is not a directory", root)
	}
	var refs []runRef
	err = filepath.WalkDir(root, func(path string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			if path == root {
				return walkErr
			}
			return nil
		}
		if !entry.IsDir() {
			return nil
		}
		relative, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		if _, err := os.Stat(filepath.Join(path, stateFileName)); err == nil {
			name := filepath.ToSlash(relative)
			if name == "." {
				name = filepath.Base(path)
			}
			refs = append(refs, runRef{Name: name, StateDir: path})
			return filepath.SkipDir
		}
		if relative != "." && strings.Count(filepath.ToSlash(relative), "/")+1 >= maxRootDepth {
			return filepath.SkipDir
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	sort.Slice(refs, func(i, j int) bool { return refs[i].Name < refs[j].Name })
	return refs, nil
}

// readGroupState reads one run for a group view. A run whose state cannot be
// read shows as "unreadable" instead of failing the whole command.
func readGroupState(ref runRef, alive func(int) bool) runState {
	state, err := readState(ref.StateDir)
	if err != nil {
		return runState{StateDir: ref.StateDir, Status: "unreadable", Error: err.Error()}
	}
	if !terminalStatus(state.Status) && !alive(state.PID) {
		// The controller persists its terminal state before exiting; re-read
		// before calling a run stale.
		if final, finalErr := readState(ref.StateDir); finalErr == nil && terminalStatus(final.Status) {
			return final
		}
		state.Status = "stale"
		state.Error = fmt.Sprintf("Ruddr pid %d is not running; persisted state is stale", state.PID)
	}
	return state
}

// settledStatus reports whether a group wait should stop watching a run.
func settledStatus(status string) bool {
	return terminalStatus(status) || status == "stale" || status == "unreadable"
}

func printGroupTable(w io.Writer, refs []runRef, states []runState, now time.Time) {
	table := tabwriter.NewWriter(w, 0, 0, 2, ' ', 0)
	fmt.Fprintln(table, "NAME\tSTATUS\tPROVIDER\tMODEL\tTURNS\tTOKENS\tELAPSED\tERROR")
	for i, ref := range refs {
		state := states[i]
		turns := "-"
		if state.Turns > 0 {
			turns = fmt.Sprint(state.Turns)
		}
		tokens := "-"
		if state.TokenUsage != nil && state.TokenUsage.TotalTokens > 0 {
			tokens = formatTokenCount(state.TokenUsage.TotalTokens)
		}
		fmt.Fprintf(table, "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n",
			ref.Name, state.Status, dash(state.Provider), dash(state.Model), turns, tokens,
			elapsed(state, now), oneLine(state.Error, 100))
	}
	table.Flush()
}

func dash(value string) string {
	if value == "" {
		return "-"
	}
	return value
}

func formatTokenCount(count int64) string {
	switch {
	case count >= 1_000_000:
		return fmt.Sprintf("%.1fM", float64(count)/1_000_000)
	case count >= 1_000:
		return fmt.Sprintf("%.1fK", float64(count)/1_000)
	default:
		return fmt.Sprint(count)
	}
}

// elapsed is the run's wall time: until now while it runs, until completion
// (or its last update) once it has settled.
func elapsed(state runState, now time.Time) string {
	if state.StartedAt.IsZero() {
		return "-"
	}
	end := now
	if settledStatus(state.Status) {
		end = state.CompletedAt
		if end.IsZero() {
			end = state.UpdatedAt
		}
	}
	if end.Before(state.StartedAt) {
		return "-"
	}
	return end.Sub(state.StartedAt).Round(time.Second).String()
}

func groupStatus(refs []runRef, asJSON bool) error {
	states := make([]runState, len(refs))
	for i, ref := range refs {
		states[i] = readGroupState(ref, processAlive)
	}
	if asJSON {
		return printJSON(states)
	}
	printGroupTable(os.Stdout, refs, states, time.Now())
	return nil
}

// waitForRuns polls every run until all of them settle, or the first one
// does when anyRun is set, or the deadline passes. It prints the final table and
// fails unless every settled run completed.
func waitForRuns(w io.Writer, refs []runRef, deadline time.Time, anyRun bool, alive func(int) bool, tick time.Duration) error {
	ticker := time.NewTicker(tick)
	defer ticker.Stop()
	states := make([]runState, len(refs))
	settled := make([]bool, len(refs))
	for {
		settledCount := 0
		for i, ref := range refs {
			if !settled[i] {
				states[i] = readGroupState(ref, alive)
				settled[i] = settledStatus(states[i].Status)
			}
			if settled[i] {
				settledCount++
			}
		}
		timedOut := !deadline.IsZero() && time.Now().After(deadline)
		if settledCount == len(refs) || (anyRun && settledCount > 0) || timedOut {
			printGroupTable(w, refs, states, time.Now())
			failed := 0
			for i := range refs {
				if settled[i] && states[i].Status != "completed" {
					failed++
				}
			}
			switch {
			case settledCount == 0 || (!anyRun && settledCount < len(refs)):
				return fmt.Errorf("wait timed out: %d of %d runs still running", len(refs)-settledCount, len(refs))
			case failed > 0:
				return fmt.Errorf("%d of %d runs did not complete", failed, len(refs))
			}
			return nil
		}
		<-ticker.C
	}
}

// broadcastControl sends one control command to every run whose status
// allows it and reports the others as skipped. It fails if any send failed.
func broadcastControl(w io.Writer, refs []runRef, command, wantStatus string, timeout time.Duration) error {
	acted, failed := 0, 0
	for _, ref := range refs {
		state := readGroupState(ref, processAlive)
		if state.Status != wantStatus {
			fmt.Fprintf(w, "%s: skipped (status=%s)\n", ref.Name, state.Status)
			continue
		}
		request := controlRequest{Command: command}
		if command == "interrupt" {
			request.ExpectedTurnID = state.TurnID
		}
		response, err := sendControl(ref.StateDir, request, timeout)
		if err == nil && !response.OK {
			err = errors.New(response.Error)
		}
		if err != nil {
			fmt.Fprintf(w, "%s: failed: %v\n", ref.Name, err)
			failed++
			continue
		}
		fmt.Fprintf(w, "%s: %s requested\n", ref.Name, command)
		acted++
	}
	if failed > 0 {
		return fmt.Errorf("%s failed for %d of %d runs", command, failed, len(refs))
	}
	if acted == 0 {
		verb := command
		if command == "shutdown" {
			verb = "stop"
		}
		fmt.Fprintf(w, "no %s runs to %s\n", wantStatus, verb)
	}
	return nil
}
