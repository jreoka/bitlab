package main

import (
	"bytes"
	"encoding/binary"
	"path/filepath"
	"testing"
	"time"
)

// ---- tier classification ----

func TestClassifyTier(t *testing.T) {
	cases := []struct {
		title string
		want  srcTier
	}{
		{"Dune.Part.Two.2024.1080p.WEB-DL.DDP5.1.Atmos.H.264-FLUX", tierWeb},
		{"Dune.Part.Two.2024.2160p.UHD.BluRay.REMUX.DV.HDR.HEVC", tierDisc},
		{"Dune.Part.Two.2024.1080p.BluRay.x264-SPARKS", tierDisc},
		{"Some.Show.S01E01.1080p.HDTV.x264-GROUP", tierTv},
		{"Dune.Part.Two.2024.1080p.WEBRip.x264-RARBG", tierRip},
		{"Dune.Part.Two.2024.1080p.HDCAM.x264-CAM", tierCam},
		{"Dune.Part.Two.2024.HDTS.x264", tierCam},
		{"Dune.Part.Two.2024.TELESYNC.XviD", tierCam},
		{"Dune.Part.Two.2024.DVDSCR.x264", tierCam},
		{"Dune.Part.Two.2024.1080p.BluRay.3D.HSBS.H264", tierCam},
		{"Dune.Part.Two.2024.HOU.1080p.BluRay", tierCam},
		{"[ENTE] Re Zero Kara Hajimeru Isekai Seikatsu (2016) S01E01 [AV1] [OPUS] [BD] [1080p]", tierDisc},
		{"[SubsPlease] Some Anime - 05 (1080p) [WEB] [AAC]", tierWeb},
		{"Dune Part Two 2024 1080p", tierUnknown},
	}
	for _, c := range cases {
		if got := classifyTier(c.title); got != c.want {
			t.Errorf("classifyTier(%q) = %v, want %v", c.title, got, c.want)
		}
	}
}

func TestResolutionFromName(t *testing.T) {
	cases := map[string]int{
		"Movie.2024.2160p.WEB-DL": 2160,
		"Movie.2024.4K.HDR":       2160,
		"Movie.2024.1080p.BluRay": 1080,
		"Movie.2024.1080i.HDTV":   1080,
		"Movie.2024.720p.WEBRip":  720,
		"Movie.2024.480p.DVDRip":  480,
		"Movie.2024":              0,
	}
	for title, want := range cases {
		if got := resolutionFromName(title); got != want {
			t.Errorf("resolutionFromName(%q) = %d, want %d", title, got, want)
		}
	}
}

// ---- the gate ----

func cand(title string, res int, sizeGB, have float64) *autoCand {
	return &autoCand{
		Result: Result{Title: title, Size: int64(sizeGB * float64(1<<30))},
		tier:   classifyTier(title),
		res:    res,
		need:   0, // set below from size, as pickAuto does
		have:   have,
	}
}

func movieCand(title string, res int, sizeGB, have float64) *autoCand {
	c := cand(title, res, sizeGB, have)
	c.need = neededMbps(c.Result, "movie", 0)
	return c
}

// A 4K remux that can only just sustain its bitrate must lose to a healthy
// 1080p WEB-DL; the whole point of auto is that it does not chase the number.
func TestGatePrefersPlayableOverHighestResolution(t *testing.T) {
	remux := movieCand("Movie.2024.2160p.BluRay.REMUX.HEVC", 2160, 40, 40) // 44 Mbps needed
	web := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 8, 40)         //  8.9 Mbps needed

	if got := remux.have / remux.need; got >= gateHeadroom {
		t.Fatalf("test setup: remux headroom %.2f should fail the gate", got)
	}
	best, _ := chooseAuto([]*autoCand{remux, web}, nil)
	if best != web {
		t.Fatalf("auto chose %q, want the web-dl", best.Title)
	}
}

