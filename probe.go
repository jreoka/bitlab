package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"io"
	"strings"
	"time"
)

// TrackInfo is what a release's container header actually says — resolution,
// how many audio and subtitle tracks it carries, their languages — plus the
// throughput observed while reading that header. Filenames lie about all of
// it, so auto ranking reads the header instead.
type TrackInfo struct {
	Container   string
	Width       int
	Height      int
	Video       int
	Audio       int
	Subs        int
	Langs       []string
	AudioCodecs []string
	Mbps        float64
	// Partial means the header was cut short by a slow swarm: the video track
	// was read but the audio and subtitle tracks may not have been. A partial
	// probe must not be read as "this release has no extra tracks".
	Partial bool
}

// heightLabel maps a container's pixel height to the "quality rung" a user
// thinks in. Width decides for scope-shaped releases (1920x804 is 1080p).
func (ti TrackInfo) heightLabel() int {
	w := ti.Width
	h := ti.Height
	if w <= 0 && h <= 0 {
		return 0
	}
	long := w
	if h > long {
		long = h
	}
	switch {
	case long >= 3000:
		return 2160
	case long >= 1800:
		return 1080
	case long >= 1200:
		return 720
	case long >= 800:
		return 576
	default:
		return 480
	}
}

// ---- EBML / Matroska ----

const (
	ebmlSegment     = 0x18538067
	ebmlTracks      = 0x1654AE6B
	ebmlTrackEntry  = 0xAE
	ebmlTrackType   = 0x83
	ebmlCodecID     = 0x86
	ebmlLanguage    = 0x22B59C
	ebmlVideo       = 0xE0
	ebmlPixelWidth  = 0xB0
	ebmlPixelHeight = 0xBA
)

// readEBMLID returns the raw element id (marker bits kept, as Matroska names
// them) and its length in bytes.
func readEBMLID(b []byte) (uint32, int, bool) {
	if len(b) == 0 || b[0] == 0 {
		return 0, 0, false
	}
	n := 0
	for i := 0; i < 4; i++ {
		if b[0]&(0x80>>uint(i)) != 0 {
			n = i + 1
			break
		}
	}
	if n == 0 || len(b) < n {
		return 0, 0, false
	}
	var id uint32
	for i := 0; i < n; i++ {
		id = id<<8 | uint32(b[i])
	}
	return id, n, true
}

// readEBMLSize returns an element's payload size and length. unknown reports
// the all-ones encoding Matroska uses for live/streamed masters.
func readEBMLSize(b []byte) (size uint64, n int, unknown bool, ok bool) {
	if len(b) == 0 || b[0] == 0 {
		return 0, 0, false, false
	}
	n = 0
	for i := 0; i < 8; i++ {
		if b[0]&(0x80>>uint(i)) != 0 {
			n = i + 1
			break
		}
	}
	if n == 0 || len(b) < n {
		return 0, 0, false, false
	}
	var v uint64
	for i := 0; i < n; i++ {
		v = v<<8 | uint64(b[i])
	}
	mask := uint64(1)<<(7*uint(n)) - 1
	v &= mask
	return v, n, v == mask, true
}

func ebmlUint(b []byte) uint64 {
	var v uint64
	for _, c := range b {
		v = v<<8 | uint64(c)
	}
	return v
}

// parseMKV walks EBML header -> Segment -> children until it finds Tracks.
// A 256 KiB head read almost always covers it; callers read more if it fails.
func parseMKV(b []byte) (TrackInfo, bool) {
	ti := TrackInfo{Container: "mkv"}
	off := 0
	for off < len(b) {
		id, n, ok := readEBMLID(b[off:])
		if !ok {
			return ti, false
		}
		size, m, unknown, ok := readEBMLSize(b[off+n:])
		if !ok {
			return ti, false
		}
		payload := off + n + m
		if id == ebmlSegment {
			p := payload
			for p < len(b) {
				cid, cn, ok := readEBMLID(b[p:])
				if !ok {
					break
				}
				csz, cm, cUnknown, ok := readEBMLSize(b[p+cn:])
				if !ok {
					break
				}
				start := p + cn + cm
				if cUnknown {
					// An unknown-size element can't be skipped. Tracks is
					// muxed before Cluster in every real release, so if we
					// hit one of these first the header is out of reach.
					break
				}
				end := start + int(csz)
				if end > len(b) {
					end = len(b)
				}
				if cid == ebmlTracks {
					return ti, parseMKVTracks(b[start:end], &ti)
				}
				p = end
			}
			return ti, false
		}
		if unknown {
			return ti, false
		}
		off = payload + int(size)
	}
	return ti, false
}

