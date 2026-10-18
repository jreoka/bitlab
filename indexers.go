package main

import (
	"context"
	"encoding/json"
	"encoding/xml"
	"fmt"
	"math"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Result is one candidate release discovered by an indexer.
type Result struct {
	Title    string
	InfoHash string
	Size     int64
	Seeders  int
	Indexer  string
	Magnet   string
	Score    float64
	Trackers []string
}

var ihRe = regexp.MustCompile(`(?i)\burn:btih:([a-z0-9]{32,40})\b`)

var videoExts = map[string]bool{
	".mp4": true, ".mkv": true, ".avi": true, ".mov": true, ".m4v": true,
	".webm": true, ".ts": true, ".wmv": true, ".mpg": true, ".mpeg": true, ".flv": true,
}

// searchAll fans out across every enabled indexer and merges the results.
func (s *Server) searchAll(ctx context.Context, queries []string, typ string, season, ep int, imdbID string) ([]Result, []string) {
	if len(queries) == 0 {
		return nil, []string{"empty query"}
	}
	if len(queries) > 6 {
		queries = queries[:6]
	}
	indexers, err := s.store.ListIndexers()
	if err != nil {
		return nil, []string{err.Error()}
	}
	var enabled []Indexer
	for _, ix := range indexers {
		if ix.Enabled {
			enabled = append(enabled, ix)
		}
	}
	if len(enabled) == 0 {
		return nil, []string{"no indexers configured"}
	}

	type out struct {
		res  []Result
		err  error
		name string
	}
	// Every query runs against every indexer: anime needs all of its names
	// (romaji, English, synonyms) because release titles use any of them.
	// Small indexers rate-limit hard (nyaa answers 429), so they get fewer.
	ch := make(chan out, len(enabled)*len(queries))
	var wg sync.WaitGroup
	for _, ix := range enabled {
		for _, q := range queriesFor(ix.Kind, queries) {
			wg.Add(1)
			go func(ix Indexer, q string) {
				defer wg.Done()
				c, cancel := context.WithTimeout(ctx, 15*time.Second)
				defer cancel()
				r, err := s.searchIndexer(c, ix, q, typ, season, ep, imdbID)
				ch <- out{res: r, err: err, name: ix.Name}
			}(ix, q)
		}
	}
	wg.Wait()
	close(ch)

	var all []Result
	var errs []string
	seen := map[string]bool{}
	for o := range ch {
		if o.err != nil {
			errs = append(errs, o.name+": "+o.err.Error())
			continue
		}
		for _, r := range o.res {
			if r.InfoHash == "" || seen[r.InfoHash] {
				if seen[r.InfoHash] {
					continue
				}
				continue
			}
			seen[r.InfoHash] = true
			all = append(all, r)
		}
	}
	for i := range all {
		all[i].Score = scoreResult(all[i], typ, season, ep)
	}
	// If we know which episode we want and some release matches it, drop the
	// ones that are clearly a different episode or a season pack. Anime naming
	// ("Show - 04", "Show S01E04") varies, so this is judged per title.
	if typ == "series" && season > 0 {
		keep := make([]Result, 0, len(all))
		for _, r := range all {
			if episodeSignal(r.Title, season, ep) < 0 {
				continue // a different episode, a different season, or a pack
			}
			keep = append(keep, r)
		}
		all = keep
	}
	sortResults(all)
	all = dropDead(all)
	if len(all) > 40 {
		all = all[:40]
	}
	return all, errs
}

// dropDead removes releases with no seeders — they cannot play — unless that
// would leave nothing at all.
func dropDead(rs []Result) []Result {
	live := make([]Result, 0, len(rs))
	for _, r := range rs {
		if r.Seeders > 0 {
			live = append(live, r)
		}
	}
	if len(live) == 0 {
		return rs
	}
	return live
}

// queriesFor trims the fan-out for indexers that throttle.
func queriesFor(kind string, queries []string) []string {
	switch kind {
	case "eztv", "yts":
		// Both answer an imdb id, so every extra query would be the same
		// request again. Anime aliases buy nothing here either.
		if len(queries) > 1 {
			return queries[:1]
		}
	case "nyaa", "apibay", "animetosho":
		if len(queries) > 3 {
			return queries[:3]
		}
	}
	return queries
}

type ixCacheEntry struct {
	res []Result
	exp time.Time
}

// searchIndexer caches each (indexer, query) briefly. Paging through episodes
// repeats most queries, and small indexers answer 429 when hammered.
func (s *Server) searchIndexer(ctx context.Context, ix Indexer, q string, typ string, season, ep int, imdbID string) ([]Result, error) {
	ck := ix.Kind + "|" + ix.URL + "|" + typ + "|" + strconv.Itoa(season) + "|" + strconv.Itoa(ep) + "|" + strings.ToLower(q)
	if v, ok := s.ixCache.Load(ck); ok {
		e := v.(ixCacheEntry)
		if time.Now().Before(e.exp) {
			return e.res, nil
		}
		s.ixCache.Delete(ck)
	}
	res, err := s.searchIndexerUncached(ctx, ix, q, typ, season, ep, imdbID)
	if err == nil && len(res) > 0 {
		s.ixCache.Store(ck, ixCacheEntry{res: res, exp: time.Now().Add(3 * time.Minute)})
	}
	return res, err
}

func (s *Server) searchIndexerUncached(ctx context.Context, ix Indexer, q string, typ string, season, ep int, imdbID string) ([]Result, error) {
	switch ix.Kind {
	case "nyaa":
		return nyaaSearch(ctx, s.http, ix, q)
	case "knaben":
		return knabenSearch(ctx, s.http, ix, q)
	case "apibay":
		return apibaySearch(ctx, s.http, ix, q)
	case "eztv":
		return eztvSearch(ctx, s.http, ix, typ, season, ep, imdbID)
	case "yts":
		return ytsSearch(ctx, s.http, ix, typ, imdbID, q)
	case "animetosho":
		return animeToshoSearch(ctx, s.http, ix, q)
	case "torznab", "":
		return torznabSearch(ctx, s.http, ix, q, typ, season, ep, imdbID)
	default:
		return nil, fmt.Errorf("unknown indexer kind %q", ix.Kind)
	}
}

// ---------- Torznab (Prowlarr / Jackett / nZEDb) ----------

type torznabItem struct {
	Title     string `xml:"title"`
	Link      string `xml:"link"`
	GUID      string `xml:"guid"`
	Size      int64  `xml:"size"`
	Enclosure struct {
		URL  string `xml:"url,attr"`
		Type string `xml:"type,attr"`
	} `xml:"enclosure"`
	Attrs []struct {
		Name  string `xml:"name,attr"`
		Value string `xml:"value,attr"`
	} `xml:"attr"`
}

type torznabFeed struct {
	Channel struct {
		Items []torznabItem `xml:"item"`
	} `xml:"channel"`
}

func torznabSearch(ctx context.Context, hc *http.Client, ix Indexer, q string, typ string, season, ep int, imdbID string) ([]Result, error) {
	base := strings.TrimRight(ix.URL, "/")
	if !strings.HasSuffix(base, "/api") {
		base += "/api"
	}
	u, err := url.Parse(base)
	if err != nil {
		return nil, err
	}
	v := u.Query()
	if ix.APIKey != "" {
		v.Set("apikey", ix.APIKey)
	}
	v.Set("limit", "100")
	v.Set("extended", "1")
	switch typ {
	case "series":
		v.Set("t", "tvsearch")
		v.Set("q", q)
		if season > 0 {
			v.Set("season", strconv.Itoa(season))
			v.Set("ep", strconv.Itoa(ep))
		}
	case "movie":
		v.Set("t", "movie")
		v.Set("q", q)
		if imdbID != "" {
			v.Set("imdbid", strings.TrimPrefix(imdbID, "tt"))
		}
	default:
		v.Set("t", "search")
		v.Set("q", q)
	}
	u.RawQuery = v.Encode()

	req, err := http.NewRequestWithContext(ctx, "GET", u.String(), nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "marlin/1.0")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	body, err := readAllLimit(resp.Body, 8<<20)
	if err != nil {
		return nil, err
	}
	var feed torznabFeed
	if err := xml.Unmarshal(body, &feed); err != nil {
		return nil, fmt.Errorf("bad xml: %w", err)
	}
	var out []Result
	for _, it := range feed.Channel.Items {
		r := Result{Title: strings.TrimSpace(it.Title), Indexer: ix.Name, Size: it.Size}
		for _, a := range it.Attrs {
			switch strings.ToLower(a.Name) {
			case "infohash":
				r.InfoHash = strings.ToLower(strings.TrimSpace(a.Value))
			case "seeders":
				r.Seeders, _ = strconv.Atoi(a.Value)
			case "size":
				if n, err := strconv.ParseInt(a.Value, 10, 64); err == nil {
					r.Size = n
				}
			case "magneturl":
				r.Magnet = a.Value
			}
		}
		if r.InfoHash == "" {
			for _, cand := range []string{it.Enclosure.URL, it.Link, it.GUID, r.Magnet} {
				if m := ihRe.FindStringSubmatch(cand); m != nil {
					r.InfoHash = strings.ToLower(m[1])
					break
				}
			}
		}
		if r.InfoHash == "" || len(r.InfoHash) != 40 {
			continue
		}
		out = append(out, r)
	}
	return out, nil
}

// ---------- apibay (public index, no key) ----------

func apibaySearch(ctx context.Context, hc *http.Client, ix Indexer, q string) ([]Result, error) {
	base := strings.TrimRight(ix.URL, "/")
	if base == "" {
		base = "https://apibay.org"
	}
	u := base + "/q.php?q=" + url.QueryEscape(q) + "&cat=0"
	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) marlin/1.0")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	body, err := readAllLimit(resp.Body, 8<<20)
	if err != nil {
		return nil, err
	}
	var raw []struct {
		ID       string `json:"id"`
		Name     string `json:"name"`
		InfoHash string `json:"info_hash"`
		Seeders  string `json:"num_seeders"`
		Leechers string `json:"num_leechers"`
		Size     string `json:"size"`
	}
	if err := json.Unmarshal(body, &raw); err != nil {
		return nil, fmt.Errorf("bad json: %w", err)
	}
	var out []Result
	for _, r := range raw {
		if r.ID == "0" || r.Name == "No results returned" || len(r.InfoHash) != 40 {
			continue
		}
		seeds, _ := strconv.Atoi(r.Seeders)
		size, _ := strconv.ParseInt(r.Size, 10, 64)
		out = append(out, Result{
			Title:    r.Name,
			InfoHash: strings.ToLower(r.InfoHash),
			Seeders:  seeds,
			Size:     size,
			Indexer:  ix.Name,
		})
	}
	return out, nil
}

