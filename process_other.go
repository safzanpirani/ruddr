//go:build !unix && !windows

package main

import (
	"os"
	"os/exec"
	"syscall"
)

func processAlive(pid int) bool {
	if pid <= 0 {
		return false
	}
	process, err := os.FindProcess(pid)
	if err != nil {
		return false
	}
	defer process.Release()
	return process.Signal(syscall.Signal(0)) == nil
}

func configureChildProcess(cmd *exec.Cmd) {}

func terminateProcessTree(cmd *exec.Cmd, force bool) {
	if cmd != nil && cmd.Process != nil {
		_ = cmd.Process.Kill()
	}
}
