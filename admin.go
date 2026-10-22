package main

import (
	"context"
	"embed"
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
	// Bound the map: a spoofed address per request must not grow it forever.
	if len(f.hits) > 10_000 {
		cut := time.Now().Add(-10 * time.Minute)
		for k, v := range f.hits {
			if len(v) == 0 || v[len(v)-1].Before(cut) {
				delete(f.hits, k)
			}
		}
		if len(f.hits) > 10_000 {
			f.hits = map[string][]time.Time{}
		}
	}
	f.hits[ip] = append(f.hits[ip], time.Now())
}

func (f *failWindow) reset(ip string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	delete(f.hits, ip)
}

// clientIP returns the address used for login throttling. Forwarding headers
// are only honoured when the deployment says a proxy sets them; otherwise a
// direct caller could rotate the bucket with a header per request. When they
// are honoured, X-Real-IP and the rightmost XFF hop are the proxy's additions.
func (s *Server) clientIP(r *http.Request) string {
	ip := ""
	if s.cfg.TrustProxy {
		ip = strings.TrimSpace(r.Header.Get("X-Real-IP"))
		if ip == "" {
			if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
				parts := strings.Split(xff, ",")
				ip = strings.TrimSpace(parts[len(parts)-1])
			}
		}
	}
	if ip == "" {
		if host, _, err := net.SplitHostPort(r.RemoteAddr); err == nil {
			ip = host
		} else {
			ip = r.RemoteAddr
		}
	}
	if net.ParseIP(ip) == nil {
		return "unknown"
	}
	return ip
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
	// Every tab owns a path so a refresh stays where it was. The shell is the
	// same document; the script reads location.pathname. Unknown paths under
	// /admin/ are a 404 rather than a silent jump to the first tab.
	switch strings.TrimSuffix(r.URL.Path, "/") {
	case "/admin", "/admin/keys", "/admin/activity", "/admin/security":
	default:
		http.NotFound(w, r)
		return
	}
	// The console has no anonymous state: an unauthenticated visitor belongs on
	// the login URL, and comes back here once signed in. The destination rides
	// in a short-lived cookie rather than ?next=, so the login URL stays clean
	// and the path is not in the address bar, history or a proxy log.
	if _, ok := s.currentSession(r); !ok {
		if p := cleanNextPath(r.URL.Path); p != "/admin/keys" {
			http.SetCookie(w, &http.Cookie{
				Name:     "bitlab_next",
				Value:    p,
				Path:     "/",
				MaxAge:   600,
				HttpOnly: true,
				SameSite: http.SameSiteLaxMode,
				Secure:   r.TLS != nil || strings.HasPrefix(s.cfg.BaseURL, "https://"),
			})
		}
		http.Redirect(w, r, "/login", http.StatusFound)
		return
	}
	s.serveAdminShell(w, r)
}

// handleLoginPage serves the sign-in form at its own URL. The document is the
// console's shell; the script shows only the auth card when the path is /login.
func (s *Server) handleLoginPage(w http.ResponseWriter, r *http.Request) {
	if _, ok := s.currentSession(r); ok {
		http.Redirect(w, r, "/admin/keys", http.StatusFound)
		return
	}
	s.serveAdminShell(w, r)
}

// cleanNextPath is the allow-list for where a sign-in may land. Only the
// console tabs are accepted, so the cookie cannot be used as an open redirect.
func cleanNextPath(p string) string {
	switch strings.TrimSuffix(p, "/") {
	case "/admin/activity":
		return "/admin/activity"
	case "/admin/security":
		return "/admin/security"
	}
	return "/admin/keys"
}

