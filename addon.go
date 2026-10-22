package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

type Config struct {
	Addr              string
	DataDir           string
	BaseURL           string
	Trackers          []string
	CacheMaxBytes     int64
	CacheReserveBytes int64
	CacheRetain       time.Duration
	CacheGrace        time.Duration
	IdleTimeout       time.Duration
	InfoTimeout       time.Duration
	ReadaheadBytes    int64
	PrebufferMB       int64
	TorrentPort       int
	MaxTorrents       int
	Indexers          []Indexer
	AdminPassword     string
	TrustProxy        bool
}

type Server struct {
	cfg        Config
	store      *Store
	engine     *Engine
	http       *http.Client
	log        *log.Logger
	start      time.Time
	sessionKey []byte
	bytesOut   int64
	hints      sync.Map // infohash -> []string trackers, learned from search results
	hintN      atomic.Int64
	totpMu     sync.Mutex
	ixCache    sync.Map // indexer+query -> cached results for a few minutes
	epCache    sync.Map // imdb id -> ordered [season, episode] pairs
}

// ---- addon protocol types ----

type Manifest struct {
	ID          string   `json:"id"`
	Version     string   `json:"version"`
	Name        string   `json:"name"`
	Description string   `json:"description"`
	Resources   []string `json:"resources"`
	Types       []string `json:"types"`
	IDPrefixes  []string `json:"idPrefixes"`
	Catalogs    []any    `json:"catalogs"`
	BehaviorH   any      `json:"behaviorHints,omitempty"`
}

type Stream struct {
	Name          string         `json:"name"`
	Title         string         `json:"title,omitempty"`
	URL           string         `json:"url"`
	BehaviorHints map[string]any `json:"behaviorHints,omitempty"`
}

type StreamResponse struct {
	Streams []Stream `json:"streams"`
}

func (s *Server) manifestFor(k Key) Manifest {
	return Manifest{
		ID:          "org.bitlab." + k.ID,
		Version:     "2.9.12",
		Name:        "Bitlab",
		Description: "Self-hosted streaming hub. Manifest: " + k.Label,
		Resources:   []string{"stream"},
		Types:       []string{"movie", "series"},
		IDPrefixes:  []string{"tt"},
		Catalogs:    []any{},
	}
}