// With room to spare the same 4K should win on picture quality instead.
func TestGateLets4KWinWhenItCanStream(t *testing.T) {
	remux := movieCand("Movie.2024.2160p.BluRay.REMUX.HEVC", 2160, 20, 140) // 22 Mbps needed, 6x room
	web := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 8, 40)

	best, _ := chooseAuto([]*autoCand{web, remux}, nil)
	if best != remux {
		t.Fatalf("auto chose %q, want the remux", best.Title)
	}
}

// More tracks break a tie between otherwise identical releases.
func TestTrackCountBreaksTies(t *testing.T) {
	base := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 8, 40)
	base.probed = true
	base.tracks = TrackInfo{Video: 1, Audio: 1, Subs: 0}

	rich := movieCand("Movie.2024.1080p.WEB-DL.H.264-GROUP", 1080, 8, 40)
	rich.probed = true
	rich.tracks = TrackInfo{Video: 1, Audio: 3, Subs: 12}

	best, _ := chooseAuto([]*autoCand{base, rich}, nil)
	if best != rich {
		t.Fatalf("auto chose the release with fewer tracks")
	}
}

// A release whose throughput was measured beats one that only looks fast on a
// seeder estimate — that is the difference between evidence and a guess.
func TestMeasuredCandidateBeatsUnmeasured(t *testing.T) {
	guessed := movieCand("Movie.2024.2160p.WEB-DL.H.265", 2160, 20, 200) // 22 Mbps need, "9x" on paper
	guessed.probed = false

	seen := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 8, 30) // 8.9 Mbps need, 3.4x measured
	seen.probed = true

	best, _ := chooseAuto([]*autoCand{guessed, seen}, nil)
	if best != seen {
		t.Fatalf("auto chose the unmeasured release %q", best.Title)
	}
}

// When nothing clears the gate, the fallback must prefer "might actually
// stream" over picture quality: a 4K that cannot obviously sustain its bitrate
// is the classic minute-long spinner.
func TestFallbackPrefersHeadroomOverResolution(t *testing.T) {
	uhd := movieCand("Movie.2024.2160p.WEB-DL.H.265", 2160, 20, 25) // 22 Mbps need, 1.1x
	hd := movieCand("Movie.2024.1080p.WEBRip.x264", 1080, 7, 6)     // 7.8 Mbps need, 0.77x
	hd.measured = true

	if got := chooseFallback([]*autoCand{uhd, hd}); got != hd {
		t.Fatalf("fallback picked %q", got.Title)
	}
}

// A 4K with real room still wins; the discount is about doubt, not resolution.
func TestFallbackKeeps4KWhenItHasRoom(t *testing.T) {
	uhd := movieCand("Movie.2024.2160p.BluRay.REMUX.HEVC", 2160, 20, 60) // 1.7x measured
	uhd.measured = true
	hd := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 7, 8)
	hd.measured = true

	if got := chooseFallback([]*autoCand{hd, uhd}); got != uhd {
		t.Fatalf("fallback picked %q", got.Title)
	}
}

// A measured release that is merely tight beats an unmeasured one that looks
// comfortable on a seeder estimate.
func TestFallbackTrustsMeasurement(t *testing.T) {
	guess := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 7, 9)  // 1.15x, estimated
	seen := movieCand("Movie.2024.1080p.WEB-DL.AAC-GRP", 1080, 7, 7) // 0.89x, measured
	seen.measured = true

	if got := chooseFallback([]*autoCand{guess, seen}); got != seen {
		t.Fatalf("fallback picked %q", got.Title)
	}
}

// A candidate demoted by a real measurement must not come back on the next pass.
func TestChooseAutoSkipsDemotedCandidates(t *testing.T) {
	slow := movieCand("Movie.2024.1080p.BluRay.REMUX.HEVC", 1080, 12, 60)
	slow.probed, slow.measured, slow.gated = true, true, true

	ok := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 8, 40)
	ok.probed, ok.measured = true, true

	best, _ := chooseAuto([]*autoCand{slow, ok}, nil)
	if best != ok {
		t.Fatalf("auto chose a demoted candidate")
	}
}

