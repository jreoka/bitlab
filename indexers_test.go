package main

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
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

const eztvFixture = `{"torrents_count":4,"torrents":[
 {"hash":"D03D2022E4ED79545C4B8D70DFC3900C4780FC47","filename":"Show.S01E10.1080p.WEB.H264.mkv",
  "title":"Show S01E10 1080p WEB H264-KBBQ EZTV","magnet_url":"magnet:?xt=urn:btih:d03d2022e4ed79545c4b8d70dfc3900c4780fc47&dn=x&tr=udp%3A%2F%2Ft.one%3A1337%2Fannounce",
  "season":"1","episode":"10","seeds":4,"peers":3,"size_bytes":"3223111412"},
 {"hash":"1111111111111111111111111111111111111111","title":"Show S02E01 1080p WEB","season":"2","episode":"1","seeds":9,"size_bytes":"1000"},
 {"hash":"2222222222222222222222222222222222222222","title":"Show S01 1080p WEB H264-PACK","season":"1","episode":"0","seeds":7,"size_bytes":7000},
 {"hash":"3333333333333333333333333333333333333333","title":"Show S01E09 1080p WEB","season":"1","episode":"9","seeds":2,"size_bytes":"500"}
]}`

func TestEZTVSearchFiltersSeasonAndEpisode(t *testing.T) {
	var gotURL string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotURL = r.URL.String()
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, eztvFixture)
	}))
	defer srv.Close()

	ix := Indexer{Kind: "eztv", Name: "eztv", URL: srv.URL}
	res, err := eztvSearch(context.Background(), srv.Client(), ix, "series", 1, 10, "tt0903747")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(gotURL, "imdb_id=0903747") {
		t.Errorf("imdb id should be numeric, got %q", gotURL)
	}
	// S01E10 plus the S01 pack; the other season and the wrong episode go.
	if len(res) != 2 {
		t.Fatalf("want 2 results, got %d: %+v", len(res), res)
	}
	if res[0].InfoHash != "d03d2022e4ed79545c4b8d70dfc3900c4780fc47" || res[0].Seeders != 4 || res[0].Size != 3223111412 {
		t.Errorf("first result = %+v", res[0])
	}
	if len(res[0].Trackers) != 1 {
		t.Errorf("magnet trackers not kept: %+v", res[0].Trackers)
	}
	if res[1].Seeders != 7 {
		t.Errorf("season pack should be kept: %+v", res[1])
	}
	// EZTV has no text search, so a movie must not even hit the network.
	if r, err := eztvSearch(context.Background(), srv.Client(), ix, "movie", 0, 0, "tt0903747"); err != nil || r != nil {
		t.Errorf("movie should be skipped: %v %v", r, err)
	}
	if r, _ := eztvSearch(context.Background(), srv.Client(), ix, "series", 1, 1, ""); r != nil {
		t.Errorf("no imdb id should be skipped, got %+v", r)
	}
}

const ytsFixture = `{"status":"ok","data":{"movie_count":2,"movies":[
 {"title":"Dune: Part Two","year":2024,"imdb_code":"tt15239678","torrents":[
   {"hash":"82322840619ECE8BB1722ACFD815828B93F76557","quality":"720p","type":"bluray","video_codec":"x264","seeds":100,"peers":38,"size_bytes":1599875318},
   {"hash":"C2F364BA7F59B22A1845B16FD8A1FC1783568F8B","quality":"1080p","type":"web","video_codec":"x265","seeds":42,"peers":40,"size_bytes":3285649981}]},
 {"title":"Dune","year":2021,"imdb_code":"tt1160419","torrents":[
   {"hash":"4444444444444444444444444444444444444444","quality":"1080p","type":"bluray","seeds":3,"size_bytes":2000}]}
]}}`

func TestYTSSearchExpandsTorrents(t *testing.T) {
	var gotURL string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotURL = r.URL.String()
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, ytsFixture)
	}))
	defer srv.Close()

	ix := Indexer{Kind: "yts", Name: "yts", URL: srv.URL}
	res, err := ytsSearch(context.Background(), srv.Client(), ix, "movie", "tt15239678", "dune part two 2024")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(gotURL, "query_term=tt15239678") {
		t.Errorf("should search by imdb id, got %q", gotURL)
	}
	// Two rows for Dune Part Two; the loose Dune (2021) match is dropped.
	if len(res) != 2 {
		t.Fatalf("want 2 results, got %d: %+v", len(res), res)
	}
	if !strings.Contains(res[0].Title, "1080p") && !strings.Contains(res[1].Title, "1080p") {
		t.Errorf("titles should carry quality: %+v", res)
	}
	if res[0].Seeders != 100 || res[0].Size != 1599875318 {
		t.Errorf("first row = %+v", res[0])
	}
	if r, _ := ytsSearch(context.Background(), srv.Client(), ix, "series", "tt15239678", ""); r != nil {
		t.Errorf("YTS has no TV, expected no results, got %+v", r)
	}
}