func (s *Server) handleManifest(w http.ResponseWriter, r *http.Request) {
	k, ok := s.authKey(w, r)
	if !ok {
		return
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.Header().Set("Access-Control-Allow-Origin", "*")
	w.Header().Set("Cache-Control", "max-age=300")
	json.NewEncoder(w).Encode(s.manifestFor(k))
}

// fixEpisodeNumber reconciles the client's numbering with the show's. Anime
// catalogs number episodes continuously and file them all under season 1, while
// releases name them by season: a request for S01E51 of a 25+25 episode show
// means S03E01, and searching "S01E51" finds nothing.
func (s *Server) fixEpisodeNumber(imdbID string, season, ep int) (int, int) {
	if ep <= 0 || season <= 0 {
		return season, ep
	}
	v, ok := s.epCache.Load(imdbID)
	if !ok {
		return season, ep
	}
	return remapEpisode(v.([][2]int), season, ep)
}

// remapEpisode treats ep as an absolute number when it cannot fit the season it
// was filed under.
func remapEpisode(eps [][2]int, season, ep int) (int, int) {
	if len(eps) == 0 {
		return season, ep
	}
	// Only the first season can be the one carrying a continuous count; an
	// out-of-range number anywhere else is just bad data.
	if season != eps[0][0] {
		return season, ep
	}
	inSeason := 0
	for _, e := range eps {
		if e[0] == season {
			inSeason++
		}
	}
	if ep <= inSeason {
		return season, ep // it fits: no remapping needed
	}
	if ep <= len(eps) {
		return eps[ep-1][0], eps[ep-1][1]
	}
	return season, ep
}

func (s *Server) handleStream(w http.ResponseWriter, r *http.Request) {
	k, ok := s.authKey(w, r)
	if !ok {
		return
	}
	typ := r.PathValue("type")
	rawID := strings.TrimSuffix(r.PathValue("id"), ".json")
	baseID, season, ep := parseMediaID(rawID)

	if typ != "movie" && typ != "series" {
		writeJSON(w, 200, StreamResponse{Streams: []Stream{}})
		return
	}
	if typ == "series" && season <= 0 {
		// Specials (season 0) and season-less ids carry no episode numbering, so
		// there is nothing to match a release file against: the rows used to be
		// whatever the indexers returned and the play handler served the largest
		// file, which is how S00E01 answered with the latest airing episode.
		s.log.Printf("stream %q: no season/episode to match, refusing to guess", rawID)
		writeJSON(w, 200, StreamResponse{Streams: []Stream{}})
		return
	}

	ctx, cancel := context.WithTimeout(r.Context(), 30*time.Second)
	defer cancel()

	title, year, err := s.resolveMeta(ctx, typ, baseID)
	if err != nil {
		s.log.Printf("stream %q: meta: %v", rawID, err)
		writeJSON(w, 200, StreamResponse{Streams: []Stream{}})
		return
	}

	// Reconcile the client's numbering with the show's before searching: an
	// absolute episode number filed under season 1 is a different season here.
	season, ep = s.fixEpisodeNumber(baseID, season, ep)

	query := title
	if typ == "series" && season > 0 {
		query = fmt.Sprintf("%s S%02dE%02d", title, season, ep)
	}

	queries := []string{query}
	var aliases, playHints []string
	if typ == "series" && season > 0 {
		// Older seasons are usually only available as a batch, which is never
		// titled "S01E01" — ask for the season as well as the episode.
		queries = append(queries, fmt.Sprintf("%s S%02d", title, season))
		aliases = s.animeAliases(ctx, title)
		for i, alias := range aliases {
			if i >= 2 {
				break
			}
			queries = append(queries, fmt.Sprintf("%s S%02dE%02d", alias, season, ep))
			if i == 0 {
				queries = append(queries, fmt.Sprintf("%s S%02d", alias, season))
			}
		}
		// Release groups write anime as "Show - 04" far more often than "SxxE04",
		// so aliases are also searched by name alone and filtered by episode later.
		if len(aliases) > 0 {
			queries = append(queries, aliases[0])
		}
	} else if typ == "movie" {
		// Anime films are indexed under their romaji title and usually carry the
		// Japanese release year, which is two or three years before the date
		// Cinemeta reports. Asking only for the English title left Spirited Away
		// with 1 row out of 40; the alias is the only way to see the rest.
		// Releases are filed under the part of a franchise name, not the whole
		// line: "Re:ZERO -Starting Life in Another World- Memory Snow" returns
		// zero hits from knaben, nyaa and animetosho, while "Memory Snow 2018"
		// returns thirty. The same name is a *bundle* entry on AniList — one
		// record covering both OVAs — so the part is also the only name that
		// identifies which film this is.
		forms := titleForms(title)
		lookup := title
		if len(forms) > 1 {
			lookup = forms[1]
			queries = append(queries, withYear(forms[1], year))
		}
		aliases = s.animeAliases(ctx, lookup)
		for _, f := range forms[1:] {
			aliases = append(aliases, f) // the part is what releases say
		}
		for i, alias := range aliases {
			if i >= 2 {
				break
			}
			queries = append(queries, withYear(alias, year))
		}
		if clean := cleanTitle(title); clean != "" && !strings.EqualFold(clean, title) {
			queries = append(queries, clean)
		}
		// The year goes last: appending it to an anime film's name is what made
		// nyaa and animetosho answer with nothing at all, while the relevance
		// gate still drops a release whose name carries a different year.
		queries = append(queries, withYear(title, year))
		// Every name this film goes by is a hint for the play handler, which has
		// to pick the right film out of a release that holds several.
		for _, h := range append(append([]string{}, forms...), aliases...) {
			playHints = append(playHints, titleForms(h)...)
		}
		playHints = dedupeStrings(playHints)
	}
	queries = dedupeStrings(queries)
	results, errs := s.searchAll(ctx, queries, typ, season, ep, baseID)
	for _, e := range errs {
		s.log.Printf("stream %q: %s", rawID, e)
	}
	// Indexers answer with whatever their fuzzy matcher liked; keep only what
	// could plausibly be this title, this year.
	if filtered, dropped := filterRelevant(results, typ, title, aliases, year, season); dropped > 0 {
		s.log.Printf("stream %s: dropped %d of %d candidates that do not match %q (%d)",
			rawID, dropped, len(results), title, year)
		results = filtered
	}
	if len(results) == 0 {
		s.log.Printf("stream %q: no results for %q", rawID, query)
		writeJSON(w, 200, StreamResponse{Streams: []Stream{}})
		return
	}

	base := s.baseURL(r)
	streams := make([]Stream, 0, len(results))
	for i, res := range results {
		// The infohash becomes part of a magnet URI and a play URL; refuse
		// anything that is not 40 hex characters before it reaches either.
		res.InfoHash = strings.ToLower(strings.TrimSpace(res.InfoHash))
		if !validInfohash(res.InfoHash) {
			continue
		}
		if len(res.Trackers) > 0 {
			clean := make([]string, 0, len(res.Trackers))
			for _, tr := range res.Trackers {
				if len(clean) >= 20 {
					break
				}
				// Indexer hints may only carry HTTP(S) trackers: UDP tracker
				// resolution is done inside anacrolix and cannot be forced through
				// the guarded dialer.
				if validTracker(tr) && (strings.HasPrefix(strings.ToLower(tr), "http://") || strings.HasPrefix(strings.ToLower(tr), "https://")) {
					clean = append(clean, strings.TrimSpace(tr))
				}
			}
			if len(clean) > 0 {
				if s.hintN.Add(1) > 8192 {
					s.hints.Range(func(k, _ any) bool {
						s.hints.Delete(k)
						s.hintN.Add(-1)
						return true
					})
				}
				s.hints.Store(res.InfoHash, clean)
			}
		}
		label, q := releaseLabel(res.Title)
		src := strings.TrimSpace(strings.TrimPrefix(label, q))
		// First line is the bold one in the player: show the title being watched,
		// not the addon name, with the resolution as the differentiator.
		display := title
		if typ == "series" && season > 0 {
			display = fmt.Sprintf("%s S%02dE%02d", title, season, ep)
		} else if year > 0 {
			display = fmt.Sprintf("%s (%d)", title, year)
		}
		tk, exp := s.signedPlayToken(k.ID, res.InfoHash, season, ep)
		link := fmt.Sprintf("%s/k/%s/play/%s?n=%s&tk=%s&exp=%d", base, k.Token, res.InfoHash, url.QueryEscape(truncate(res.Title, 120)), tk, exp)
		if typ == "series" && season > 0 {
			// Tells the play handler which file to pick inside a season pack.
			link += fmt.Sprintf("&s=%d&e=%d", season, ep)
		}
		// A movie torrent can hold several films (an OVA pack, a trilogy), so the
		// play handler needs the title to pick the right file. Series rows carry
		// the episode instead: "largest file" would be the wrong episode.
		if typ == "movie" {
			hints := playHints
			if len(hints) == 0 {
				hints = titleForms(title)
			}
			for i, h := range hints {
				if i >= 4 {
					break
				}
				link += "&t=" + url.QueryEscape(h)
			}
		}
		bits := []string{}
		if src != "" {
			bits = append(bits, src)
		}
		bits = append(bits, "💾 "+humanBytes(res.Size), fmt.Sprintf("👥 %d", res.Seeders), res.Indexer)
		detail := strings.Join(bits, " · ")
		streams = append(streams, Stream{
			Name:  fmt.Sprintf("%s · %s", display, q),
			Title: detail,
			URL:   link,
			BehaviorHints: map[string]any{
				"bingeGroup":  "bitlab-" + q,
				"notWebReady": false,
			},
		})
		// Warm the head of the plausible picks: deeper for the top rows, which
		// are the ones actually clicked, and shallow for the rest.
		if i < 6 {
			mb := int64(6)
			if i < 3 {
				mb = s.cfg.PrebufferMB * 3
			}
			s.engine.Warm(res.InfoHash, res.Title, nil, mb)
		}
	}
	writeJSON(w, 200, StreamResponse{Streams: streams})
}

func parseMediaID(id string) (base string, season, ep int) {
	id = strings.TrimSuffix(id, ".json")
	parts := strings.Split(id, ":")
	base = parts[0]
	if len(parts) >= 3 {
		season, _ = strconv.Atoi(parts[1])
		ep, _ = strconv.Atoi(parts[2])
	}
	return
}

func releaseLabel(title string) (string, string) {
	q := "SD"
	switch {
	case regexp.MustCompile(`(?i)\b(2160p|4k|uhd)\b`).MatchString(title):
		q = "2160p"
	case regexp.MustCompile(`(?i)\b1080p\b`).MatchString(title):
		q = "1080p"
	case regexp.MustCompile(`(?i)\b720p\b`).MatchString(title):
		q = "720p"
	}
	src := ""
	switch {
	case regexp.MustCompile(`(?i)\bremux\b`).MatchString(title):
		src = "REMUX"
	case regexp.MustCompile(`(?i)\b(bluray|blu-ray|bdrip)\b`).MatchString(title):
		src = "BluRay"
	case regexp.MustCompile(`(?i)\b(web-dl|webrip|web)\b`).MatchString(title):
		src = "WEB"
	}
	label := strings.TrimSpace(strings.Join([]string{q, src}, " "))
	return label, q
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n]
}