func TestChooseAutoFallsBackWhenNothingPasses(t *testing.T) {
	slow := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 8, 1)
	best, fallback := chooseAuto([]*autoCand{slow}, nil)
	if best != nil {
		t.Fatalf("expected no gated pick, got %q", best.Title)
	}
	if fallback != slow {
		t.Fatal("expected the fallback to be the only candidate")
	}
}

func TestNeededMbpsDividesSeasonPack(t *testing.T) {
	ep := Result{Title: "Some.Show.S02E03.1080p.WEB-DL", Size: 12 << 30}
	pack := Result{Title: "Some.Show.S02.1080p.WEB-DL", Size: 144 << 30}
	// A 12 GiB file over 3000s is ~34 Mbps either way; a pack is judged by one
	// twelfth of its size, so the two land in the same place.
	if got, want := neededMbps(pack, "series", 2), neededMbps(ep, "series", 2); got != want {
		t.Fatalf("pack %.1f != episode %.1f", got, want)
	}
}

// ---- container parsing ----

func vintSize(n int) []byte {
	b := make([]byte, 8)
	b[0] = 0x01
	v := uint64(n)
	for i := 7; i >= 1; i-- {
		b[i] = byte(v)
		v >>= 8
	}
	return b
}

func ebmlElem(id []byte, payload []byte) []byte {
	out := append([]byte{}, id...)
	out = append(out, vintSize(len(payload))...)
	return append(out, payload...)
}

func ebmlUintElem(id byte, v uint64) []byte {
	var body []byte
	for v > 0 {
		body = append([]byte{byte(v)}, body...)
		v >>= 8
	}
	if len(body) == 0 {
		body = []byte{0}
	}
	return ebmlElem([]byte{id}, body)
}

func mkvTrackEntry(tt byte, lang string, w, h uint64) []byte {
	body := ebmlUintElem(ebmlTrackType, uint64(tt))
	if lang != "" {
		body = append(body, ebmlElem([]byte{0x22, 0xB5, 0x9C}, []byte(lang))...)
	}
	if tt == 2 {
		body = append(body, ebmlElem([]byte{0x86}, []byte("A_EAC3"))...)
	}
	if tt == 1 {
		video := append(ebmlUintElem(ebmlPixelWidth, w), ebmlUintElem(ebmlPixelHeight, h)...)
		body = append(body, ebmlElem([]byte{0xE0}, video)...)
	}
	return ebmlElem([]byte{0xAE}, body)
}

func TestParseMKV(t *testing.T) {
	tracks := bytes.Join([][]byte{
		mkvTrackEntry(1, "", 1920, 1080),
		mkvTrackEntry(2, "eng", 0, 0),
		mkvTrackEntry(2, "jpn", 0, 0),
		mkvTrackEntry(17, "eng", 0, 0),
	}, nil)
	doc := bytes.Join([][]byte{
		ebmlElem([]byte{0x1A, 0x45, 0xDF, 0xA3}, []byte("headerbytes")),
		ebmlElem([]byte{0x18, 0x53, 0x80, 0x67}, ebmlElem([]byte{0x16, 0x54, 0xAE, 0x6B}, tracks)),
	}, nil)

	ti, ok := parseMKV(doc)
	if !ok {
		t.Fatal("parseMKV failed")
	}
	if ti.Width != 1920 || ti.Height != 1080 {
		t.Errorf("dims = %dx%d", ti.Width, ti.Height)
	}
	if ti.Video != 1 || ti.Audio != 2 || ti.Subs != 1 {
		t.Errorf("tracks = video %d audio %d subs %d", ti.Video, ti.Audio, ti.Subs)
	}
	if ti.heightLabel() != 1080 {
		t.Errorf("heightLabel = %d", ti.heightLabel())
	}
	if len(ti.Langs) != 3 {
		t.Errorf("langs = %v", ti.Langs)
	}
	if len(ti.AudioCodecs) != 2 || ti.AudioCodecs[0] != "EAC3" {
		t.Errorf("codecs = %v", ti.AudioCodecs)
	}
}