const toshoFixture = `[
 {"title":"[Group] Frieren - 04 [1080p]","torrent_name":"[Group] Frieren - 04 [1080p]","info_hash":"2FD2133F28FBD29021B775C17C8E1CF5ACEB9A15",
  "magnet_uri":"magnet:?xt=urn:btih:F7JBGPZI7PJJAINXOXAXZDQ46WWOXGQV&tr=udp%3A%2F%2Fnyaa.tracker.wf%3A7777%2Fannounce","seeders":31,"total_size":1234567890},
 {"title":"no hash","torrent_name":"no hash","info_hash":"","seeders":5,"total_size":10}
]`

func TestAnimeToshoSearchParsesFeed(t *testing.T) {
	var gotURL string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotURL = r.URL.String()
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, toshoFixture)
	}))
	defer srv.Close()

	ix := Indexer{Kind: "animetosho", Name: "animetosho", URL: srv.URL}
	res, err := animeToshoSearch(context.Background(), srv.Client(), ix, "Frieren")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(gotURL, "q=Frieren") {
		t.Errorf("query not passed: %q", gotURL)
	}
	if len(res) != 1 {
		t.Fatalf("want 1 result (hashless dropped), got %d", len(res))
	}
	if res[0].InfoHash != "2fd2133f28fbd29021b775c17c8e1cf5aceb9a15" || res[0].Seeders != 31 || res[0].Size != 1234567890 {
		t.Errorf("result = %+v", res[0])
	}
	if len(res[0].Trackers) != 1 {
		t.Errorf("trackers = %+v", res[0].Trackers)
	}
}

func TestQueriesForCapsFanOut(t *testing.T) {
	q := []string{"a", "b", "c", "d"}
	if got := queriesFor("eztv", q); len(got) != 1 {
		t.Errorf("eztv = %v", got)
	}
	if got := queriesFor("yts", q); len(got) != 1 {
		t.Errorf("yts = %v", got)
	}
	if got := queriesFor("animetosho", q); len(got) != 3 {
		t.Errorf("animetosho = %v", got)
	}
}

func TestToshoRelevanceGuard(t *testing.T) {
	cases := []struct {
		query, title string
		want         bool
	}{
		{"Ran 1985", "Kimagure Orange Road Pilot (1985)v2.mkv", false},
		{"Ran 1985", "Ran.1985.1080p.BluRay.x264", true},
		{"Frieren: Beyond Journey's End", "[9volt] Sousou no Frieren - 04 [1080p]", true},
		{"Frieren S01E04", "[Group] Frieren - 04 [1080p]", true},
		{"The Office 2005", "Urashima Taro [Orange-Flash]", false},
		{"Air 2005", "Air - 01 [1080p]", true},
	}
	for _, c := range cases {
		if got := toshoRelevant(c.query, c.title); got != c.want {
			t.Errorf("toshoRelevant(%q, %q) = %v, want %v", c.query, c.title, got, c.want)
		}
	}
	if got := toshoTitleQuery("Frieren S01E04 2024"); got != "Frieren" {
		t.Errorf("toshoTitleQuery = %q", got)
	}
}

func TestEstimatedMbpsPlateaus(t *testing.T) {
	cases := []struct {
		seeders int
		want    float64
	}{
		{0, 0}, {1, 5}, {4, 8}, {60, 25.2}, {100, 32}, {442, 40}, {5000, 40},
	}
	for _, c := range cases {
		got := estimatedMbps(c.seeders)
		if math.Abs(got-c.want) > 0.05 {
			t.Errorf("estimatedMbps(%d) = %.2f, want %.2f", c.seeders, got, c.want)
		}
	}
}

