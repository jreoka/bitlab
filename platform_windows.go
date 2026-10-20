//go:build windows

package main

import (
	"io"
	"os"
	"path/filepath"
	"strings"
	"unsafe"

	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/storage"
	"golang.org/x/sys/windows"
)

// fileStandardInfo mirrors the Win32 FILE_STANDARD_INFO structure. The field
// order and offsets must match the C declaration exactly.
type fileStandardInfo struct {
	AllocationSize int64
	EndOfFile      int64
	NumberOfLinks  uint32
	DeletePending  bool
	Directory      bool
}

// allocatedSize reports the bytes NTFS actually allocated for the file, the
// Windows equivalent of st_blocks. Torrent files are sparse while streaming,
// so the apparent size would overstate the cache footprint.
func allocatedSize(path string, info os.FileInfo) int64 {
	f, err := os.Open(path)
	if err != nil {
		return info.Size()
	}
	defer f.Close()
	var si fileStandardInfo
	err = windows.GetFileInformationByHandleEx(
		windows.Handle(f.Fd()),
		windows.FileStandardInfo,
		(*byte)(unsafe.Pointer(&si)),
		uint32(unsafe.Sizeof(si)),
	)
	if err != nil || si.AllocationSize <= 0 {
		return info.Size()
	}
	return si.AllocationSize
}

// windowsReserved are the device names Windows refuses to use as file names,
// with or without an extension.
var windowsReserved = map[string]bool{
	"CON": true, "PRN": true, "AUX": true, "NUL": true,
	"COM1": true, "COM2": true, "COM3": true, "COM4": true, "COM5": true,
	"COM6": true, "COM7": true, "COM8": true, "COM9": true,
	"LPT1": true, "LPT2": true, "LPT3": true, "LPT4": true, "LPT5": true,
	"LPT6": true, "LPT7": true, "LPT8": true, "LPT9": true,
}

// sanitizePathComponent turns a torrent or file name into something Windows
// will accept: forbidden characters become '_', trailing dots and spaces are
// dropped (Windows silently strips them, which would desynchronise names) and
// reserved device names get a prefix.
func sanitizePathComponent(s string) string {
	var b strings.Builder
	for _, r := range s {
		switch {
		case r < 0x20 || r == 0x7f:
			b.WriteByte('_')
		case strings.ContainsRune(`<>:"/\|?*`, r):
			b.WriteByte('_')
		default:
			b.WriteRune(r)
		}
	}
	out := strings.TrimRight(b.String(), " .")
	if out == "" {
		return "_"
	}
	stem := out
	if i := strings.IndexByte(stem, '.'); i >= 0 {
		stem = stem[:i]
	}
	if windowsReserved[strings.ToUpper(stem)] {
		return "_" + out
	}
	return out
}

// removeAll deletes a tree. Windows will not delete a file that still carries
// the read-only attribute, and the torrent storage marks completed files
// read-only when it promotes them, so clear that first.
func removeAll(root string) error {
	filepath.Walk(root, func(p string, info os.FileInfo, err error) error {
		if err != nil || info.IsDir() {
			return nil
		}
		os.Chmod(p, 0o666)
		return nil
	})
	return os.RemoveAll(root)
}

// windowsFilePathMaker is the client's default file layout with every path
// component sanitized, so torrents whose names contain characters Windows
// forbids ("Movie: Title", "AC/DC") can still be stored.
func windowsFilePathMaker(opts storage.FilePathMakerOpts) string {
	var parts []string
	if n := opts.Info.Name; n != "" && n != metainfo.NoName {
		parts = append(parts, sanitizePathComponent(n))
	}
	for _, p := range opts.File.BestPath() {
		parts = append(parts, sanitizePathComponent(p))
	}
	return filepath.Join(parts...)
}

// defaultStorage keeps the torrent client's file storage but routes paths
// through the Windows-safe maker. The piece completion database stays in the
// data dir, matching the client's own choice on other platforms.
func defaultStorage(dir string) (storage.ClientImpl, io.Closer) {
	completion, err := storage.NewDefaultPieceCompletionForDir(dir)
	if err != nil {
		completion = storage.NewMapPieceCompletion()
	}
	impl := storage.NewFileOpts(storage.NewFileClientOpts{
		ClientBaseDir:   dir,
		FilePathMaker:   windowsFilePathMaker,
		PieceCompletion: completion,
	})
	return impl, impl
}
