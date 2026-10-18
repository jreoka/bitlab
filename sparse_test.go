package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"io"
	"os"
	"testing"
)

// memReader is a byteSource over a buffer, so the probe path can be exercised
// against a real muxed file without a swarm.
type memReader struct {
	b   []byte
	off int64
	cut int64 // stop serving past this offset, to imitate a slow/dead swarm
	ni  int   // reads performed
}

func newMemReader(b []byte) *memReader { return &memReader{b: b, cut: int64(len(b))} }

func (m *memReader) Read(p []byte) (int, error) {
	m.ni++
	if m.off >= m.cut || int(m.off) >= len(m.b) {
		return 0, io.EOF
	}
	end := m.off + int64(len(p))
	if end > m.cut {
		end = m.cut
	}
	if int(end) > len(m.b) {
		end = int64(len(m.b))
	}
	n := copy(p, m.b[m.off:end])
	m.off += int64(n)
	return n, nil
}

func (m *memReader) Seek(off int64, whence int) (int64, error) {
	switch whence {
	case io.SeekStart:
		m.off = off
	case io.SeekCurrent:
		m.off += off
	case io.SeekEnd:
		m.off = int64(len(m.b)) + off
	default:
		return 0, errors.New("bad whence")
	}
	if m.off < 0 {
		return 0, errors.New("negative seek")
	}
	return m.off, nil
}

func (m *memReader) Close() error               { return nil }
func (m *memReader) SetReadahead(int64)         {}
func (m *memReader) SetResponsive()             {}
func (m *memReader) SetContext(context.Context) {}

// A moov at the end of a file must be found by offset and read sparsely: only
// each trak's opening bytes, not the sample tables behind them.
func TestSparseMP4ReadsOnlyTrakPrefixes(t *testing.T) {
	moov := mp4Box("moov", bytes.Join([][]byte{
		mp4Track("vide", 1920, 1080),
		mp4Track("soun", 0, 0),
		mp4Track("soun", 0, 0),
		mp4Track("subt", 0, 0),
	}, nil))
	// 4 MiB of payload before the moov, so it cannot be found in a head window.
	file := append(bytes.Repeat([]byte{0x11}, 4<<20), moov...)
	r := newMemReader(file)

	start, size, hdrLen, _, ok := locateMoov(r, int64(len(file)))
	if !ok {
		t.Fatal("locateMoov did not find a tail moov")
	}
	if start+int64(size) != int64(len(file)) {
		t.Fatalf("moov location %d+%d does not end the file (%d)", start, size, len(file))
	}
	ti, read, complete := sparseMP4(r, start, size, hdrLen)
	if !complete {
		t.Fatal("sparse walk reported incomplete for a whole moov")
	}
	if ti.Video != 1 || ti.Audio != 2 || ti.Subs != 1 {
		t.Fatalf("sparse counts = video %d audio %d subs %d", ti.Video, ti.Audio, ti.Subs)
	}
	if ti.Width != 1920 || ti.Height != 1080 {
		t.Fatalf("sparse dims = %dx%d", ti.Width, ti.Height)
	}
	// The whole point: a few hundred bytes, nothing like the 4 MiB of head that
	// a window scan would have pulled.
	if read > 4096 {
		t.Fatalf("sparse read %d bytes; expected a few hundred", read)
	}
	t.Logf("sparse read %d bytes for a %d-byte moov", read, size)
}