func parseMKVTracks(p []byte, ti *TrackInfo) bool {
	found := false
	for off := 0; off < len(p); {
		id, n, ok := readEBMLID(p[off:])
		if !ok {
			break
		}
		size, m, unknown, ok := readEBMLSize(p[off+n:])
		if !ok || unknown {
			break
		}
		start := off + n + m
		end := start + int(size)
		if end > len(p) {
			end = len(p)
		}
		if id == ebmlTrackEntry {
			parseMKVTrack(p[start:end], ti)
			found = true
		}
		off = end
	}
	return found && (ti.Video > 0 || ti.Audio > 0)
}

func parseMKVTrack(p []byte, ti *TrackInfo) {
	tt := 0
	lang := ""
	codec := ""
	w, h := 0, 0
	for off := 0; off < len(p); {
		id, n, ok := readEBMLID(p[off:])
		if !ok {
			break
		}
		size, m, unknown, ok := readEBMLSize(p[off+n:])
		if !ok || unknown {
			break
		}
		start := off + n + m
		end := start + int(size)
		if end > len(p) {
			end = len(p)
		}
		val := p[start:end]
		switch id {
		case ebmlTrackType:
			tt = int(ebmlUint(val))
		case ebmlLanguage:
			lang = strings.TrimRight(string(val), "\x00")
		case ebmlCodecID:
			codec = strings.TrimRight(string(val), "\x00")
		case ebmlVideo:
			w, h = parseMKVVideo(val)
		}
		off = end
	}
	switch tt {
	case 1:
		ti.Video++
		if w > 0 {
			ti.Width, ti.Height = w, h
		}
	case 2:
		ti.Audio++
		if codec != "" {
			ti.AudioCodecs = append(ti.AudioCodecs, shortCodec(codec))
		}
	case 17:
		ti.Subs++
	}
	if lang != "" && (tt == 2 || tt == 17) {
		ti.Langs = append(ti.Langs, lang)
	}
}

func parseMKVVideo(p []byte) (int, int) {
	w, h := 0, 0
	for off := 0; off < len(p); {
		id, n, ok := readEBMLID(p[off:])
		if !ok {
			break
		}
		size, m, unknown, ok := readEBMLSize(p[off+n:])
		if !ok || unknown {
			break
		}
		start := off + n + m
		end := start + int(size)
		if end > len(p) {
			end = len(p)
		}
		switch id {
		case ebmlPixelWidth:
			w = int(ebmlUint(p[start:end]))
		case ebmlPixelHeight:
			h = int(ebmlUint(p[start:end]))
		}
		off = end
	}
	return w, h
}

func shortCodec(id string) string {
	// "A_EAC3" -> "EAC3", "V_MPEGH/ISO/HEVC" -> "HEVC"
	id = strings.TrimPrefix(id, "A_")
	id = strings.TrimPrefix(id, "V_")
	if i := strings.LastIndex(id, "/"); i >= 0 {
		id = id[i+1:]
	}
	return id
}

// ---- ISO BMFF / MP4 ----

func iterBoxes(p []byte, fn func(typ string, payload []byte) bool) {
	for off := 0; off+8 <= len(p); {
		sz := int(binary.BigEndian.Uint32(p[off:]))
		typ := string(p[off+4 : off+8])
		hdr := 8
		if sz == 1 {
			if off+16 > len(p) {
				return
			}
			sz = int(binary.BigEndian.Uint64(p[off+8:]))
			hdr = 16
		}
		if sz < hdr {
			return
		}
		end := off + sz
		if dz := off + sz; dz <= 0 || dz > len(p) {
			end = len(p)
		}
		if fn(typ, p[off+hdr:end]) {
			return
		}
		off = end
	}
}