func TestScoreDemotesUnplayableRemux(t *testing.T) {
	// 64 GiB over a two-hour feature needs ~76 Mbps; the same film as a 29 GiB
	// WEB needs ~25. Both fit in the quality bonuses, so playability decides.
	remux := Result{Title: "Dune Part Two 2024 2160p REMUX HDR H264", Size: 64 << 30, Seeders: 300}
	web := Result{Title: "Dune Part Two 2024 2160p WEB-DL HDR H264", Size: 29 << 30, Seeders: 300}
	if scoreResult(remux, "movie", 0, 0) >= scoreResult(web, "movie", 0, 0) {
		t.Fatalf("remux %.0f should rank below web %.0f", scoreResult(remux, "movie", 0, 0), scoreResult(web, "movie", 0, 0))
	}
	// A modest release on a small swarm must still beat a giant one on a big swarm.
	small := Result{Title: "Movie 2024 1080p WEB-DL H264", Size: 3 << 30, Seeders: 8}
	huge := Result{Title: "Movie 2024 1080p BluRay REMUX H264", Size: 60 << 30, Seeders: 8}
	if scoreResult(small, "movie", 0, 0) <= scoreResult(huge, "movie", 0, 0) {
		t.Fatalf("playable release should win: small %.0f huge %.0f", scoreResult(small, "movie", 0, 0), scoreResult(huge, "movie", 0, 0))
	}
	// A 1080p WEB episode on a healthy swarm keeps its place above a 4K remux.
	ep := Result{Title: "Show S01E01 1080p WEB-DL H264", Size: 2 << 30, Seeders: 400}
	ep4k := Result{Title: "Show S01E01 2160p REMUX H265", Size: 45 << 30, Seeders: 400}
	if scoreResult(ep, "series", 1, 1) <= scoreResult(ep4k, "series", 1, 1) {
		t.Fatalf("1080p episode should beat an unstreamable remux: %.0f vs %.0f", scoreResult(ep, "series", 1, 1), scoreResult(ep4k, "series", 1, 1))
	}
}

func TestIndexerTimeoutShortensSlowSources(t *testing.T) {
	if got := indexerTimeout("animetosho"); got != 6*time.Second {
		t.Errorf("animetosho timeout = %v", got)
	}
	if got := indexerTimeout("knaben"); got != 15*time.Second {
		t.Errorf("default timeout = %v", got)
	}
}

const torrentioFixture = `{"streams":[
 {"name":"Torrentio\n1080p",
  "title":"The.Wire.S02.1080p.BluRay.x265-RARBG\nThe.Wire.S02E01.1080p.BluRay.x265-RARBG.mp4\n👤 408 💾 935.67 MB ⚙️ RARBG",
  "infoHash":"C1607D2B2E9BD164638D453C169C727F6368289C","fileIdx":25,
  "behaviorHints":{"filename":"The.Wire.S02E01.1080p.BluRay.x265-RARBG.mp4"},
  "sources":["tracker:udp://tracker.opentrackr.org:1337/announce","dht:c1607d2b2e9bd164638d453c169c727f6368289c"]},
 {"name":"Torrentio\n1080p",
  "title":"Прослушка.Сезон.2.1080p.mkv\n👤 45 💾 9.1 GB ⚙️ Rutracker",
  "infoHash":"3414a47523f5ae2f08f5fff5413b66790bf13dfa","sources":[]},
 {"name":"Torrentio\n4k",
  "title":"Матрица / The Matrix [1999 WEB-DL HDR10]\nМатрица.1999.WEB-DL.2160p.mkv\n👤 448 💾 18.45 GB ⚙️ Rutracker",
  "infoHash":"61065ea115b7cc3e8db9fb5ab1f6f327f08bd1c9","sources":[]},
 {"name":"Torrentio\n1080p","title":"no hash here\n👤 10 💾 1 GB ⚙️ 1337x","infoHash":"","sources":[]}
]}`

func TestTorrentioSearchParsesStreams(t *testing.T) {
	var gotURL string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotURL = r.URL.Path
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, torrentioFixture)
	}))
	defer srv.Close()

	ix := Indexer{Kind: "torrentio", Name: "torrentio", URL: srv.URL}
	res, err := torrentioSearch(context.Background(), srv.Client(), ix, "series", 2, 1, "tt0306414")
	if err != nil {
		t.Fatal(err)
	}
	if gotURL != "/stream/series/tt0306414:2:1.json" {
		t.Errorf("requested %q", gotURL)
	}
	if len(res) != 2 {
		t.Fatalf("want 2 results (Cyrillic-only and hashless dropped), got %d: %+v", len(res), res)
	}
	if res[0].Seeders != 408 || res[0].Indexer != "torrentio/RARBG" || res[0].Size != parseSize("935.67 MB") {
		t.Errorf("first = %+v", res[0])
	}
	if len(res[0].Trackers) != 1 || res[0].Trackers[0] != "udp://tracker.opentrackr.org:1337/announce" {
		t.Errorf("trackers = %v", res[0].Trackers)
	}
	if res[1].Seeders != 448 || res[1].Size != parseSize("18.45 GB") {
		t.Errorf("second = %+v", res[1])
	}
	// The release name has no resolution token of its own; the stream name's
	// "4k" has to be appended or scoreResult reads a 4K rip as SD.
	if strings.Contains(res[0].Title, "1080p 1080p") {
		t.Errorf("quality duplicated: %q", res[0].Title)
	}
	if !strings.Contains(res[1].Title, "4k") {
		t.Errorf("quality not carried over: %q", res[1].Title)
	}
	if !strings.Contains(res[1].Title, "The Matrix") {
		t.Errorf("title = %q", res[1].Title)
	}
}

