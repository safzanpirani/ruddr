package main

import (
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

// Measure a complete session, including filesystem writes, rather than a
// single message whose cost depends on an unreported transcript size.
func BenchmarkAgentOutputSession(b *testing.B) {
	for _, messages := range []int{128, 1024} {
		b.Run(fmt.Sprintf("messages=%d", messages), func(b *testing.B) {
			path := filepath.Join(b.TempDir(), "output.md")
			message := strings.Repeat("x", 1024)
			b.SetBytes(int64(messages * len(message)))
			b.ReportAllocs()
			for i := 0; i < b.N; i++ {
				if err := writePrivateFile(path, nil); err != nil {
					b.Fatal(err)
				}
				r := &controller{store: &stateStore{state: runState{OutputPath: path}}}
				for n := 0; n < messages; n++ {
					if err := r.recordAgentMessage(message); err != nil {
						b.Fatal(err)
					}
				}
			}
		})
	}
}