// ---------- ranking ----------

var qualityPatterns = []struct {
	re    *regexp.Regexp
	bonus float64
}{
	{regexp.MustCompile(`(?i)\b(2160p|4k|uhd)\b`), 900},
	{regexp.MustCompile(`(?i)\b1080p\b`), 700},
	{regexp.MustCompile(`(?i)\b720p\b`), 400},
	{regexp.MustCompile(`(?i)\b(480p|sd)\b`), 100},
	{regexp.MustCompile(`(?i)\b(bluray|blu-ray|bdrip|web-dl|webrip|remux)\b`), 350},
	{regexp.MustCompile(`(?i)\b(x265|hevc|h\.265)\b`), 120},
	{regexp.MustCompile(`(?i)\b(hdr|dolby.?vision)\b`), 80},
	{regexp.MustCompile(`(?i)\b(cam|hdcam|tsrip|telesync|\bts\b|screener)\b`), -1200},
	{regexp.MustCompile(`(?i)\b(x264|h\.264)\b`), 40},
}

func scoreResult(r Result, typ string, season, ep int) float64 {
	// Swarm health matters, but a 900-seeder CAM must never outrank a healthy
	// 2160p release, so seeders are log-scaled and quality is weighted hard.
	score := math.Log1p(float64(r.Seeders)) * 220
	for _, q := range qualityPatterns {
		if q.re.MatchString(r.Title) {
			score += q.bonus
		}
	}
	// Can this release actually keep up? A 30 Mbps episode on a 6-seed swarm
	// buffers no matter what it looks like, so penalise the shortfall. Roughly
	// 1.5 Mbps per seeder, capped at what the link will do.
	if r.Size > 0 {
		duration := 7200.0 // a feature
		size := r.Size
		if typ == "series" {
			duration = 3000 // ~50 minute episode
			// A batch is one episode's worth of streaming per file, so judge it
			// by a twelfth of its size rather than the whole season.
			if reSeason.MatchString(r.Title) && !reSxxExx.MatchString(r.Title) {
				size /= 12
			}
		}
		needMbps := float64(size) * 8 / duration / 1e6
		haveMbps := math.Min(float64(r.Seeders)*1.5, 60)
		if needMbps > haveMbps {
			score -= (needMbps - haveMbps) * 150
		}
	}
	// A healthy release size is a strong quality signal.
	if r.Size > 0 {
		switch {
		case r.Size > 6<<30 && r.Size < 60<<30:
			score += 200
		case r.Size > 1<<30:
			score += 100
		case r.Size < 100<<20:
			score -= 400 // almost certainly a sample
		}
	}
	if typ == "series" && season > 0 {
		if reSeason.MatchString(r.Title) && !reSxxExx.MatchString(r.Title) {
			score -= 150 // a pack: playable (we pick the episode file), slightly dearer
		}
		tag := fmt.Sprintf("s%02de%02d", season, ep)
		if strings.Contains(strings.ToLower(strings.ReplaceAll(r.Title, " ", "")), tag) {
			score += 400
		}
	}
	return score
}

