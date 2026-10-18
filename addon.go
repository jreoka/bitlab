package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"
)

type Config struct {
	Addr           string
	DataDir        string
	BaseURL        string
	Trackers       []string
	CacheMaxBytes  int64
	IdleTimeout    time.Duration
	InfoTimeout    time.Duration
	ReadaheadBytes int64
	PrebufferMB    int64
	TorrentPort    int
	Indexers       []Indexer
	AdminPassword  string
}

type Server struct {
	cfg         Config
	store       *Store
	engine      *Engine
	http        *http.Client
	log         *log.Logger
	start       time.Time
	sessionKey  []byte
	bytesOut    int64
	hints       sync.Map // infohash -> []string trackers, learned from search results
	ixCache     sync.Map // indexer+query -> cached results for a few minutes
	probeSem    chan struct{}
	probeMu     sync.Mutex
	probeBusy   map[string]bool
	resolveMu   sync.Mutex
	resolveBusy map[string]chan struct{}
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
		Version:     "2.2.3",
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

// streamQueries is the query set for one title. Anime is indexed under several
// names, and older seasons usually exist only as a batch, which is never titled
// "S01E01" — so both the episode and the season are asked for.
func (s *Server) streamQueries(ctx context.Context, typ, title string, year, season, ep int) []string {
	query := title
	if typ == "series" && season > 0 {
		query = fmt.Sprintf("%s S%02dE%02d", title, season, ep)
	} else if year > 0 {
		query = fmt.Sprintf("%s %d", title, year)
	}
	queries := []string{query}
	if typ != "series" || season <= 0 {
		return queries
	}
	queries = append(queries, fmt.Sprintf("%s S%02d", title, season))
	aliases := s.animeAliases(ctx, title)
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
	return queries
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

	ctx, cancel := context.WithTimeout(r.Context(), 30*time.Second)
	defer cancel()

	title, year, err := s.resolveMeta(ctx, typ, baseID)
	if err != nil {
		s.log.Printf("stream %s: meta: %v", rawID, err)
		writeJSON(w, 200, StreamResponse{Streams: []Stream{}})
		return
	}

	queries := s.streamQueries(ctx, typ, title, year, season, ep)
	results, errs := s.searchAll(ctx, queries, typ, season, ep, baseID)
	for _, e := range errs {
		s.log.Printf("stream %s: %s", rawID, e)
	}
	if len(results) == 0 {
		s.log.Printf("stream %s: no results for %q", rawID, queries[0])
		writeJSON(w, 200, StreamResponse{Streams: []Stream{}})
		return
	}

	// One row, and the system chooses. The results are still walked here: the
	// top candidates are warmed and probed so the click that follows does not
	// wait on a cold swarm, but they are no longer offered as alternatives.
	for i, res := range results {
		if len(res.Trackers) > 0 {
			s.hints.Store(res.InfoHash, res.Trackers)
		}
		if i < 6 {
			mb := int64(6)
			if i < 3 {
				mb = s.cfg.PrebufferMB * 3
			}
			s.engine.Warm(res.InfoHash, res.Title, nil, mb)
		}
		if i < 4 {
			go s.warmProbe(res)
		}
	}
	writeJSON(w, 200, StreamResponse{Streams: []Stream{s.autoRow(s.baseURL(r), k, typ, rawID)}})

	// The list is already on the client's screen; decide behind it. The search
	// results and the warm probes are in hand here, so by the time the row is
	// clicked there is usually nothing left to work out.
	go s.resolveAhead(rawID, typ, season, results)
}

func bestCachedFile(files []CachedFile) (int, bool) {
	best := -1
	for i, f := range files {
		if !isVideo(f.Path) || f.Size < 20<<20 {
			continue
		}
		if best < 0 || f.Size > files[best].Size {
			best = i
		}
	}
	if best < 0 {
		return 0, false
	}
	return files[best].Idx, true
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

var qualityRe = regexp.MustCompile(`(?i)\b(2160p|4k|1080p|720p|480p|sd)\b`)

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
	ck := "meta:" + typ + ":" + imdbID
	if v, ok := s.store.GetCachedMeta(ck); ok {
		var m struct {
			Name string `json:"name"`
			Year int    `json:"year"`
		}
		if json.Unmarshal([]byte(v), &m) == nil && m.Name != "" {
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
		} `json:"meta"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
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
	b, _ := json.Marshal(map[string]any{"name": name, "year": year})
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