func TestParseMKVUnknownSizeSegment(t *testing.T) {
	// Streamed muxes write the all-ones size for Segment; children must still
	// be walked.
	tracks := mkvTrackEntry(1, "", 3840, 2160)
	seg := append([]byte{0x18, 0x53, 0x80, 0x67}, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF)
	seg = append(seg, ebmlElem([]byte{0x16, 0x54, 0xAE, 0x6B}, tracks)...)

	ti, ok := parseMKV(seg)
	if !ok {
		t.Fatal("parseMKV failed on unknown-size segment")
	}
	if ti.Width != 3840 || ti.heightLabel() != 2160 {
		t.Errorf("dims = %dx%d", ti.Width, ti.Height)
	}
}

func mp4Box(typ string, payload []byte) []byte {
	hdr := make([]byte, 8)
	binary.BigEndian.PutUint32(hdr, uint32(8+len(payload)))
	copy(hdr[4:], typ)
	return append(hdr, payload...)
}

func mp4Track(handler string, w, h uint32) []byte {
	tkhd := make([]byte, 84)
	binary.BigEndian.PutUint32(tkhd[76:], w<<16)
	binary.BigEndian.PutUint32(tkhd[80:], h<<16)
	hdlr := make([]byte, 24) // version/flags, pre_defined, handler, reserved
	copy(hdlr[8:12], handler)
	return mp4Box("trak", bytes.Join([][]byte{mp4Box("tkhd", tkhd), mp4Box("mdia", mp4Box("hdlr", hdlr))}, nil))
}

func TestParseMP4(t *testing.T) {
	moov := mp4Box("moov", bytes.Join([][]byte{
		mp4Track("vide", 1920, 1080),
		mp4Track("soun", 0, 0),
		mp4Track("soun", 0, 0),
		mp4Track("subt", 0, 0),
	}, nil))
	// moov at the tail of the buffer, as a non-faststart file would look.
	buf := append(bytes.Repeat([]byte{0x00}, 64), moov...)

	ti, ok := parseMP4(buf)
	if !ok {
		t.Fatal("parseMP4 failed")
	}
	if ti.Video != 1 || ti.Audio != 2 || ti.Subs != 1 {
		t.Fatalf("tracks = video %d audio %d subs %d", ti.Video, ti.Audio, ti.Subs)
	}
	if ti.Width != 1920 || ti.Height != 1080 {
		t.Errorf("dims = %dx%d", ti.Width, ti.Height)
	}
}

func TestHeightLabelUsesLongEdge(t *testing.T) {
	cases := []struct {
		w, h int
		want int
	}{
		{1920, 800, 1080},
		{3840, 1600, 2160},
		{1280, 720, 720},
		{0, 0, 0},
	}
	for _, c := range cases {
		ti := TrackInfo{Width: c.w, Height: c.h}
		if got := ti.heightLabel(); got != c.want {
			t.Errorf("heightLabel(%dx%d) = %d, want %d", c.w, c.h, got, c.want)
		}
	}
}

// ---- store: stall memory and probe cache ----

func testStore(t *testing.T) *Store {
	t.Helper()
	s, err := OpenStore(filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("open store: %v", err)
	}
	t.Cleanup(func() { s.Close() })
	return s
}