func sortResults(rs []Result) {
	for i := 1; i < len(rs); i++ {
		for j := i; j > 0 && rs[j].Score > rs[j-1].Score; j-- {
			rs[j], rs[j-1] = rs[j-1], rs[j]
		}
	}
}

// magnetURI builds a magnet link with the configured trackers attached.
func magnetURI(ih, name string, trackers []string) string {
	var b strings.Builder
	b.WriteString("magnet:?xt=urn:btih:" + ih)
	if name != "" {
		b.WriteString("&dn=" + url.QueryEscape(name))
	}
	for _, t := range trackers {
		b.WriteString("&tr=" + url.QueryEscape(t))
	}
	return b.String()
}

// ---------- knaben (public meta-index, no key) ----------

var knabenSkipCategories = map[string]bool{
	"audio": true, "applications": true, "apps": true, "pc": true, "games": true,
	"books": true, "ebook": true, "ebooks": true, "software": true, "android": true,
	"mac": true, "windows": true, "xxx": true, "other": true,
}

func knabenSearch(ctx context.Context, hc *http.Client, ix Indexer, q string) ([]Result, error) {
	res, err := knabenRequest(ctx, hc, ix, q)
	if err == nil && len(res) > 0 {
		return res, nil
	}
	// "100%" requires every token, so a year suffix or "S01E01" can over-constrain.
	// Retry once on the loosest useful form before giving up.
	if alt := looseQuery(q); alt != "" && alt != q {
		if r2, e2 := knabenRequest(ctx, hc, ix, alt); e2 == nil {
			return r2, nil
		}
	}
	return res, err
}

