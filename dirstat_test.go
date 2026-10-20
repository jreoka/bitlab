package main

import (
	"os"
	"path/filepath"
	"testing"
)

// TestDirSizeTracksFiles proves the cache-budget walk counts file data on every
// platform (Linux uses st_blocks, Windows uses FILE_STANDARD_INFO, and both
// must grow when a file appears).
func TestDirSizeTracksFiles(t *testing.T) {
	dir := t.TempDir()
	empty, err := dirSize(dir)
	if err != nil {
		t.Fatalf("dirSize(empty): %v", err)
	}
	if empty != 0 {
		t.Fatalf("dirSize(empty) = %d, want 0", empty)
	}

	// Non-zero, incompressible bytes: a run of zeroes can be compressed to
	// nothing on some filesystems, which would make the comparison flaky.
	blob := make([]byte, 1<<20)
	for i := range blob {
		blob[i] = byte(i*31 + 7)
	}
	if err := os.WriteFile(filepath.Join(dir, "a.bin"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
	one, err := dirSize(dir)
	if err != nil {
		t.Fatalf("dirSize(one): %v", err)
	}
	if one <= empty {
		t.Fatalf("dirSize did not grow after adding a file: %d then %d", empty, one)
	}

	if err := os.MkdirAll(filepath.Join(dir, "sub"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "sub", "b.bin"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
	two, err := dirSize(dir)
	if err != nil {
		t.Fatalf("dirSize(two): %v", err)
	}
	if two <= one {
		t.Fatalf("dirSize ignored a nested file: %d then %d", one, two)
	}
}