// parseMP4 reads a tail buffer containing the moov atom (video MP4s usually
// put moov last) and counts track handlers.
func parseMP4(b []byte) (TrackInfo, bool) {
	ti := TrackInfo{Container: "mp4"}
	i := bytes.Index(b, []byte("moov"))
	if i < 4 {
		return ti, false
	}
	start := i - 4
	sz := int(binary.BigEndian.Uint32(b[start:]))
	hdr := 8
	if sz == 1 {
		if start+16 > len(b) {
			return ti, false
		}
		sz = int(binary.BigEndian.Uint64(b[start+8:]))
		hdr = 16
	}
	if sz < hdr || start+sz > len(b) {
		sz = len(b) - start
	}
	moov := b[start+hdr : start+sz]
	iterBoxes(moov, func(typ string, payload []byte) bool {
		if typ != "trak" {
			return false
		}
		parseMP4Track(payload, &ti)
		return false
	})
	return ti, ti.Video > 0 || ti.Audio > 0
}

func parseMP4Track(p []byte, ti *TrackInfo) { countMP4TrackPrefix(p, ti) }

// tkhdDims reads the 16.16 fixed-point width/height that close the tkhd box.
// The offset moved between spec revisions, so accept the first plausible pair.
func tkhdDims(p []byte) (int, int) {
	for _, off := range []int{76, 72, 84} {
		if off+8 > len(p) {
			continue
		}
		w := int(binary.BigEndian.Uint32(p[off:]) >> 16)
		h := int(binary.BigEndian.Uint32(p[off+4:]) >> 16)
		if w >= 320 && w <= 7680 && h >= 180 && h <= 4320 && w >= h {
			return w, h
		}
	}
	return 0, 0
}

// ---- probing a live release ----

const (
	probeHeadBytes = 4 << 20 // Matroska Tracks lives in the first few hundred KiB
	probeTailBytes = 8 << 20 // MP4 moov can sit at the end of the file
	probeChunk     = 256 << 10
	maxMoovBytes   = 16 << 20 // a moov bigger than this is pathological
)

// probeRelease opens one candidate, reads just enough of the container header
// to learn its real tracks, and times that read. A release that cannot deliver
// its own header cannot stream, so the timeout doubles as a health check.
func (s *Server) probeRelease(ctx context.Context, res Result) (TrackInfo, bool) {
	select {
	case s.probeSem <- struct{}{}:
		defer func() { <-s.probeSem }()
	case <-ctx.Done():
		return TrackInfo{}, false
	}

	t, err := s.engine.ensure(res.InfoHash, res.Title, res.Trackers)
	if err != nil {
		return TrackInfo{}, false
	}
	if err := waitInfo(ctx, t); err != nil {
		s.log.Printf("probe %s: metadata: %v", res.InfoHash[:8], err)
		return TrackInfo{}, false
	}
	s.engine.recordMedia(t)

	files := t.Files()
	if len(files) == 0 {
		return TrackInfo{}, false
	}
	paths := make([]string, len(files))
	sizes := make([]int64, len(files))
	for i, f := range files {
		paths[i] = f.DisplayPath()
		sizes[i] = f.Length()
	}
	idx, err := chooseFile(paths, sizes, -1)
	if err != nil {
		return TrackInfo{}, false
	}
	f := files[idx]

	r := f.NewReader()
	defer r.Close()
	r.SetReadahead(0)
	r.SetResponsive()
	r.SetContext(ctx)
	before := t.BytesCompleted()

	var (
		ti    TrackInfo
		read  int64
		start = time.Now()
		buf   = make([]byte, probeChunk)
		head  = make([]byte, 0, probeHeadBytes)
	)

	// Head first: Matroska puts Tracks in the first few hundred KiB, and that is
	// the whole header for most releases.
	for off := int64(0); off < probeHeadBytes; off += probeChunk {
		n, err := readAt(r, buf, off)
		if n > 0 {
			read += int64(n)
			head = append(head, buf[:n]...)
		}
		if err != nil || n == 0 {
			break
		}
		if ext, _ := sniffContainer(head); ext == ".mkv" {
			if info, ok := parseMKV(head); ok {
				ti = info
				break
			}
		}
	}
	// MP4 keeps its tracks in `moov`, which is either at the front or at the very
	// end. Either way the audio and subtitle traks sit *after* the video one, and
	// a video trak's sample tables alone run to megabytes — so the box has to be
	// read whole rather than guessed at with a fixed window.
	if ti.Video == 0 {
		if start, size, hdrLen, n, ok := locateMoov(r, f.Length()); ok {
			read += n
			// Sparse first: the handlers live in the first bytes of each trak,
			// and skipping the sample tables by size is the difference between
			// reading 200 KiB and 20 MiB.
			info, n2, complete := sparseMP4(r, start, size, hdrLen)
			read += n2
			if complete || info.Video > 0 || info.Audio > 0 {
				ti = info
				ti.Partial = !complete
			} else if moov, n3, complete3 := readMoovBox(r, start, size, hdrLen); moov != nil {
				read += n3
				if info, ok := parseMP4(moov); ok {
					ti = info
					ti.Partial = !complete3
				}
			}
		}
	}

	elapsed := time.Since(start).Seconds()
	if elapsed > 0 {
		// What the swarm delivered for this torrent while we were probing — not
		// just our own bytes. The warm pass is usually prebuffering the same
		// release, and its traffic counts: the question is whether the swarm
		// can feed a player, not whether one reader was fast.
		pulled := t.BytesCompleted() - before
		if pulled < read {
			pulled = read
		}
		ti.Mbps = float64(pulled) * 8 / elapsed / 1e6
		if ti.Mbps > 500 { // already on disk; the gate should treat it as instant
			ti.Mbps = 500
		}
	}
	ti.Langs = dedupeStrings(ti.Langs)
	if ti.Container == "" {
		ti.Container = "?"
	}
	return ti, ti.Video > 0 || ti.Audio > 0
}