func (s *Server) serveAdminShell(w http.ResponseWriter, r *http.Request) {
	b, err := webFS.ReadFile("web/admin.html")
	if err != nil {
		http.Error(w, "ui missing", 500)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	// The console is one inline-script page and uses inline handlers, so
	// unsafe-inline cannot be removed without a rewrite; everything else is
	// locked to this origin and framing is refused.
	w.Header().Set("Content-Security-Policy",
		"default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")
	w.Write(b)
}

func (s *Server) handleLogin(w http.ResponseWriter, r *http.Request) {
	ip := s.clientIP(r)
	if loginWindow.blocked(ip) {
		httpError(w, 429, "too many attempts, try again later")
		return
	}
	var body struct {
		Password string `json:"password"`
		Code     string `json:"code"`
	}
	if !decodeJSON(w, r, &body) {
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
	// Where the visitor was headed before the redirect to /login, if anywhere.
	next := "/admin/keys"
	if c, err := r.Cookie("bitlab_next"); err == nil {
		next = cleanNextPath(c.Value)
		http.SetCookie(w, &http.Cookie{Name: "bitlab_next", Value: "", Path: "/", MaxAge: -1})
	}
	writeJSON(w, 200, map[string]any{"ok": true, "session": sess.ID, "next": next})
}

func (s *Server) handleLogout(w http.ResponseWriter, r *http.Request) {
	if sess, ok := s.currentSession(r); ok {
		s.store.DeleteSession(sess.ID)
	}
	http.SetCookie(w, &http.Cookie{Name: "bitlab_session", Value: "", Path: "/", MaxAge: -1})
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
	Cache      CacheStats    `json:"cache"`
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
	// The console never renders indexers; do not ship their credentials to
	// every admin page load (and every XSS that reaches this endpoint).
	for i := range ix {
		ix[i].APIKey = ""
	}
	idle := int(s.engine.idleTimeout.Minutes())
	cache := s.engine.CacheStats()
	writeJSON(w, 200, overview{
		Keys:       keys,
		Indexers:   ix,
		Torrents:   s.engine.Views(),
		CacheBytes: cache.Used,
		CacheMax:   cache.Budget,
		Cache:      cache,
		Uptime:     int64(time.Since(s.start).Seconds()),
		Version:    "2.9.14",
		BaseURL:    s.cfg.BaseURL,
		KeyCount:   s.store.ActiveKeyCount(),
		MediaRows:  s.store.MediaCacheCount(),
		IdleMin:    idle,
	})
}

func (s *Server) handleListKeys(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query().Get("q")
	page := atoiDefault(r.URL.Query().Get("page"), 1)
	if page < 1 {
		page = 1
	}
	per := atoiDefault(r.URL.Query().Get("per"), 25)
	if per < 5 {
		per = 5
	}
	if per > 100 {
		per = 100
	}
	keys, total, err := s.store.ListKeysPage(q, per, (page-1)*per)
	if err != nil {
		s.log.Printf("list keys: %v", err)
		httpError(w, 500, "store unavailable")
		return
	}
	writeJSON(w, 200, map[string]any{"keys": keys, "total": total, "page": page, "per": per, "query": q})
}

func atoiDefault(s string, def int) int {
	n, err := strconv.Atoi(strings.TrimSpace(s))
	if err != nil {
		return def
	}
	return n
}

func (s *Server) handleCreateKey(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Label   string `json:"label"`
		Note    string `json:"note"`
		Expires string `json:"expires"`
		Days    int    `json:"days"`
	}
	if !decodeJSON(w, r, &body) {
		return
	}
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
	if !decodeJSON(w, r, &body) {
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
	var body struct {
		ID       string `json:"id"`
		Kind     string `json:"kind"`
		Name     string `json:"name"`
		URL      string `json:"url"`
		APIKey   string `json:"api_key"`
		Enabled  *bool  `json:"enabled"`
		Priority int    `json:"priority"`
	}
	if !decodeJSON(w, r, &body) {
		return
	}
	ix := Indexer{ID: body.ID, Kind: body.Kind, Name: body.Name, URL: body.URL,
		APIKey: body.APIKey, Priority: body.Priority, Enabled: true}
	if body.Enabled != nil {
		ix.Enabled = *body.Enabled
	}
	if ix.Name == "" {
		ix.Name = "indexer"
	}
	if ix.URL != "" {
		if err := checkOutboundURL(ix.URL); err != nil {
			httpError(w, 400, err.Error())
			return
		}
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
	if !decodeJSON(w, r, &ix) {
		return
	}
	if ix.URL != "" {
		if err := checkOutboundURL(ix.URL); err != nil {
			httpError(w, 400, err.Error())
			return
		}
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
	if !decodeJSON(w, r, &body) {
		return
	}
	if body.CacheGB != nil && *body.CacheGB >= 0 && *body.CacheGB <= 1_000_000 {
		s.engine.maxCacheBytes = int64(*body.CacheGB) << 30
		s.store.SetSetting("cache_gb", strconv.Itoa(*body.CacheGB))
	}
	if body.IdleMinutes != nil && *body.IdleMinutes > 0 && *body.IdleMinutes <= 7*24*60 {
		s.engine.idleTimeout = time.Duration(*body.IdleMinutes) * time.Minute
		s.store.SetSetting("idle_minutes", strconv.Itoa(*body.IdleMinutes))
	}
	if body.ReadaheadMB != nil && *body.ReadaheadMB > 0 && *body.ReadaheadMB <= 1024 {
		s.cfg.ReadaheadBytes = int64(*body.ReadaheadMB) << 20
		s.store.SetSetting("readahead_mb", strconv.Itoa(*body.ReadaheadMB))
	}
	s.handleOverview(w, r)
}