// The case that matters in the wild: a film's video trak carries megabytes of
// sample tables, and the audio traks sit behind them. Reading trak headers only
// must still find them, for a few hundred bytes instead of the whole moov.
func TestSparseMP4SkipsHugeSampleTables(t *testing.T) {
	bigVideo := mp4Box("trak", bytes.Join([][]byte{
		mp4Box("tkhd", func() []byte {
			b := make([]byte, 84)
			binary.BigEndian.PutUint32(b[76:], 1920<<16)
			binary.BigEndian.PutUint32(b[80:], 1080<<16)
			return b
		}()),
		mp4Box("mdia", bytes.Join([][]byte{
			mp4Box("hdlr", append(make([]byte, 8), []byte("vide0123456789ab")...)),
			mp4Box("minf", mp4Box("stbl", make([]byte, 8<<20))), // 8 MiB of tables
		}, nil)),
	}, nil))
	moov := mp4Box("moov", bytes.Join([][]byte{
		bigVideo,
		mp4Track("soun", 0, 0),
		mp4Track("subt", 0, 0),
	}, nil))
	if len(moov) < 8<<20 {
		t.Fatalf("fixture moov is only %d bytes", len(moov))
	}
	file := append(bytes.Repeat([]byte{0x22}, 1<<20), moov...)
	r := newMemReader(file)

	start, size, hdrLen, _, ok := locateMoov(r, int64(len(file)))
	if !ok {
		t.Fatal("locateMoov failed")
	}
	ti, read, complete := sparseMP4(r, start, size, hdrLen)
	if !complete {
		t.Fatal("sparse walk should be complete")
	}
	if ti.Video != 1 || ti.Audio != 1 || ti.Subs != 1 {
		t.Fatalf("counts = video %d audio %d subs %d", ti.Video, ti.Audio, ti.Subs)
	}
	if ti.Width != 1920 || ti.Height != 1080 {
		t.Fatalf("dims = %dx%d", ti.Width, ti.Height)
	}
	if read > 4096 {
		t.Fatalf("sparse read %d bytes for a %d-byte moov", read, size)
	}
	t.Logf("read %d bytes of a %d-byte moov", read, size)
}

// When the swarm dies mid-walk the box headers run out: report incomplete
// rather than claiming a release has no audio.
func TestSparseMP4ReportsIncomplete(t *testing.T) {
	moov := mp4Box("moov", bytes.Join([][]byte{
		mp4Track("vide", 1920, 800),
		mp4Track("soun", 0, 0),
	}, nil))
	file := append(bytes.Repeat([]byte{0x11}, 4096), moov...)
	r := newMemReader(file)
	r.cut = int64(len(file) - 20) // stop just inside the moov

	start, size, hdrLen, _, ok := locateMoov(r, int64(len(file)))
	if !ok {
		t.Skip("fixture too small to locate")
	}
	_, _, complete := sparseMP4(r, start, size, hdrLen)
	if complete {
		t.Fatal("sparse walk claimed completeness with a truncated moov")
	}
}

// The same path against a file a real muxer wrote.
func TestSparseMP4RealFile(t *testing.T) {
	path := os.Getenv("BITLAB_PROBE_MP4")
	if path == "" {
		t.Skip("set BITLAB_PROBE_MP4 to a real media file")
	}
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	r := newMemReader(b)
	start, size, hdrLen, _, ok := locateMoov(r, int64(len(b)))
	if !ok {
		t.Fatalf("%s: no moov", path)
	}
	ti, read, complete := sparseMP4(r, start, size, hdrLen)
	t.Logf("%s: video=%d audio=%d subs=%d %dx%d read=%d/%d complete=%v",
		path, ti.Video, ti.Audio, ti.Subs, ti.Width, ti.Height, read, size, complete)
	if ti.Video != 1 || ti.Audio < 1 {
		t.Fatalf("sparse parse missed tracks: %+v", ti)
	}
	full, _, _ := readMoovBox(r, start, size, hdrLen)
	fromFull, ok := parseMP4(full)
	if !ok {
		t.Fatal("full moov parse failed")
	}
	if fromFull.Audio != ti.Audio || fromFull.Subs != ti.Subs || fromFull.Video != ti.Video {
		t.Fatalf("sparse %d/%d/%d disagrees with full %d/%d/%d",
			ti.Video, ti.Audio, ti.Subs, fromFull.Video, fromFull.Audio, fromFull.Subs)
	}
}