// locateMoov finds the MP4 `moov` box without reading it: the head first
// (faststart files), then the tail, widening the tail window only when a
// smaller one misses. A big multi-audio MP4 can keep several megabytes of moov
// at the end, but most keep far less, and a fixed 16 MiB tail read would cost
// more swarm time than the tracks are worth.
func locateMoov(r byteSource, fileSize int64) (start int64, size, hdrLen int, read int64, ok bool) {
	if fileSize < 16 {
		return 0, 0, 0, 0, false
	}
	headN := int64(probeChunk * 4)
	if headN > fileSize {
		headN = fileSize
	}
	win := make([]byte, headN)
	n, _ := readAt(r, win, 0)
	if n <= 0 {
		return 0, 0, 0, 0, false
	}
	read += int64(n)
	base := int64(0)
	win = win[:n]

	bs := findMoovBox(win)
	if bs < 0 {
		for _, tailN := range []int64{1 << 20, 4 << 20, maxMoovBytes} {
			if tailN > fileSize {
				tailN = fileSize
			}
			if tailN <= int64(len(win)) && base == 0 {
				continue // the head window already covered this much
			}
			tail := make([]byte, tailN)
			m, _ := readAt(r, tail, fileSize-tailN)
			if m <= 0 {
				return 0, 0, 0, read, false
			}
			read += int64(m)
			win, base = tail[:m], fileSize-tailN
			if bs = findMoovBox(win); bs >= 0 {
				break
			}
		}
		if bs < 0 {
			return 0, 0, 0, read, false
		}
	}

	size = int(binary.BigEndian.Uint32(win[bs:]))
	hdrLen = 8
	if size == 1 {
		size = int(binary.BigEndian.Uint64(win[bs+8:]))
		hdrLen = 16
	}
	start = base + int64(bs)
	// Clamp a declared size that runs past the end of the file: some muxers
	// write 0 or a stale length.
	if avail := fileSize - start; start+int64(size) > fileSize || size <= hdrLen {
		size = int(avail)
	}
	return start, size, hdrLen, read, size > hdrLen
}

// readMoovBox reads a located moov whole, capped so a pathological box cannot
// eat the probe budget. complete is false when the cap or a slow swarm cut it.
func readMoovBox(r byteSource, start int64, size, hdrLen int) ([]byte, int64, bool) {
	if size > maxMoovBytes {
		size = maxMoovBytes
	}
	box := make([]byte, size)
	m, _ := readAt(r, box, start)
	if m <= hdrLen {
		return nil, int64(m), false
	}
	// Rewrite the declared size to what we actually hold, so parseMP4 clamps
	// instead of rejecting a header that overruns the buffer.
	binary.BigEndian.PutUint32(box[:4], uint32(m))
	return box[:m], int64(m), m >= size
}

