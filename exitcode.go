package main

import (
	"errors"
	"flag"
)

// Exit codes let scripts and agents tell outcomes apart without parsing
// error text. Every other error exits exitFailed.
const (
	exitFailed  = 1 // a run failed or was interrupted, or any other error
	exitUsage   = 2 // bad flags or a missing required argument
	exitRunning = 3 // a wait timed out, or a result was asked of a running run
	exitStale   = 4 // a controller died without persisting a terminal state
)

type exitCodeError struct {
	code int
	err  error
}

func (e exitCodeError) Error() string { return e.err.Error() }
func (e exitCodeError) Unwrap() error { return e.err }

func withExitCode(code int, err error) error {
	if err == nil {
		return nil
	}
	return exitCodeError{code: code, err: err}
}

func usageError(err error) error { return withExitCode(exitUsage, err) }

// exitCodeFor maps a command error to the process exit status. A help request
// is a success.
func exitCodeFor(err error) int {
	if err == nil || errors.Is(err, flag.ErrHelp) {
		return 0
	}
	var coded exitCodeError
	if errors.As(err, &coded) {
		return coded.code
	}
	return exitFailed
}
