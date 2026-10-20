package main

import "testing"

func TestPickAliases(t *testing.T) {
	got := pickAliases("Re:ZERO -Starting Life in Another World-", []string{
		"Re:ZERO -Starting Life in Another World-", // same as the original
		"Re:Zero kara Hajimeru Isekai Seikatsu",    // romaji
		"Re: Life in a different world from zero",  // synonym
		"Re：从零开始的异世界生活",                            // CJK, unusable in a latin search
		"Re:Zero რეის",                             // non-latin script
		"Re Zero",
		"ReZero",
	})
	if len(got) != 3 {
		t.Fatalf("want 3 aliases, got %d: %v", len(got), got)
	}
	if got[0] != "Re:Zero kara Hajimeru Isekai Seikatsu" {
		t.Errorf("first alias = %q", got[0])
	}
	for _, a := range got {
		if a == "Re:ZERO -Starting Life in Another World-" {
			t.Error("original title should be excluded")
		}
	}
}

func TestIsLatin(t *testing.T) {
	if !isLatin("Re:Zero kara Hajimeru Isekai Seikatsu") {
		t.Error("romaji should pass")
	}
	if isLatin("进击的巨人") {
		t.Error("CJK should fail")
	}
	if !isLatin("ab") {
		t.Error("two letters is the floor, so it should pass")
	}
}

func TestParseSize(t *testing.T) {
	cases := map[string]int64{
		"1.5 GiB":   1610612736,
		"300 MiB":   314572800,
		"1,024 KiB": 1048576,
		"12 B":      12,
		"nonsense":  0,
		"":          0,
	}
	for in, want := range cases {
		if got := parseSize(in); got != want {
			t.Errorf("parseSize(%q) = %d, want %d", in, got, want)
		}
	}
}

func TestEpisodeSignal(t *testing.T) {
	cases := []struct {
		title string
		want  int
	}{
		{"Re ZERO S04E18 1080p", 1},
		{"Re ZERO S04E19 1080p", -1},
		{"[SubsPlease] Re:Zero - 18 (1080p)", 1},
		{"[SubsPlease] Re:Zero - 19v2 (1080p)", -1},
		{"Show S04 Complete 1080p", 0},  // pack for the right season: usable
		{"Show S02 Complete 1080p", -1}, // wrong season
		{"Some Movie 2020 1080p", 0},
	}
	for _, c := range cases {
		if got := episodeSignal(c.title, 4, 18); got != c.want {
			t.Errorf("episodeSignal(%q) = %d, want %d", c.title, got, c.want)
		}
	}
}

const nyaaFixture = `<?xml version="1.0" encoding="utf-8"?>
<rss xmlns:nyaa="https://nyaa.si/xmlns/nyaa" version="2.0"><channel>
<item>
  <title>Re ZERO S04E18 1080p WEB-DL</title>
  <link>https://nyaa.si/download/2173935.torrent</link>
  <nyaa:seeders>80</nyaa:seeders>
  <nyaa:infoHash>862A878F5705BBC0C1CACC42240DF503EA3EEB4D</nyaa:infoHash>
  <nyaa:category>Anime - English-translated</nyaa:category>
  <nyaa:size>1.5 GiB</nyaa:size>
</item>
<item>
  <title>Broken entry</title>
  <nyaa:seeders>0</nyaa:seeders>
</item>
</channel></rss>`

func TestNyaaSearchParsesRSS(t *testing.T) {
	srv := testServerXML(t, nyaaFixture)
	ix := Indexer{Kind: "nyaa", Name: "nyaa", URL: srv.URL}
	res, err := nyaaSearch(t.Context(), srv.Client(), ix, "re zero")
	if err != nil {
		t.Fatal(err)
	}
	if len(res) != 1 {
		t.Fatalf("want 1 result (hashless dropped), got %d", len(res))
	}
	if res[0].InfoHash != "862a878f5705bbc0c1cacc42240df503ea3eeb4d" {
		t.Errorf("infohash = %q", res[0].InfoHash)
	}
	if res[0].Seeders != 80 || res[0].Size != 1610612736 {
		t.Errorf("seeders/size = %d/%d", res[0].Seeders, res[0].Size)
	}
}

func TestChooseFileForEpisode(t *testing.T) {
	paths := []string{
		"Show/Show - 01.mkv",
		"Show/Show - 04.mkv",
		"Show/Show - 05.mkv",
		"Show/readme.txt",
	}
	sizes := []int64{1 << 30, 900 << 20, 1 << 30, 1024}
	idx, err := chooseFileForEpisode(paths, sizes, -1, 1, 4)
	if err != nil || idx != 1 {
		t.Fatalf("episode 4 should map to file 1, got %d (%v)", idx, err)
	}
	// Unknown numbering falls back to the largest video file.
	idx, _ = chooseFileForEpisode([]string{"a/movie.mkv", "a/extra.mkv"}, []int64{2 << 30, 100}, -1, 1, 4)
	if idx != 0 {
		t.Fatalf("fallback picked %d", idx)
	}
}
