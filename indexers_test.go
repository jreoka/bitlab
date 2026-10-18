package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func jsonUnmarshal(b []byte, v any) error { return json.Unmarshal(b, v) }

const torznabFixture = `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
  <title>Test</title>
  <item>
    <title>The.Shawshank.Redemption.1994.1080p.BluRay.x264</title>
    <guid>https://example.invalid/1</guid>
    <size>2000000000</size>
    <enclosure url="magnet:?xt=urn:btih:E0D00667650ABA9EE05AACBBBD8B55EA8A51F534&amp;dn=x" type="application/x-bittorrent"/>
    <torznab:attr name="seeders" value="55"/>
    <torznab:attr name="infohash" value="e0d00667650aba9ee05aacbbbd8b55ea8a51f534"/>
  </item>
  <item>
    <title>Bad no hash item</title>
    <link>https://example.invalid/file.torrent</link>
  </item>
</channel>
</rss>`

func TestTorznabSearchParsesRSS(t *testing.T) {
	var gotQuery string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotQuery = r.URL.RawQuery
		w.Header().Set("Content-Type", "application/xml")
		fmt.Fprint(w, torznabFixture)
	}))
	defer srv.Close()

	ix := Indexer{Kind: "torznab", Name: "prowlarr", URL: srv.URL + "/1/api", APIKey: "abc123"}
	res, err := torznabSearch(context.Background(), srv.Client(), ix, "shawshank 1994", "movie", 0, 0, "tt0111161")
	if err != nil {
		t.Fatal(err)
	}
	if len(res) != 1 {
		t.Fatalf("want 1 result (hashless item dropped), got %d", len(res))
	}
	if res[0].InfoHash != "e0d00667650aba9ee05aacbbbd8b55ea8a51f534" {
		t.Errorf("infohash = %q", res[0].InfoHash)
	}
	if res[0].Seeders != 55 || res[0].Size != 2000000000 {
		t.Errorf("seeders/size = %d/%d", res[0].Seeders, res[0].Size)
	}
	for _, want := range []string{"apikey=abc123", "t=movie", "imdbid=0111161", "q=shawshank"} {
		if !strings.Contains(gotQuery, want) {
			t.Errorf("request query %q missing %q", gotQuery, want)
		}
	}
}

const knabenFixture = `{"hits":[
 {"title":"The.Shawshank.Redemption.1994.1080p.BluRay","hash":"E0D00667650ABA9EE05AACBBBD8B55EA8A51F534",
  "bytes":1722009470,"liveSeeders":488,"category":"Movies / HD",
  "magnetUrl":"magnet:?xt=urn:btih:E0D00667650ABA9EE05AACBBBD8B55EA8A51F534&dn=a&tr=udp%3A%2F%2Ftracker.one%3A1337%2Fannounce&tr=udp%3A%2F%2Ftracker.two%3A80%2Fannounce"},
 {"title":"Some Album","hash":"1111111111111111111111111111111111111111","bytes":10,"liveSeeders":5,"category":"Audio"},
 {"title":"bad","hash":"tooshort","bytes":1,"liveSeeders":1,"category":"Movies"}
]}`

func TestKnabenSearchParsesJSON(t *testing.T) {
	var body map[string]any
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		json.NewDecoder(r.Body).Decode(&body)
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, knabenFixture)
	}))
	defer srv.Close()

	ix := Indexer{Kind: "knaben", Name: "knaben", URL: srv.URL}
	res, err := knabenSearch(context.Background(), srv.Client(), ix, "shawshank")
	if err != nil {
		t.Fatal(err)
	}
	if len(res) != 1 {
		t.Fatalf("want 1 usable result, got %d", len(res))
	}
	if res[0].Seeders != 488 {
		t.Errorf("seeders = %d", res[0].Seeders)
	}
	if len(res[0].Trackers) != 2 {
		t.Errorf("trackers = %v", res[0].Trackers)
	}
	if body["search_type"] != "100%" {
		t.Errorf("search_type = %v (knaben needs the literal 100%%)", body["search_type"])
	}
	if cats, ok := body["categories"].([]any); !ok || len(cats) != 2 {
		t.Errorf("categories = %v", body["categories"])
	}
}

func TestRankingPrefersQualityAndHealth(t *testing.T) {
	// Same swarm: the better-looking release wins.
	even := []Result{
		{Title: "Movie 2020 1080p BluRay x264", Seeders: 60, Size: 8 << 30},
		{Title: "Movie 2020 2160p BluRay HEVC", Seeders: 60, Size: 20 << 30},
	}
	for i := range even {
		even[i].Score = scoreResult(even[i], "movie", 0, 0)
	}
	sortResults(even)
	if !strings.Contains(even[0].Title, "2160p") {
		t.Fatalf("with healthy swarms the 2160p should win, got %q", even[0].Title)
	}

	// Thin swarm: a 20 GiB 2160p at 10 seeds cannot keep up, so the 1080p wins.
	mixed := []Result{
		{Title: "Movie 2020 2160p BluRay HEVC", Seeders: 10, Size: 20 << 30},
		{Title: "Movie 2020 1080p BluRay x264", Seeders: 40, Size: 6 << 30},
		{Title: "Movie 2020 CAM x264", Seeders: 900, Size: 1 << 30},
	}
	for i := range mixed {
		mixed[i].Score = scoreResult(mixed[i], "movie", 0, 0)
	}
	sortResults(mixed)
	if strings.Contains(mixed[0].Title, "2160p") {
		t.Fatalf("thin-swarm 2160p should not rank first: %q", mixed[0].Title)
	}
	if !strings.Contains(mixed[len(mixed)-1].Title, "CAM") {
		t.Fatalf("cam should rank last, got %q", mixed[len(mixed)-1].Title)
	}
}