func looseQuery(q string) string {
	q = regexp.MustCompile(`(?i)\bS\d{2}E\d{2}\b`).ReplaceAllString(q, "")
	q = regexp.MustCompile(`\b(19|20)\d{2}\b`).ReplaceAllString(q, "")
	return strings.Join(strings.Fields(q), " ")
}

func knabenRequest(ctx context.Context, hc *http.Client, ix Indexer, q string) ([]Result, error) {
	base := strings.TrimRight(ix.URL, "/")
	if base == "" {
		base = "https://api.knaben.org"
	}
	if !strings.HasSuffix(base, "/v1") {
		base += "/v1"
	}
	payload, _ := json.Marshal(map[string]any{
		"search_type":     "100%",
		"search_field":    "title",
		"query":           q,
		"order_by":        "seeders",
		"order_direction": "desc",
		"categories":      []int{2000000, 5000000}, // Movies, TV
		"from":            0,
		"size":            100,
		"hide_unsafe":     true,
		"hide_xxx":        true,
	})
	req, err := http.NewRequestWithContext(ctx, "POST", base, strings.NewReader(string(payload)))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	body, err := readAllLimit(resp.Body, 8<<20)
	if err != nil {
		return nil, err
	}
	var out struct {
		Hits []struct {
			Title     string `json:"title"`
			Hash      string `json:"hash"`
			Bytes     int64  `json:"bytes"`
			Seeders   int    `json:"liveSeeders"`
			Category  string `json:"category"`
			MagnetURL string `json:"magnetUrl"`
			Tracker   string `json:"tracker"`
		} `json:"hits"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		return nil, fmt.Errorf("bad json: %w", err)
	}
	var res []Result
	for _, h := range out.Hits {
		if len(h.Hash) != 40 || knabenSkipCategories[strings.ToLower(h.Category)] {
			continue
		}
		res = append(res, Result{
			Title:    h.Title,
			InfoHash: strings.ToLower(h.Hash),
			Size:     h.Bytes,
			Seeders:  h.Seeders,
			Indexer:  ix.Name,
			Magnet:   h.MagnetURL,
			Trackers: trackersFromMagnet(h.MagnetURL),
		})
	}
	return res, nil
}

// trackersFromMagnet pulls the announce URLs out of a magnet link.
func trackersFromMagnet(m string) []string {
	u, err := url.Parse(m)
	if err != nil {
		return nil
	}
	return u.Query()["tr"]
}

// ---------- nyaa (anime, no key) ----------

type nyaaItem struct {
	Title    string `xml:"title"`
	Link     string `xml:"link"`
	InfoHash string `xml:"infoHash"`
	Seeders  int    `xml:"seeders"`
	Size     string `xml:"size"`
	Category string `xml:"category"`
	Trusted  string `xml:"trusted"`
}

func nyaaSearch(ctx context.Context, hc *http.Client, ix Indexer, q string) ([]Result, error) {
	base := strings.TrimRight(ix.URL, "/")
	if base == "" {
		base = "https://nyaa.si"
	}
	cat := "1_2" // anime, english-translated
	if strings.HasPrefix(strings.TrimPrefix(ix.URL, base), "/c=") {
		cat = strings.TrimPrefix(strings.TrimPrefix(ix.URL, base), "/c=")
		base = strings.TrimRight(strings.TrimSuffix(base, "/c="+cat), "/")
	}
	u := base + "/?page=rss&q=" + url.QueryEscape(q) + "&c=" + url.QueryEscape(cat) + "&f=0&s=seeders&o=desc"
	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) marlin/1.0")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	body, err := readAllLimit(resp.Body, 8<<20)
	if err != nil {
		return nil, err
	}
	var feed struct {
		Items []nyaaItem `xml:"channel>item"`
	}
	if err := xml.Unmarshal(body, &feed); err != nil {
		return nil, fmt.Errorf("bad xml: %w", err)
	}
	var out []Result
	for _, it := range feed.Items {
		ih := strings.ToLower(strings.TrimSpace(it.InfoHash))
		if len(ih) != 40 {
			continue
		}
		out = append(out, Result{
			Title:    strings.TrimSpace(it.Title),
			InfoHash: ih,
			Size:     parseSize(it.Size),
			Seeders:  it.Seeders,
			Indexer:  ix.Name,
			// nyaa's RSS gives no magnet, and its swarms live on its own tracker.
			Trackers: []string{"http://nyaa.tracker.wf:7777/announce"},
		})
	}
	return out, nil
}

// parseSize turns "1.5 GiB" / "300 MiB" / "1,024 KiB" into bytes.
func parseSize(s string) int64 {
	s = strings.TrimSpace(s)
	fields := strings.Fields(s)
	if len(fields) != 2 {
		return 0
	}
	num, err := strconv.ParseFloat(strings.ReplaceAll(fields[0], ",", ""), 64)
	if err != nil {
		return 0
	}
	mult := float64(1)
	switch strings.ToLower(fields[1]) {
	case "b":
		mult = 1
	case "kib", "kb":
		mult = 1 << 10
	case "mib", "mb":
		mult = 1 << 20
	case "gib", "gb":
		mult = 1 << 30
	case "tib", "tb":
		mult = 1 << 40
	default:
		return 0
	}
	return int64(num * mult)
}

var (
	reSxxExx = regexp.MustCompile(`(?i)\bs(\d{1,2})[\s._-]?e(\d{1,3})\b`)
	reDashEp = regexp.MustCompile(`(?i)(?:^|[\s\[])[-–]\s*0*(\d{1,4})(?:v\d)?\b`)
	reEpOnly = regexp.MustCompile(`(?i)\bep?(\d{1,3})\b`)
	reSeason = regexp.MustCompile(`(?i)\bs(\d{1,2})\b`)
)

// episodeSignal says whether a title is the episode we asked for (+1), a
// different one (-1), or simply unknown (0).
func episodeSignal(title string, season, ep int) int {
	if ep <= 0 {
		return 0
	}
	if m := reSxxExx.FindStringSubmatch(title); m != nil {
		s, _ := strconv.Atoi(m[1])
		e, _ := strconv.Atoi(m[2])
		if season > 0 && s != season {
			return -1
		}
		if e == ep {
			return 1
		}
		return -1
	}
	// A pack for the right season is usable: the play handler picks the episode
	// file out of it. A pack for a different season is not.
	if m := reSeason.FindStringSubmatch(title); m != nil {
		if season > 0 {
			if sn, _ := strconv.Atoi(m[1]); sn != season {
				return -1
			}
		}
		return 0
	}
	if m := reDashEp.FindStringSubmatch(title); m != nil {
		if e, _ := strconv.Atoi(m[1]); e == ep {
			return 1
		}
		return -1
	}
	if m := reEpOnly.FindStringSubmatch(title); m != nil {
		if e, _ := strconv.Atoi(m[1]); e == ep {
			return 1
		}
		return -1
	}
	return 0
}

// ---------- EZTV (TV only, no key, imdb-id lookup) ----------

// flexInt64 accepts both "123" and 123: EZTV quotes size_bytes, and a quoted
// number would make encoding/json reject the whole payload.
type flexInt64 int64

func (f *flexInt64) UnmarshalJSON(b []byte) error {
	s := strings.Trim(strings.TrimSpace(string(b)), `"`)
	if s == "" || s == "null" {
		*f = 0
		return nil
	}
	n, err := strconv.ParseFloat(s, 64)
	if err != nil {
		return err
	}
	*f = flexInt64(n)
	return nil
}

type eztvTorrent struct {
	Hash     string    `json:"hash"`
	Filename string    `json:"filename"`
	Title    string    `json:"title"`
	Magnet   string    `json:"magnet_url"`
	Season   string    `json:"season"`
	Episode  string    `json:"episode"`
	Seeds    int       `json:"seeds"`
	Size     flexInt64 `json:"size_bytes"`
}

// eztvSearch answers with an imdb lookup only. EZTV indexes television
// exclusively and has no text search, so movies and title-only calls return
// nothing instead of paying for a request that cannot match.
func eztvSearch(ctx context.Context, hc *http.Client, ix Indexer, typ string, season, ep int, imdbID string) ([]Result, error) {
	if typ != "series" || imdbID == "" {
		return nil, nil
	}
	base := strings.TrimRight(ix.URL, "/")
	if base == "" {
		base = "https://eztvx.to"
	}
	// EZTV wants the numeric id; a "tt" prefix makes it ignore the filter and
	// answer with the whole "latest torrents" feed.
	u := base + "/api/get-torrents?limit=100&imdb_id=" + url.QueryEscape(strings.TrimPrefix(imdbID, "tt"))
	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) marlin/1.0")
	req.Header.Set("Accept", "application/json")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	body, err := readAllLimit(resp.Body, 8<<20)
	if err != nil {
		return nil, err
	}
	var feed struct {
		Torrents []eztvTorrent `json:"torrents"`
	}
	if err := json.Unmarshal(body, &feed); err != nil {
		return nil, fmt.Errorf("bad json: %w", err)
	}
	var res []Result
	for _, t := range feed.Torrents {
		ih := strings.ToLower(strings.TrimSpace(t.Hash))
		if len(ih) != 40 {
			continue
		}
		s, _ := strconv.Atoi(strings.TrimSpace(t.Season))
		e, _ := strconv.Atoi(strings.TrimSpace(t.Episode))
		if season > 0 {
			if s != season {
				continue
			}
			// Episode "0" is a season pack on EZTV: keep it, the play handler
			// picks the file. Anything else must be the episode we asked for.
			if ep > 0 && e != 0 && e != ep {
				continue
			}
		}
		title := strings.TrimSpace(t.Title)
		if title == "" {
			title = strings.TrimSpace(t.Filename)
		}
		if title == "" {
			continue
		}
		res = append(res, Result{
			Title:    title,
			InfoHash: ih,
			Size:     int64(t.Size),
			Seeders:  t.Seeds,
			Indexer:  ix.Name,
			Magnet:   t.Magnet,
			Trackers: trackersFromMagnet(t.Magnet),
		})
	}
	return res, nil
}

// ---------- YTS (movies only, no key) ----------

type ytsTorrent struct {
	Hash      string `json:"hash"`
	Quality   string `json:"quality"`
	Type      string `json:"type"`
	VideoCode string `json:"video_codec"`
	Seeds     int    `json:"seeds"`
	Peers     int    `json:"peers"`
	SizeBytes int64  `json:"size_bytes"`
}

type ytsMovie struct {
	Title    string       `json:"title"`
	Year     int          `json:"year"`
	IMDBCode string       `json:"imdb_code"`
	Torrents []ytsTorrent `json:"torrents"`
}

// ytsSearch walks the movie's quality/size ladder. YTS has no TV catalogue, and
// its query_term takes an imdb id, which is far more reliable than a title.
func ytsSearch(ctx context.Context, hc *http.Client, ix Indexer, typ string, imdbID, q string) ([]Result, error) {
	if typ != "movie" {
		return nil, nil
	}
	term := strings.TrimSpace(imdbID)
	if term == "" {
		term = q
	}
	if term == "" {
		return nil, nil
	}
	base := strings.TrimRight(ix.URL, "/")
	if base == "" {
		base = "https://yts.lt"
	}
	u := base + "/api/v2/list_movies.json?query_term=" + url.QueryEscape(term) + "&limit=20&sort_by=seeds&order_by=desc"
	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) marlin/1.0")
	req.Header.Set("Accept", "application/json")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	body, err := readAllLimit(resp.Body, 8<<20)
	if err != nil {
		return nil, err
	}
	var feed struct {
		Data struct {
			Movies []ytsMovie `json:"movies"`
		} `json:"data"`
	}
	if err := json.Unmarshal(body, &feed); err != nil {
		return nil, fmt.Errorf("bad json: %w", err)
	}
	var res []Result
	for _, m := range feed.Data.Movies {
		if imdbID != "" && m.IMDBCode != "" && !strings.EqualFold(m.IMDBCode, imdbID) {
			continue // YTS matches loosely; keep only the title we asked for
		}
		for _, t := range m.Torrents {
			ih := strings.ToLower(strings.TrimSpace(t.Hash))
			if len(ih) != 40 {
				continue
			}
			src := strings.ToUpper(strings.TrimSpace(t.Type)) // bluray, web
			if src == "BLURAY" {
				src = "BluRay"
			}
			bits := []string{m.Title}
			if m.Year > 0 {
				bits = append(bits, strconv.Itoa(m.Year))
			}
			for _, b := range []string{t.Quality, src, strings.TrimSpace(t.VideoCode)} {
				if b != "" {
					bits = append(bits, b)
				}
			}
			res = append(res, Result{
				Title:    strings.Join(bits, " "),
				InfoHash: ih,
				Size:     t.SizeBytes,
				Seeders:  t.Seeds,
				Indexer:  ix.Name,
			})
		}
	}
	return res, nil
}

