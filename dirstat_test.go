package main

import (
	"os"
	"path/filepath"
	"testing"
)

func cacheTotal(t *testing.T, dir string) int64 {
	t.Helper()
	entries, err := scanCache(dir)
	if err != nil {
		t.Fatalf("scanCache: %v", err)
	}
	var n int64
	for _, e := range entries {
		n += e.Bytes
	}
	return n
}

// TestScanCacheTracksFiles proves the cache-accounting walk counts file data
// on every platform (Linux uses st_blocks, Windows uses FILE_STANDARD_INFO,
// and both must grow when a file appears).
func TestScanCacheTracksFiles(t *testing.T) {
	dir := t.TempDir()
	empty := cacheTotal(t, dir)
	if empty != 0 {
		t.Fatalf("empty cache = %d, want 0", empty)
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
	one := cacheTotal(t, dir)
	if one <= empty {
		t.Fatalf("cache did not grow after adding a file: %d then %d", empty, one)
	}

	if err := os.MkdirAll(filepath.Join(dir, "sub"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "sub", "b.bin"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
	two := cacheTotal(t, dir)
	if two <= one {
		t.Fatalf("cache ignored a nested file: %d then %d", one, two)
	}
}