// sparseMP4 walks the moov's top-level boxes, reading only each box header and,
// for a trak, the first 512 bytes — where tkhd and the mdia handler live. The
// sample tables that follow are what make a moov tens of megabytes.
func sparseMP4(r byteSource, start int64, size, hdrLen int) (TrackInfo, int64, bool) {
	ti := TrackInfo{Container: "mp4"}
	var read int64
	off := start + int64(hdrLen)
	end := start + int64(size)
	complete := true
	for off+8 <= end {
		var hb [16]byte
		n, _ := readAt(r, hb[:], off)
		if n < 8 {
			complete = false
			break
		}
		read += int64(n)
		sz := int(binary.BigEndian.Uint32(hb[:4]))
		typ := string(hb[4:8])
		ch := 8
		if sz == 1 {
			if n < 16 {
				complete = false
				break
			}
			sz = int(binary.BigEndian.Uint64(hb[8:16]))
			ch = 16
		}
		if sz < ch {
			complete = false
			break
		}
		if typ == "trak" {
			want := sz - ch
			if want > 512 {
				want = 512
			}
			buf := make([]byte, want)
			m, _ := readAt(r, buf, off+int64(ch))
			read += int64(m)
			if m < want {
				complete = false // the trak header itself was cut short
			}
			if m > 0 {
				countMP4TrackPrefix(buf[:m], &ti)
			}
		}
		off += int64(sz)
	}
	if off < end {
		complete = false
	}
	return ti, read, complete
}

// countMP4TrackPrefix counts one trak from its opening bytes.
func countMP4TrackPrefix(p []byte, ti *TrackInfo) {
	handler := ""
	w, h := 0, 0
	iterBoxes(p, func(typ string, payload []byte) bool {
		switch typ {
		case "tkhd":
			w, h = tkhdDims(payload)
		case "mdia":
			iterBoxes(payload, func(t2 string, p2 []byte) bool {
				if t2 == "hdlr" && len(p2) >= 12 {
					handler = string(p2[8:12])
				}
				return false
			})
		}
		return false
	})
	switch handler {
	case "vide":
		ti.Video++
		if w > 0 {
			ti.Width, ti.Height = w, h
		}
	case "soun":
		ti.Audio++
	case "subt", "sbtl", "text":
		ti.Subs++
	}
}

// findMoovBox returns the offset of a `moov` box header whose declared size is
// plausible. Scanning for the literal alone is not enough: a film's mdat will
// occasionally contain those four bytes, and a bogus offset makes the size
// field nonsense.
func findMoovBox(win []byte) int {
	best := -1
	for from := 0; ; {
		i := bytes.Index(win[from:], []byte("moov"))
		if i < 0 || i+from < 4 {
			return best
		}
		i += from
		bs := i - 4
		sz := int(binary.BigEndian.Uint32(win[bs:]))
		hdr := 8
		if sz == 1 && bs+16 <= len(win) {
			sz = int(binary.BigEndian.Uint64(win[bs+8:]))
			hdr = 16
		}
		if sz >= hdr && sz <= maxMoovBytes {
			if bs+sz == len(win) {
				return bs // ends exactly at the end of the window: this is it
			}
			best = bs
		}
		from = i + 4
	}
}

// byteSource is the read side of one file inside a torrent. It is deliberately
// narrower than torrent.Reader so a container header can be parsed from a plain
// in-memory buffer in tests.
type byteSource interface {
	io.ReadSeeker
	io.Closer
	SetReadahead(int64)
	SetResponsive()
	SetContext(context.Context)
}

// readAt is Seek+Read: the reader is an io.ReadSeeker, not an io.ReaderAt, so
// random access to a container header is two calls.
func readAt(r byteSource, p []byte, off int64) (int, error) {
	if _, err := r.Seek(off, io.SeekStart); err != nil {
		return 0, err
	}
	return io.ReadFull(r, p)
}

func dedupeStrings(in []string) []string {
	seen := make(map[string]bool, len(in))
	out := in[:0]
	for _, s := range in {
		s = strings.TrimSpace(s)
		if s == "" || seen[s] {
			continue
		}
		seen[s] = true
		out = append(out, s)
	}
	return out
}