func TestStallMemoryRoundTrip(t *testing.T) {
	s := testStore(t)
	const ih = "0123456789abcdef0123456789abcdef01234567"

	if len(s.StalledHashes("tt1234567:1:1", time.Hour)) != 0 {
		t.Fatal("fresh store should have no stalls")
	}
	s.RecordStall(ih, "tt1234567:1:1")
	got := s.StalledHashes("tt1234567:1:1", time.Hour)
	if !got[ih] {
		t.Fatal("stall was not remembered")
	}
	// A stall belongs to one episode, not to the release everywhere.
	if s.StalledHashes("tt1234567:1:2", time.Hour)[ih] {
		t.Fatal("stall leaked to another episode")
	}
	// An aged-out stall must not demote anything.
	if s.StalledHashes("tt1234567:1:1", -time.Second)[ih] {
		t.Fatal("stall outlived its window")
	}
}

func TestProbeCacheRoundTrip(t *testing.T) {
	s := testStore(t)
	const ih = "89abcdef0123456789abcdef0123456789abcdef"

	if _, ok := s.GetProbe(ih); ok {
		t.Fatal("empty cache returned a probe")
	}
	s.SaveProbe(ih, 2, TrackInfo{
		Container: "mkv", Width: 1920, Height: 1080,
		Video: 1, Audio: 3, Subs: 7, Langs: []string{"eng", "jpn"},
		AudioCodecs: []string{"EAC3", "AC3"}, Mbps: 22.5,
	})
	p, ok := s.GetProbe(ih)
	if !ok {
		t.Fatal("probe not cached")
	}
	if p.Audio != 3 || p.Subs != 7 || p.Width != 1920 || p.FileIdx != 2 {
		t.Fatalf("probe round-trip lost data: %+v", p)
	}
	if len(p.Langs) != 2 || p.AudioCodecs[0] != "EAC3" {
		t.Fatalf("probe slices lost: %+v", p)
	}
	if p.Mbps != 22.5 {
		t.Fatalf("mbps = %v", p.Mbps)
	}
}

func TestPicksForHash(t *testing.T) {
	s := testStore(t)
	const ih = "cafebabecafebabecafebabecafebabecafebabe"

	s.SaveAutoPick("tt1:1:1", ih, "2160p WEB-DL", 80, 24)
	if got := s.PicksForHash(ih); got["tt1:1:1"] != 24 {
		t.Fatalf("PicksForHash = %v", got)
	}
	if len(s.PicksForHash("0000000000000000000000000000000000000000")) != 0 {
		t.Fatal("a different hash should have no picks")
	}
	// Demotion is keyed by (hash, media), so the same release can still be used
	// for another episode.
	s.RecordStall(ih, "tt1:1:1")
	if !s.StalledHashes("tt1:1:1", 6*time.Hour)[ih] {
		t.Fatal("stall not recorded")
	}
	if s.StalledHashes("tt1:1:2", 6*time.Hour)[ih] {
		t.Fatal("stall leaked across episodes")
	}
}

func TestAutoPickHistory(t *testing.T) {
	s := testStore(t)
	const ih = "ba5eba11ba5eba11ba5eba11ba5eba11ba5eba11"

	if _, _, _, ok := s.GetAutoPick("tt1"); ok {
		t.Fatal("history should start empty")
	}
	s.SaveAutoPick("tt1", ih, "1080p WEB-DL · 3 audio", 81.5, 9)
	s.SaveAutoPick("tt1", ih, "2160p BluRay · 2 audio", 90, 24) // upsert, same key
	gotIH, label, at, ok := s.GetAutoPick("tt1")
	if !ok || gotIH != ih {
		t.Fatalf("pick history = %q %v", gotIH, ok)
	}
	if label != "2160p BluRay · 2 audio" {
		t.Fatalf("label = %q", label)
	}
	if time.Since(time.Unix(at, 0)) > time.Minute {
		t.Fatal("pick timestamp is not recent")
	}
}

