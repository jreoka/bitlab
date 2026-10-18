package main

import (
	"context"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"
)

//go:embed web/admin.html web/favicon.svg
var webFS embed.FS

type adminCtxKey struct{}

type failWindow struct {
	mu   sync.Mutex
	hits map[string][]time.Time
}

var loginWindow = &failWindow{hits: map[string][]time.Time{}}

func (f *failWindow) blocked(ip string) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	cut := time.Now().Add(-10 * time.Minute)
	var keep []time.Time
	for _, t := range f.hits[ip] {
		if t.After(cut) {
			keep = append(keep, t)
		}
	}
	f.hits[ip] = keep
	return len(keep) >= 10
}

func (f *failWindow) fail(ip string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.hits[ip] = append(f.hits[ip], time.Now())
}

func (f *failWindow) reset(ip string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	delete(f.hits, ip)
}

func clientIP(r *http.Request) string {
	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		return strings.TrimSpace(strings.Split(xff, ",")[0])
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}

func (s *Server) handleFavicon(w http.ResponseWriter, r *http.Request) {
	b, err := webFS.ReadFile("web/favicon.svg")
	if err != nil {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Type", "image/svg+xml")
	w.Header().Set("Cache-Control", "public, max-age=86400")
	w.Write(b)
}

func (s *Server) handleAdminPage(w http.ResponseWriter, r *http.Request) {
	b, err := webFS.ReadFile("web/admin.html")
	if err != nil {
		http.Error(w, "ui missing", 500)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	w.Write(b)
}

func (s *Server) handleLogin(w http.ResponseWriter, r *http.Request) {
	ip := clientIP(r)
	if loginWindow.blocked(ip) {
		httpError(w, 429, "too many attempts, try again later")
		return
	}
	var body struct {
		Password string `json:"password"`
		Code     string `json:"code"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpError(w, 400, "bad json")
		return
	}
	if !s.checkPassword(body.Password) {
		loginWindow.fail(ip)
		s.log.Printf("admin login failed from %s", ip)
		httpError(w, 401, "wrong password")
		return
	}
	if s.twoFactorEnabled() {
		if strings.TrimSpace(body.Code) == "" {
			// Password was right, so this is not a failed attempt — just ask for the code.
			writeJSON(w, 401, map[string]any{"error": "two-factor code required", "twofa": true})
			return
		}
		if !s.checkSecondFactor(body.Code) {
			loginWindow.fail(ip)
			s.log.Printf("two-factor failure from %s", ip)
			httpError(w, 401, "invalid two-factor code")
			return
		}
	}
	loginWindow.reset(ip)
	ua := r.UserAgent()
	sess, err := s.store.CreateSession(sessionName(ua), ip, ua, 7*24*time.Hour)
	if err != nil {
		httpError(w, 500, err.Error())
		return
	}
	s.store.PruneSessions(50)
	s.issueCookie(w, r, sess)
	s.log.Printf("admin login from %s (%s)", ip, sess.Name)
	writeJSON(w, 200, map[string]any{"ok": true, "session": sess.ID})
}

func (s *Server) handleLogout(w http.ResponseWriter, r *http.Request) {
	if sess, ok := s.currentSession(r); ok {
		s.store.DeleteSession(sess.ID)
	}
	http.SetCookie(w, &http.Cookie{Name: "marlin_session", Value: "", Path: "/", MaxAge: -1})
	writeJSON(w, 200, map[string]any{"ok": true})
}

func (s *Server) requireAdmin(h http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if _, ok := s.currentSession(r); !ok {
			httpError(w, 401, "not signed in")
			return
		}
		h(w, r)
	}
}

type overview struct {
	Keys       []Key         `json:"keys"`
	Indexers   []Indexer     `json:"indexers"`
	Torrents   []TorrentView `json:"torrents"`
	CacheBytes int64         `json:"cache_bytes"`
	CacheMax   int64         `json:"cache_max_bytes"`
	Uptime     int64         `json:"uptime_seconds"`
	Version    string        `json:"version"`
	BaseURL    string        `json:"base_url"`
	KeyCount   int           `json:"active_keys"`
	MediaRows  int           `json:"media_rows"`
	IdleMin    int           `json:"idle_minutes"`
}

func (s *Server) handleOverview(w http.ResponseWriter, r *http.Request) {
	keys, err := s.store.ListKeys()
	if err != nil {
		s.log.Printf("overview: list keys: %v", err)
		httpError(w, 500, "store unavailable")
		return
	}
	ix, err := s.store.ListIndexers()
	if err != nil {
		s.log.Printf("overview: list indexers: %v", err)
		httpError(w, 500, "store unavailable")
		return
	}
	if keys == nil {
		keys = []Key{}
	}
	if ix == nil {
		ix = []Indexer{}
	}
	idle := int(s.engine.idleTimeout.Minutes())
	writeJSON(w, 200, overview{
		Keys:       keys,
		Indexers:   ix,
		Torrents:   s.engine.Views(),
		CacheBytes: s.engine.CacheBytes(),
		CacheMax:   s.engine.maxCacheBytes,
		Uptime:     int64(time.Since(s.start).Seconds()),
		Version:    "1.6.3",
		BaseURL:    s.cfg.BaseURL,
		KeyCount:   s.store.ActiveKeyCount(),
		MediaRows:  s.store.MediaCacheCount(),
		IdleMin:    idle,
	})
}

func (s *Server) handleCreateKey(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Label   string `json:"label"`
		Note    string `json:"note"`
		Expires string `json:"expires"`
		Days    int    `json:"days"`
	}
	json.NewDecoder(r.Body).Decode(&body)
	ttl, err := parseTTL(body.Expires)
	if err != nil {
		httpError(w, 400, err.Error())
		return
	}
	if ttl == 0 && body.Days > 0 {
		ttl = time.Duration(body.Days) * 24 * time.Hour
	}
	k, err := s.store.CreateKey(strings.TrimSpace(body.Label), strings.TrimSpace(body.Note), ttl)
	if err != nil {
		httpError(w, 500, err.Error())
		return
	}
	s.log.Printf("key created id=%s label=%q", k.ID, k.Label)
	writeJSON(w, 200, k)
}

func (s *Server) handlePauseKey(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	var body struct {
		Paused bool `json:"paused"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpError(w, 400, "bad json")
		return
	}
	status := "active"
	if body.Paused {
		status = "paused"
	}
	if err := s.store.SetKeyStatus(id, status); err != nil {
		code := 500
		if errors.Is(err, ErrKeyNotFound) {
			code = 404
		}
		httpError(w, code, err.Error())
		return
	}
	s.log.Printf("key %s status=%s", id, status)
	writeJSON(w, 200, map[string]any{"ok": true, "status": status})
}

func (s *Server) handleDeleteKey(w http.ResponseWriter, r *http.Request) {
	if err := s.store.DeleteKey(r.PathValue("id")); err != nil {
		httpError(w, 404, err.Error())
		return
	}
	s.log.Printf("key %s revoked (deleted)", r.PathValue("id"))
	writeJSON(w, 200, map[string]any{"ok": true})
}

func (s *Server) handleSaveIndexer(w http.ResponseWriter, r *http.Request) {
	var ix Indexer
	if err := json.NewDecoder(r.Body).Decode(&ix); err != nil {
		httpError(w, 400, "bad json")
		return
	}
	if ix.Name == "" {
		ix.Name = "indexer"
	}
	out, err := s.store.SaveIndexer(ix)
	if err != nil {
		httpError(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, out)
}

func (s *Server) handleDeleteIndexer(w http.ResponseWriter, r *http.Request) {
	if err := s.store.DeleteIndexer(r.PathValue("id")); err != nil {
		httpError(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, map[string]any{"ok": true})
}

func (s *Server) handleTestIndexer(w http.ResponseWriter, r *http.Request) {
	var ix Indexer
	if err := json.NewDecoder(r.Body).Decode(&ix); err != nil {
		httpError(w, 400, "bad json")
		return
	}
	q := r.URL.Query().Get("q")
	if q == "" {
		q = "big buck bunny"
	}
	ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
	defer cancel()
	res, err := s.searchIndexer(ctx, ix, q, "movie", 0, 0, "")
	if err != nil {
		httpError(w, 200, err.Error())
		return
	}
	if len(res) > 5 {
		res = res[:5]
	}
	writeJSON(w, 200, map[string]any{"count": len(res), "results": res})
}

func (s *Server) handleSearch(w http.ResponseWriter, r *http.Request) {
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	typ := r.URL.Query().Get("type")
	if typ == "" {
		typ = "movie"
	}
	if q == "" {
		httpError(w, 400, "q required")
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 25*time.Second)
	defer cancel()

	baseID, season, ep := parseMediaID(q)
	var imdb string
	if strings.HasPrefix(baseID, "tt") {
		imdb = baseID
		title, year, err := s.resolveMeta(ctx, typ, baseID)
		if err != nil {
			httpError(w, 502, "meta: "+err.Error())
			return
		}
		if typ == "series" && season > 0 {
			q = fmt.Sprintf("%s S%02dE%02d", title, season, ep)
		} else if year > 0 {
			q = fmt.Sprintf("%s %d", title, year)
		} else {
			q = title
		}
	}
	res, errs := s.searchAll(ctx, []string{q}, typ, season, ep, imdb)
	writeJSON(w, 200, map[string]any{"query": q, "count": len(res), "results": res, "errors": errs})
}

func (s *Server) handleSettings(w http.ResponseWriter, r *http.Request) {
	var body struct {
		CacheGB     *int `json:"cache_gb"`
		IdleMinutes *int `json:"idle_minutes"`
		ReadaheadMB *int `json:"readahead_mb"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpError(w, 400, "bad json")
		return
	}
	if body.CacheGB != nil && *body.CacheGB >= 0 {
		s.engine.maxCacheBytes = int64(*body.CacheGB) << 30
		s.store.SetSetting("cache_gb", strconv.Itoa(*body.CacheGB))
	}
	if body.IdleMinutes != nil && *body.IdleMinutes > 0 {
		s.engine.idleTimeout = time.Duration(*body.IdleMinutes) * time.Minute
		s.store.SetSetting("idle_minutes", strconv.Itoa(*body.IdleMinutes))
	}
	if body.ReadaheadMB != nil && *body.ReadaheadMB > 0 {
		s.cfg.ReadaheadBytes = int64(*body.ReadaheadMB) << 20
		s.store.SetSetting("readahead_mb", strconv.Itoa(*body.ReadaheadMB))
	}
	s.handleOverview(w, r)
}

func (s *Server) handleDropTorrent(w http.ResponseWriter, r *http.Request) {
	ih := strings.ToLower(r.PathValue("infohash"))
	for _, t := range s.engine.cl.Torrents() {
		if t.InfoHash().HexString() == ih {
			t.Drop()
			writeJSON(w, 200, map[string]any{"ok": true})
			return
		}
	}
	httpError(w, 404, "torrent not loaded")
}

func (s *Server) handleQuickPlay(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Magnet string `json:"magnet"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpError(w, 400, "bad json")
		return
	}
	ih, err := infohashFromMagnet(body.Magnet)
	if err != nil {
		httpError(w, 400, err.Error())
		return
	}
	keys, _ := s.store.ListKeys()
	if len(keys) == 0 {
		httpError(w, 400, "create a manifest key first")
		return
	}
	link := s.baseURL(r) + "/k/" + keys[0].Token + "/play/" + ih
	writeJSON(w, 200, map[string]any{"infohash": ih, "url": link})
}

func infohashFromMagnet(m string) (string, error) {
	if sub := ihRe.FindStringSubmatch(m); sub != nil {
		return strings.ToLower(sub[1]), nil
	}
	return "", errors.New("no infohash in magnet link")
}
