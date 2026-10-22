//go:build windows

package main

import (
	"path/filepath"
	"testing"
	"unsafe"

	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/storage"
)

func TestSanitizePathComponent(t *testing.T) {
	for _, tc := range []struct{ in, want string }{
		{"Some.Show.S01E01.1080p", "Some.Show.S01E01.1080p"},
		{"Movie: The Sequel", "Movie_ The Sequel"},
		{"AC/DC Live", "AC_DC Live"},
		{`What? "Really" <Yes> |No|`, "What_ _Really_ _Yes_ _No_"},
		{"trailing dots...", "trailing dots"},
		{"trailing space ", "trailing space"},
		{"CON", "_CON"},
		{"nul.txt", "_nul.txt"},
		{"COM1", "_COM1"},
		{"lpt9.mkv", "_lpt9.mkv"},
		{"COM10", "COM10"},
		{"...", "_"},
		{"", "_"},
		{"tab\there", "tab_here"},
	} {
		if got := sanitizePathComponent(tc.in); got != tc.want {
			t.Errorf("sanitizePathComponent(%q) = %q, want %q", tc.in, got, tc.want)
		}
	}
}

// TestFileStandardInfoLayout pins the struct to the Win32 declaration; a field
// inserted or reordered here would make the syscall fill the wrong offsets.
func TestFileStandardInfoLayout(t *testing.T) {
	var si fileStandardInfo
	if got := unsafe.Sizeof(si); got != 24 {
		t.Fatalf("fileStandardInfo is %d bytes, FILE_STANDARD_INFO is 24", got)
	}
	if got := unsafe.Offsetof(si.EndOfFile); got != 8 {
		t.Fatalf("EndOfFile offset = %d, want 8", got)
	}
	if got := unsafe.Offsetof(si.NumberOfLinks); got != 16 {
		t.Fatalf("NumberOfLinks offset = %d, want 16", got)
	}
	if got := unsafe.Offsetof(si.DeletePending); got != 20 {
		t.Fatalf("DeletePending offset = %d, want 20", got)
	}
	if got := unsafe.Offsetof(si.Directory); got != 21 {
		t.Fatalf("Directory offset = %d, want 21", got)
	}
}

// TestWindowsFilePathMakerSanitizes covers the per-file sanitizer; the torrent
// directory itself is the infohash (TorrentDirMaker), so it is not name-based.
func TestWindowsFilePathMakerSanitizes(t *testing.T) {
	got := windowsFilePathMaker(storage.FilePathMakerOpts{
		Info: &metainfo.Info{Name: "Movie: Part Two"},
		File: &metainfo.FileInfo{Path: []string{"Movies: extras", "film.mkv"}},
	})
	want := filepath.Join("Movie_ Part Two", "Movies_ extras", "film.mkv")
	if got != want {
		t.Fatalf("windowsFilePathMaker = %q, want %q", got, want)
	}
}