// findMoovBox has to survive the literal "moov" appearing inside mdat data,
// which makes the preceding 4 bytes a nonsense box size.
func TestFindMoovBoxPrefersTheRealAtom(t *testing.T) {
	moov := mp4Box("moov", mp4Track("vide", 1920, 800))
	decoy := []byte("\x00\x00\x00\x00moov\xde\xad\xbe\xef")
	buf := append(append([]byte{}, decoy...), moov...)
	bs := findMoovBox(buf)
	if bs != len(decoy) {
		t.Fatalf("findMoovBox = %d, want %d", bs, len(decoy))
	}
}

// A moov cut short must still yield the video trak rather than nothing: a
// partial answer beats falling back to the filename.
func TestParseMP4TruncatedMoovKeepsVideo(t *testing.T) {
	body := append(append([]byte{}, mp4Track("vide", 1920, 800)...), 0, 0, 0, 16, 'm', 'd', 'a', 't')
	short := mp4Box("moov", body)
	short = short[:len(short)-5]

	ti, ok := parseMP4(short)
	if !ok {
		t.Fatal("parseMP4 rejected a truncated moov")
	}
	if ti.Video != 1 || ti.Width != 1920 {
		t.Fatalf("truncated moov lost the video trak: %+v", ti)
	}
}

// A moov cut short by a slow swarm says nothing about how many audio tracks a
// release has, so it must not be scored as if it had none.
func TestPartialProbeDoesNotPenaliseOptions(t *testing.T) {
	full := movieCand("Movie.2024.1080p.WEB-DL.H.264", 1080, 8, 40)
	full.probed = true
	full.tracks = TrackInfo{Video: 1, Audio: 0, Subs: 0, Partial: true}

	same := movieCand("Movie.2024.1080p.WEB-DL.H.264-GRP", 1080, 8, 40)
	same.probed = true
	same.tracks = TrackInfo{Video: 1, Audio: 0, Subs: 0, Partial: true}

	full.evaluate()
	same.evaluate()
	if full.score != same.score {
		t.Fatalf("partial probes scored differently: %v vs %v", full.score, same.score)
	}
	// And the neutral options weight is what an unprobed candidate gets.
	unprobed := movieCand("Movie.2024.1080p.WEB-DL.H.264-X", 1080, 8, 40)
	unprobed.evaluate()
	if got, want := full.score, unprobed.score; got != want {
		t.Fatalf("partial probe scored %v, unprobed scored %v", got, want)
	}
}

func TestRangeStart(t *testing.T) {
	cases := []struct {
		h    string
		want int64
		ok   bool
	}{
		{"bytes=0-", 0, true},
		{"bytes=1874183985-", 1874183985, true},
		{"bytes=0-262143", 0, true},
		{"bytes=1048576-2097151", 1048576, true},
		{"bytes=500-600,700-800", 500, true},
		{"bytes=-500", 0, false}, // suffix range: no start offset
		{"", 0, false},
		{"items=0-", 0, false},
	}
	for _, c := range cases {
		got, ok := rangeStart(c.h)
		if ok != c.ok || (ok && got != c.want) {
			t.Errorf("rangeStart(%q) = %d,%v want %d,%v", c.h, got, ok, c.want, c.ok)
		}
	}
}

// ---- auto row ----
func TestAutoRowIsAStableBingeGroup(t *testing.T) {
	s := &Server{store: testStore(t)}
	row := s.autoRow("https://stream.example", Key{Token: "bit_abc"}, "series", "tt123:1:2")
	if row.Name != "Auto" {
		t.Errorf("name = %q", row.Name)
	}
	if want := "https://stream.example/k/bit_abc/auto/series/tt123:1:2"; row.URL != want {
		t.Errorf("url = %q, want %q", row.URL, want)
	}
	if g, _ := row.BehaviorHints["bingeGroup"].(string); g != "bitlab-auto" {
		t.Errorf("bingeGroup = %v", row.BehaviorHints["bingeGroup"])
	}
	if row.Title == "" {
		t.Error("detail line is empty")
	}
}