func TestTorrentioAlsoQueriesMovies(t *testing.T) {
	var gotURL string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotURL = r.URL.Path
		fmt.Fprint(w, `{"streams":[]}`)
	}))
	defer srv.Close()
	ix := Indexer{Kind: "torrentio", Name: "torrentio", URL: srv.URL}
	if _, err := torrentioSearch(context.Background(), srv.Client(), ix, "movie", 0, 0, "tt0133093"); err != nil {
		t.Fatal(err)
	}
	if gotURL != "/stream/movie/tt0133093.json" {
		t.Errorf("requested %q", gotURL)
	}
	// A series request without a season has nothing to look up.
	if res, err := torrentioSearch(context.Background(), srv.Client(), ix, "series", 0, 0, "tt0133093"); err != nil || len(res) != 0 {
		t.Errorf("seasonless series = %v, %v", res, err)
	}
}

const apibayFixture = `[
 {"id":"7349687","name":"The Matrix (1999) 1080p BrRip x264 - YIFY","info_hash":"D7A46713EAEE18C746B3254B7D1492A50FD9D6CE","seeders":"852","leechers":"131","size":"1992277407","category":"207"},
 {"id":"81783075","name":"The Matrix 1999 CAM","info_hash":"1111111111111111111111111111111111111111","seeders":"3","leechers":"1","size":"800000000","category":"505"},
 {"id":"0","name":"No results returned","info_hash":"0000000000000000000000000000000000000000"}
]`

func TestApibaySearchReadsSeeders(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, apibayFixture)
	}))
	defer srv.Close()
	ix := Indexer{Kind: "apibay", Name: "apibay", URL: srv.URL}
	res, err := apibaySearch(context.Background(), srv.Client(), ix, "The Matrix 1999")
	if err != nil {
		t.Fatal(err)
	}
	// apibay sends "seeders", not "num_seeders": reading the wrong key left every
	// row at zero and dropDead then discarded the source entirely.
	if len(res) != 1 {
		t.Fatalf("want 1 result (non-Video category dropped), got %d: %+v", len(res), res)
	}
	if res[0].Seeders != 852 || res[0].Size != 1992277407 {
		t.Errorf("result = %+v", res[0])
	}
}

// Re:Zero S01E04 pulled up the airing season's episode twice: "… 4th Season -
// 04" is an absolute number that reads as episode 4. A stated season must win.
func TestEpisodeSignalRejectsAnotherSeasonAndRecuts(t *testing.T) {
	cases := []struct {
		title string
		want  int
	}{
		{"[Erai-raws] Re Zero kara Hajimeru Isekai Seikatsu 4th Season - 04 [1080p]", -1},
		{"[Erai-raws] Re Zero kara Hajimeru Isekai Seikatsu 3rd Season - 04 [1080p]", -1},
		{"[Judas] Re.Zero kara Hajimeru Isekai Seikatsu (Season 2 Part 2) [1080p]", -1},
		{"Re.Zero.Starting.Life.in.Another.World.S01.1080p.BluRay.x264", 0},
		{"[SubsPlease] Re Zero kara Hajimeru Isekai Seikatsu - 04 (1080p)", 1},
		{"Re ZERO -Starting Life in Another World- Director's Cut S01E04 MULTi 1080p", -1},
		{"[Antrophiss] Kimetsu no Yaiba The Movie - Infinity Castle Part 1 1080p HEVC 10bit (WEB-DL)", -1},
		{"[224] Re.Zero kara Hajimeru Isekai Seikatsu - Shin Henshuu-ban [BDRip 1080p]", -1},
	}
	for _, c := range cases {
		if got := episodeSignal(c.title, 1, 4); got != c.want {
			t.Errorf("episodeSignal(%q, 1, 4) = %d, want %d", c.title, got, c.want)
		}
	}
	// The same release is the right answer for its own season.
	if got := episodeSignal("[Erai-raws] Re Zero kara Hajimeru Isekai Seikatsu 4th Season - 04 [1080p]", 4, 4); got != 1 {
		t.Errorf("season four's own episode judged %d, want 1", got)
	}
}
