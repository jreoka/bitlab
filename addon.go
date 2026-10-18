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
	AdminPassword  string
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
		ID:          "org.marlin." + k.ID,
		Version:     "1.4.2",
		Name:        "Marlin",
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

	query := title
	if typ == "series" && season > 0 {
		query = fmt.Sprintf("%s S%02dE%02d", title, season, ep)
	} else if year > 0 {
		query = fmt.Sprintf("%s %d", title, year)
	}

	queries := []string{query}
	if typ == "series" && season > 0 {
		aliases := s.animeAliases(ctx, title)
		for i, alias := range aliases {
			if i >= 2 {
				break
			}
			queries = append(queries, fmt.Sprintf("%s S%02dE%02d", alias, season, ep))
		}
		// Release groups write anime as "Show - 04" far more often than "SxxE04",
		// so one alias is searched by name alone and filtered by episode later.
		if len(aliases) > 0 {
			queries = append(queries, aliases[0])
		}
	}
	results, errs := s.searchAll(ctx, queries, typ, season, ep, baseID)
	for _, e := range errs {
		s.log.Printf("stream %s: %s", rawID, e)
	}
	if len(results) == 0 {
		s.log.Printf("stream %s: no results for %q", rawID, query)
		writeJSON(w, 200, StreamResponse{Streams: []Stream{}})
		return
	}

	base := s.baseURL(r)
	streams := make([]Stream, 0, len(results))
	for i, res := range results {
		if len(res.Trackers) > 0 {
			s.hints.Store(res.InfoHash, res.Trackers)
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
		link := fmt.Sprintf("%s/k/%s/play/%s?n=%s", base, k.Token, res.InfoHash, url.QueryEscape(truncate(res.Title, 120)))
		if typ == "series" && season > 0 {
			// Tells the play handler which file to pick inside a season pack.
			link += fmt.Sprintf("&s=%d&e=%d", season, ep)
		}
		if _, _, files, ok := s.store.GetMedia(res.InfoHash); ok {
			if idx, ok := bestCachedFile(files); ok {
				link += "&f=" + strconv.Itoa(idx)
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
				"bingeGroup":  "marlin-" + q,
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
	req.Header.Set("User-Agent", "marlin/1.0")
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
