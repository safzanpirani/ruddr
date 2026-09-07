//go:build windows

package main

import (
	"os/exec"
	"strconv"
	"syscall"
)

func configureChildProcess(cmd *exec.Cmd) {}

func processAlive(pid int) bool {
	if pid <= 0 || uint64(pid) > 1<<32-1 {
		return false
	}
	handle, err := syscall.OpenProcess(syscall.SYNCHRONIZE, false, uint32(pid))
	if err != nil {
		return err == syscall.ERROR_ACCESS_DENIED
	}
	defer syscall.CloseHandle(handle)
	result, err := syscall.WaitForSingleObject(handle, 0)
	return err == nil && result == uint32(syscall.WAIT_TIMEOUT)
}

func terminateProcessTree(cmd *exec.Cmd, _ bool) {
	if cmd == nil || cmd.Process == nil {
		return
	}
	if err := exec.Command("taskkill.exe", windowsTaskkillArgs(cmd.Process.Pid)...).Run(); err != nil {
		_ = cmd.Process.Kill()
	}
}

func windowsTaskkillArgs(pid int) []string {
	return []string{"/PID", strconv.Itoa(pid), "/T", "/F"}
}