// resolveMeta looks up a title for an IMDb id via Cinemeta (Stremio's public catalogue).
func (s *Server) resolveMeta(ctx context.Context, typ, imdbID string) (string, int, error) {
	ck := "meta2:" + typ + ":" + imdbID
	if v, ok := s.store.GetCachedMeta(ck); ok {
		var m struct {
			Name string   `json:"name"`
			Year int      `json:"year"`
			Eps  [][2]int `json:"eps"`
		}
		if json.Unmarshal([]byte(v), &m) == nil && m.Name != "" {
			s.epCache.Store(imdbID, m.Eps)
			return m.Name, m.Year, nil
		}
	}
	u := fmt.Sprintf("https://v3-cinemeta.strem.io/meta/%s/%s.json", typ, url.PathEscape(imdbID))
	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return "", 0, err
	}
	req.Header.Set("User-Agent", "bitlab/1.0")
	resp, err := s.http.Do(req)
	if err != nil {
		return "", 0, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return "", 0, fmt.Errorf("cinemeta http %d", resp.StatusCode)
	}
	var payload struct {
		Meta struct {
			Name        string `json:"name"`
			Year        string `json:"year"`
			ReleaseInfo string `json:"releaseInfo"`
			Videos      []struct {
				Season  int `json:"season"`
				Episode int `json:"episode"`
			} `json:"videos"`
		} `json:"meta"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, 16<<20)).Decode(&payload); err != nil {
		return "", 0, err
	}
	name := strings.TrimSpace(payload.Meta.Name)
	if name == "" {
		return "", 0, errors.New("no metadata for " + imdbID)
	}
	yearStr := payload.Meta.Year
	if yearStr == "" {
		yearStr = payload.Meta.ReleaseInfo
	}
	if len(yearStr) > 4 {
		yearStr = yearStr[:4]
	}
	year, _ := strconv.Atoi(yearStr)
	// The episode list is what tells us whether the client is numbering
	// episodes per season or continuously.
	eps := make([][2]int, 0, len(payload.Meta.Videos))
	for _, v := range payload.Meta.Videos {
		if v.Season > 0 && v.Episode > 0 {
			eps = append(eps, [2]int{v.Season, v.Episode})
		}
	}
	sort.Slice(eps, func(i, j int) bool {
		if eps[i][0] != eps[j][0] {
			return eps[i][0] < eps[j][0]
		}
		return eps[i][1] < eps[j][1]
	})
	s.epCache.Store(imdbID, eps)
	b, _ := json.Marshal(map[string]any{"name": name, "year": year, "eps": eps})
	s.store.CacheMeta(ck, string(b), 30*24*time.Hour)
	return name, year, nil
}

// authKey resolves and validates the manifest key on the URL path.
func (s *Server) authKey(w http.ResponseWriter, r *http.Request) (Key, bool) {
	token := r.PathValue("token")
	if token == "" {
		httpError(w, 401, "missing manifest key")
		return Key{}, false
	}
	k, err := s.store.KeyByToken(token)
	switch {
	case err == nil:
		s.store.TouchKey(k.ID)
		return k, true
	case errors.Is(err, ErrKeyNotFound):
		httpError(w, 401, "unknown manifest key")
	case errors.Is(err, ErrKeyPaused):
		httpError(w, 403, "this manifest key is paused")
	case errors.Is(err, ErrKeyExpired):
		httpError(w, 410, "this manifest key expired")
	default:
		httpError(w, 500, err.Error())
	}
	return Key{}, false
}
