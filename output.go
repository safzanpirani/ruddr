package main

import (
	"errors"
	"fmt"
	"io"
	"os"
)

// Only append to the existing private artifact. In particular, never follow a
// replacement symlink or recreate an output that disappeared during a run.
func appendPrivateOutput(path, content string) error {
	before, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if !before.Mode().IsRegular() {
		return fmt.Errorf("output artifact is not a regular file")
	}
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_APPEND, 0)
	if err != nil {
		return err
	}
	defer f.Close()
	opened, err := f.Stat()
	if err != nil {
		return err
	}
	if !os.SameFile(before, opened) {
		return fmt.Errorf("output artifact changed while opening")
	}
	if err := f.Chmod(0o600); err != nil {
		return err
	}
	if err := appendOutputChunk(f, opened.Size(), content); err != nil {
		return err
	}
	return f.Close()
}

type outputAppender interface {
	WriteString(string) (int, error)
	Truncate(int64) error
}

// Restore the previous length after a reported partial write. Like the other
// logs, an abrupt process or machine crash can still leave a partial last item.
func appendOutputChunk(f outputAppender, size int64, content string) error {
	n, err := f.WriteString(content)
	if err == nil && n != len(content) {
		err = io.ErrShortWrite
	}
	if err != nil {
		return errors.Join(err, f.Truncate(size))
	}
	return nil
}
