package main

import (
	"os"
	"testing"
)

// TestParseRealContainer checks the header parsers against files produced by a
// real muxer, not against my own encoder. Synthetic tests prove the parser
// agrees with the test fixture; this one proves it agrees with ffmpeg.
//
//	ffmpeg -f lavfi -i testsrc=size=1920x1080 -f lavfi -i sine -f lavfi -i sine \
//	       -i eng.srt -i fre.srt -map 0:v -map 1:a -map 2:a -map 3:s -map 4:s \
//	       -c:v libx264 -c:a aac -c:s srt probe.mkv
//
// Run with BITLAB_PROBE_MKV and/or BITLAB_PROBE_MP4 pointing at such files.
func TestParseRealContainer(t *testing.T) {
	mkv := os.Getenv("BITLAB_PROBE_MKV")
	mp4 := os.Getenv("BITLAB_PROBE_MP4")
	if mkv == "" && mp4 == "" {
		t.Skip("set BITLAB_PROBE_MKV / BITLAB_PROBE_MP4 to a real media file")
	}
	if mkv != "" {
		b, err := os.ReadFile(mkv)
		if err != nil {
			t.Fatalf("read %s: %v", mkv, err)
		}
		ti, ok := parseMKV(b)
		if !ok {
			t.Fatalf("%s: parseMKV failed", mkv)
		}
		t.Logf("mkv: %dx%d video=%d audio=%d subs=%d langs=%v codecs=%v",
			ti.Width, ti.Height, ti.Video, ti.Audio, ti.Subs, ti.Langs, ti.AudioCodecs)
		if ti.Video != 1 || ti.Audio < 1 || ti.Subs < 1 {
			t.Errorf("mkv track counts look wrong: %+v", ti)
		}
		if ti.Width == 0 || ti.Height == 0 {
			t.Errorf("mkv resolution was not parsed")
		}
	}
	if mp4 != "" {
		b, err := os.ReadFile(mp4)
		if err != nil {
			t.Fatalf("read %s: %v", mp4, err)
		}
		ti, ok := parseMP4(b)
		if !ok {
			t.Fatalf("%s: parseMP4 failed", mp4)
		}
		t.Logf("mp4: %dx%d video=%d audio=%d subs=%d", ti.Width, ti.Height, ti.Video, ti.Audio, ti.Subs)
		if ti.Video != 1 || ti.Audio < 1 {
			t.Errorf("mp4 track counts look wrong: %+v", ti)
		}
		if ti.Width == 0 || ti.Height == 0 {
			t.Errorf("mp4 resolution was not parsed")
		}
	}
}
