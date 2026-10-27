//go:build !unix && !windows

package main

import (
	"errors"
	"io"
	"os"

	"github.com/anacrolix/torrent/storage"
)

// allocatedSize falls back to the apparent size where the platform has no
// block count to report.
func allocatedSize(_ string, info os.FileInfo) int64 { return info.Size() }

// freeDiskSpace is unsupported here; the cache reserve check is skipped.
func freeDiskSpace(string) (int64, error) { return 0, errors.New("free disk space not supported") }

// removeAll deletes a tree.
func removeAll(path string) error { return os.RemoveAll(path) }

// defaultStorage leaves the storage choice to the torrent client.
func defaultStorage(_ string) (storage.ClientImpl, io.Closer) { return nil, nil }
