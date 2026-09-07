package main

import (
	"os"
	"os/exec"
	"testing"
)

func TestProcessAlive(t *testing.T) {
	if os.Getenv("GO_WANT_RUDDR_LIVENESS_HELPER") == "1" {
		os.Exit(0)
	}
	if !processAlive(os.Getpid()) {
		t.Fatal("current process is reported dead")
	}
	if processAlive(0) || processAlive(-1) {
		t.Fatal("invalid PID is reported alive")
	}
	child := exec.Command(os.Args[0], "-test.run=^TestProcessAlive$")
	child.Env = append(os.Environ(), "GO_WANT_RUDDR_LIVENESS_HELPER=1")
	if err := child.Run(); err != nil {
		t.Fatal(err)
	}
	if processAlive(child.Process.Pid) {
		t.Fatal("reaped child is reported alive")
	}
}