func TestThroughputPenalty(t *testing.T) {
	// A 12 GiB episode needs ~33 Mbps; six seeds cannot carry that.
	big := Result{Title: "Show S01E01 2160p WEB-DL", Seeders: 6, Size: 12 << 30}
	small := Result{Title: "Show S01E01 1080p WEB-DL", Seeders: 6, Size: 2 << 30}
	bs := scoreResult(big, "series", 1, 1)
	ss := scoreResult(small, "series", 1, 1)
	if bs >= ss {
		t.Fatalf("big/thin release should score below the small one: %.0f vs %.0f", bs, ss)
	}
}

func TestEpisodeMatching(t *testing.T) {
	right := Result{Title: "Show S01E02 1080p WEB-DL", Seeders: 10, Size: 2 << 30}
	wrong := Result{Title: "Show S01E05 1080p WEB-DL", Seeders: 10, Size: 2 << 30}
	pack := Result{Title: "Show S01 Complete 1080p", Seeders: 10, Size: 20 << 30}
	rs, ws, ps := scoreResult(right, "series", 1, 2), scoreResult(wrong, "series", 1, 2), scoreResult(pack, "series", 1, 2)
	if rs <= ws || rs <= ps {
		t.Fatalf("right episode should outrank: right=%.0f wrong=%.0f pack=%.0f", rs, ws, ps)
	}
}

func TestChooseFile(t *testing.T) {
	paths := []string{"Movie/sample.mkv", "Movie/movie.mkv", "Movie/readme.txt"}
	sizes := []int64{30 << 20, 4 << 30, 2 << 10}
	idx, err := chooseFile(paths, sizes, -1)
	if err != nil || idx != 1 {
		t.Fatalf("chooseFile = %d, %v (want 1)", idx, err)
	}
	if idx, _ := chooseFile(paths, sizes, 2); idx != 2 {
		t.Fatalf("explicit index ignored: %d", idx)
	}
	if _, err := chooseFile(nil, nil, 0); err == nil {
		t.Fatal("expected error on empty file list")
	}
	idx, _ = chooseFile([]string{"a.bin", "b.txt"}, []int64{10, 99}, -1)
	if idx != 1 {
		t.Fatalf("fallback should pick largest file, got %d", idx)
	}
}

func TestLooseQuery(t *testing.T) {
	if got := looseQuery("Show Name S02E05 2019"); got != "Show Name" {
		t.Fatalf("looseQuery = %q", got)
	}
}

func TestMagnetURIHasTrackers(t *testing.T) {
	m := magnetURI("e0d00667650aba9ee05aacbbbd8b55ea8a51f534", "Some Release", []string{"udp://t:1/a"})
	if !strings.Contains(m, "xt=urn:btih:e0d00667650aba9ee05aacbbbd8b55ea8a51f534") ||
		!strings.Contains(m, "tr=udp%3A%2F%2Ft%3A1%2Fa") {
		t.Fatalf("bad magnet: %s", m)
	}
}

func TestHumanBytes(t *testing.T) {
	if got := humanBytes(1722009470); got != "1.6 GiB" {
		t.Fatalf("humanBytes = %q", got)
	}
}

func TestSniffContainer(t *testing.T) {
	mp4 := []byte("\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isom")
	if ext, mt := sniffContainer(mp4); ext != ".mp4" || mt != "video/mp4" {
		t.Errorf("mp4: %q %q", ext, mt)
	}
	mkv := []byte("\x1a\x45\xdf\xa3\xa3\x42\x86\x81\x01\x42\xf7\x81\x01")
	if ext, mt := sniffContainer(mkv); ext != ".mkv" || mt != "video/x-matroska" {
		t.Errorf("mkv: %q %q", ext, mt)
	}
	if ext, _ := sniffContainer([]byte("RIFF\x00\x00\x00\x00AVI LIST")); ext != ".avi" {
		t.Errorf("avi: %q", ext)
	}
	if ext, _ := sniffContainer([]byte("nonsense bytes")); ext != "" {
		t.Errorf("unknown should be empty, got %q", ext)
	}
}

func TestDropDeadReleases(t *testing.T) {
	in := []Result{
		{Title: "alive", Seeders: 12},
		{Title: "dead", Seeders: 0},
		{Title: "alive2", Seeders: 1},
	}
	out := dropDead(in)
	if len(out) != 2 || out[0].Title != "alive" || out[1].Title != "alive2" {
		t.Fatalf("dropDead = %+v", out)
	}
	// If everything is dead, return them rather than an empty list.
	all := dropDead([]Result{{Title: "dead", Seeders: 0}})
	if len(all) != 1 {
		t.Fatalf("should keep the only row: %+v", all)
	}
}

// testServerXML serves a fixed body and hands back a server the caller can hit.
func testServerXML(t *testing.T, body string) *httptest.Server {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/rss+xml")
		fmt.Fprint(w, body)
	}))
	t.Cleanup(srv.Close)
	return srv
}