// ---------- AnimeTosho (anime, no key, JSON feed) ----------

var reYear = regexp.MustCompile(`\b(19|20)\d{2}\b`)

// toshoTitleQuery drops the year and episode markers: AnimeTosho's q= is a
// fuzzy full-text match, so "Ran 1985" happily returns a 1985 anime pilot.
func toshoTitleQuery(q string) string {
	q = reSxxExx.ReplaceAllString(q, " ")
	q = reYear.ReplaceAllString(q, " ")
	return strings.Join(strings.Fields(q), " ")
}

func isAlnumByte(c byte) bool {
	return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9'
}

// containsWord reports whether needle occurs in hay as a whole word, so "ran"
// does not match "Orange".
func containsWord(hay, needle string) bool {
	h, n := strings.ToLower(hay), strings.ToLower(needle)
	if n == "" {
		return false
	}
	for start := 0; start+len(n) <= len(h); {
		i := strings.Index(h[start:], n)
		if i < 0 {
			return false
		}
		i += start
		before := i == 0 || !isAlnumByte(h[i-1])
		after := i+len(n) == len(h) || !isAlnumByte(h[i+len(n)])
		if before && after {
			return true
		}
		start = i + 1
	}
	return false
}

// toshoRelevant keeps only entries that name what we asked for. Without this
// the feed's loose matching leaks unrelated shows into the result list.
func toshoRelevant(q, title string) bool {
	var words, long []string
	for _, f := range strings.Fields(toshoTitleQuery(q)) {
		if f = strings.Trim(f, ".,:;!?'\"()[]-"); f != "" {
			words = append(words, f)
			if len(f) >= 4 {
				long = append(long, f)
			}
		}
	}
	// Prefer tokens long enough to mean something; short titles ("Air", "Ran")
	// still need to match, so fall back to all of them.
	if len(long) > 0 {
		words = long
	}
	if len(words) == 0 {
		return true
	}
	for _, w := range words {
		if containsWord(title, w) {
			return true
		}
	}
	return false
}

