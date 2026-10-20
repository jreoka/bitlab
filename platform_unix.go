//go:build unix

package main

import (
	"io"
	"os"
	"syscall"

	"github.com/anacrolix/torrent/storage"
)

// allocatedSize reports the bytes actually allocated on disk for the file at
// path. Torrent files are sparse while streaming, so the block count is the
// honest number; the apparent size would overstate the cache footprint.
func allocatedSize(_ string, info os.FileInfo) int64 {
	if st, ok := info.Sys().(*syscall.Stat_t); ok {
		return st.Blocks * 512
	}
	return info.Size()
}

// sanitizePathComponent is a no-op on unix: every byte except '/' and NUL is
// legal in a file name, so torrent names are used verbatim and the on-disk
// layout matches what every previous release produced.
func sanitizePathComponent(s string) string { return s }

// removeAll deletes a tree. Unix unlinks read-only files as long as the parent
// directory is writable, so there is nothing to prepare first.
func removeAll(path string) error { return os.RemoveAll(path) }

// defaultStorage leaves the storage choice to the torrent client, which uses
// storage.NewFile(dataDir) — the layout every previous release used.
func defaultStorage(_ string) (storage.ClientImpl, io.Closer) { return nil, nil }