type toshoEntry struct {
	Title       string `json:"title"`
	TorrentName string `json:"torrent_name"`
	InfoHash    string `json:"info_hash"`
	Magnet      string `json:"magnet_uri"`
	Seeders     int    `json:"seeders"`
	TotalSize   int64  `json:"total_size"`
}

// animeToshoSearch covers anime that nyaa missed (its feed aggregates nyaa,
// AniDex, NekoBT and its own tracker).
func animeToshoSearch(ctx context.Context, hc *http.Client, ix Indexer, q string) ([]Result, error) {
	base := strings.TrimRight(ix.URL, "/")
	if base == "" {
		base = "https://animetosho.org"
	}
	term := toshoTitleQuery(q)
	if term == "" {
		term = q
	}
	u := base + "/feed/json?q=" + url.QueryEscape(term)
	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) marlin/1.0")
	req.Header.Set("Accept", "application/json")
	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("http %d", resp.StatusCode)
	}
	body, err := readAllLimit(resp.Body, 16<<20)
	if err != nil {
		return nil, err
	}
	var feed []toshoEntry
	if err := json.Unmarshal(body, &feed); err != nil {
		return nil, fmt.Errorf("bad json: %w", err)
	}
	var res []Result
	for _, e := range feed {
		ih := strings.ToLower(strings.TrimSpace(e.InfoHash))
		if len(ih) != 40 {
			continue
		}
		title := strings.TrimSpace(e.TorrentName)
		if title == "" {
			title = strings.TrimSpace(e.Title)
		}
		if title == "" {
			continue
		}
		if !toshoRelevant(q, title) {
			continue // the feed answers loosely; do not show a different show
		}
		res = append(res, Result{
			Title:    title,
			InfoHash: ih,
			Size:     e.TotalSize,
			Seeders:  e.Seeders,
			Indexer:  ix.Name,
			Magnet:   e.Magnet,
			Trackers: trackersFromMagnet(e.Magnet),
		})
	}
	return res, nil
}
