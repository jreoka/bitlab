package main

import (
	"context"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const version = "1.5.4"

var defaultTrackers = []string{
	"udp://tracker.opentrackr.org:1337/announce",
	"udp://open.stealth.si:80/announce",
	"udp://tracker.torrent.eu.org:451/announce",
	"udp://tracker.openbittorrent.com:6969/announce",
	"udp://exodus.desync.com:6969/announce",
	"udp://open.demonii.com:1337/announce",
	"udp://tracker.moeking.me:6969/announce",
	"udp://explodie.org:6969/announce",
}

func env(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}

func envInt(k string, def int) int {
	if v := os.Getenv(k); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return def
}

func loadConfig() Config {
	dataDir := env("MARLIN_DATA", "./data")
	cfg := Config{
		Addr:           env("MARLIN_ADDR", ":8090"),
		DataDir:        dataDir,
		BaseURL:        env("MARLIN_BASE_URL", ""),
		CacheMaxBytes:  int64(envInt("MARLIN_CACHE_GB", 40)) << 30,
		IdleTimeout:    time.Duration(envInt("MARLIN_IDLE_MINUTES", 20)) * time.Minute,
		InfoTimeout:    time.Duration(envInt("MARLIN_INFO_TIMEOUT", 45)) * time.Second,
		ReadaheadBytes: int64(envInt("MARLIN_READAHEAD_MB", 64)) << 20,
		PrebufferMB:    int64(envInt("MARLIN_PREBUFFER_MB", 8)),
		TorrentPort:    envInt("MARLIN_TORRENT_PORT", 0),
		AdminPassword:  env("MARLIN_ADMIN_PASSWORD", ""),
	}
	if t := os.Getenv("MARLIN_TRACKERS"); t != "" {
		for _, s := range strings.Split(t, ",") {
			if s = strings.TrimSpace(s); s != "" {
				cfg.Trackers = append(cfg.Trackers, s)
			}
		}
	}
	if len(cfg.Trackers) == 0 {
		cfg.Trackers = defaultTrackers
	}
	return cfg
}

func main() {
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "-version", "--version", "version":
			fmt.Printf("marlin %s\n", version)
			return
		}
	}
	lg := log.New(os.Stdout, "", log.LstdFlags|log.LUTC)
	cfg := loadConfig()

	if err := os.MkdirAll(cfg.DataDir, 0o755); err != nil {
		lg.Fatalf("data dir: %v", err)
	}
	if cfg.AdminPassword == "" {
		cfg.AdminPassword = bootstrapAdminPassword(cfg.DataDir, lg)
	}

	store, err := OpenStore(filepath.Join(cfg.DataDir, "marlin.db"))
	if err != nil {
		lg.Fatalf("store: %v", err)
	}
	defer store.Close()

	if list, _ := store.ListIndexers(); len(list) == 0 && env("MARLIN_SEED_INDEXER", "on") != "off" {
		if _, err := store.SaveIndexer(Indexer{Kind: "knaben", Name: "knaben", Enabled: true}); err != nil {
			lg.Printf("seed indexer: %v", err)
		}
	}

	// Settings stored in the DB win over env defaults, so the UI is authoritative.
	if v := store.GetSetting("cache_gb", ""); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n >= 0 {
			cfg.CacheMaxBytes = int64(n) << 30
		}
	}
	if v := store.GetSetting("idle_minutes", ""); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			cfg.IdleTimeout = time.Duration(n) * time.Minute
		}
	}
	if v := store.GetSetting("readahead_mb", ""); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			cfg.ReadaheadBytes = int64(n) << 20
		}
	}

	eng, err := NewEngine(cfg, store, lg)
	if err != nil {
		lg.Fatalf("engine: %v", err)
	}
	defer eng.Close()

	secret := store.GetSetting("session_secret", "")
	if secret == "" {
		secret = randHex(32)
		store.SetSetting("session_secret", secret)
	}

	s := &Server{
		cfg:        cfg,
		store:      store,
		engine:     eng,
		http:       &http.Client{Timeout: 25 * time.Second},
		log:        lg,
		start:      time.Now(),
		sessionKey: []byte(secret),
	}

	mux := http.NewServeMux()
	s.routes(mux)
	handler := s.middleware(mux)

	srv := &http.Server{
		Addr:              cfg.Addr,
		Handler:           handler,
		ReadHeaderTimeout: 20 * time.Second,
		IdleTimeout:       0,
	}

	// Expired keys are deleted, not merely refused — sweep them even if nobody
	// ever presents the token again.
	go func() {
		t := time.NewTicker(time.Minute)
		defer t.Stop()
		for range t.C {
			if n := store.DeleteExpiredKeys(); n > 0 {
				lg.Printf("purged %d expired manifest key(s)", n)
			}
			store.PruneSessions(50)
		}
	}()

	go func() {
		lg.Printf("marlin %s listening on %s (data=%s, cache=%s)", version, cfg.Addr, cfg.DataDir, humanBytes(cfg.CacheMaxBytes))
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			lg.Fatalf("listen: %v", err)
		}
	}()

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt, syscall.SIGTERM)
	<-stop
	lg.Printf("shutting down")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	srv.Shutdown(ctx)
}

func bootstrapAdminPassword(dataDir string, lg *log.Logger) string {
	p := filepath.Join(dataDir, "ADMIN_PASSWORD")
	if b, err := os.ReadFile(p); err == nil {
		if s := strings.TrimSpace(string(b)); s != "" {
			return s
		}
	}
	pw := randToken(12)
	if err := os.WriteFile(p, []byte(pw+"\n"), 0o600); err != nil {
		lg.Fatalf("cannot write admin password: %v", err)
	}
	lg.Printf("generated admin password, saved to %s", p)
	return pw
}

func (s *Server) routes(mux *http.ServeMux) {
	// addon surface
	mux.HandleFunc("GET /k/{token}/manifest.json", s.handleManifest)
	mux.HandleFunc("GET /k/{token}/stream/{type}/{id}", s.handleStream)
	mux.HandleFunc("GET /k/{token}/play/{infohash}", s.handlePlay)
	mux.HandleFunc("GET /k/{token}/play/{infohash}/{fname}", s.handlePlay)

	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, map[string]any{"ok": true, "version": version, "uptime": int64(time.Since(s.start).Seconds())})
	})
	mux.HandleFunc("GET /manifest.json", func(w http.ResponseWriter, r *http.Request) {
		httpError(w, 401, "use your personal manifest URL: /k/<key>/manifest.json")
	})
	mux.HandleFunc("GET /", s.handleLanding)

	// admin
	mux.HandleFunc("GET /admin", s.handleAdminPage)
	mux.HandleFunc("GET /admin/", s.handleAdminPage)
	mux.HandleFunc("POST /admin/api/login", s.handleLogin)
	mux.HandleFunc("POST /admin/api/logout", s.handleLogout)
	mux.HandleFunc("GET /admin/api/overview", s.requireAdmin(s.handleOverview))
	mux.HandleFunc("GET /admin/api/keys", s.requireAdmin(s.handleOverview))
	mux.HandleFunc("POST /admin/api/keys", s.requireAdmin(s.handleCreateKey))
	mux.HandleFunc("POST /admin/api/keys/{id}/pause", s.requireAdmin(s.handlePauseKey))
	mux.HandleFunc("DELETE /admin/api/keys/{id}", s.requireAdmin(s.handleDeleteKey))
	mux.HandleFunc("POST /admin/api/indexers", s.requireAdmin(s.handleSaveIndexer))
	mux.HandleFunc("DELETE /admin/api/indexers/{id}", s.requireAdmin(s.handleDeleteIndexer))
	mux.HandleFunc("POST /admin/api/indexers/test", s.requireAdmin(s.handleTestIndexer))
	mux.HandleFunc("GET /admin/api/search", s.requireAdmin(s.handleSearch))
	mux.HandleFunc("POST /admin/api/settings", s.requireAdmin(s.handleSettings))
	mux.HandleFunc("GET /admin/api/security", s.requireAdmin(s.handleSecurity))
	mux.HandleFunc("POST /admin/api/2fa/begin", s.requireAdmin(s.handle2FABegin))
	mux.HandleFunc("POST /admin/api/2fa/enable", s.requireAdmin(s.handle2FAEnable))
	mux.HandleFunc("POST /admin/api/2fa/disable", s.requireAdmin(s.handle2FADisable))
	mux.HandleFunc("DELETE /admin/api/sessions/{id}", s.requireAdmin(s.handleDeleteSession))
	mux.HandleFunc("POST /admin/api/sessions/revoke-others", s.requireAdmin(s.handleRevokeOtherSessions))
	mux.HandleFunc("DELETE /admin/api/torrents/{infohash}", s.requireAdmin(s.handleDropTorrent))
	mux.HandleFunc("POST /admin/api/play", s.requireAdmin(s.handleQuickPlay))
}

func (s *Server) middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Range")
		w.Header().Set("Access-Control-Allow-Methods", "GET, HEAD, POST, DELETE, OPTIONS")
		w.Header().Set("Access-Control-Expose-Headers", "Content-Length, Content-Range, Accept-Ranges")
		w.Header().Set("X-Content-Type-Options", "nosniff")
		w.Header().Set("Referrer-Policy", "no-referrer")
		if r.Method == http.MethodOptions {
			w.WriteHeader(204)
			return
		}
		next.ServeHTTP(w, r)
	})
}

func (s *Server) handleLanding(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	// Deliberately does not list manifest keys: they are credentials, and this
	// page is public.
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	fmt.Fprint(w, landingHTML)
}

const landingHTML = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Marlin</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
:root{color-scheme:dark}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:grid;place-items:center;padding:32px;background:#0a0b0d;color:#e9ebef;
 font:13.5px/1.65 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;-webkit-font-smoothing:antialiased}
.wrap{width:100%;max-width:360px}
.brand{display:flex;align-items:baseline;gap:9px;margin-bottom:30px}
.mark{color:#5b8cff;font-weight:700;font-size:17px;letter-spacing:-.03em}
.name{font-size:13.5px;font-weight:600}
.ver{font-size:10px;color:#5d646f;text-transform:uppercase;letter-spacing:.09em}
h1{margin:0 0 8px;font-size:16px;font-weight:600;letter-spacing:-.01em}
p{margin:0 0 20px;color:#8b929e;font-size:12.5px;max-width:34ch}
ul{margin:0 0 26px;padding:0;list-style:none;font-size:12.2px;color:#5d646f;display:grid;gap:5px}
li::before{content:"— ";color:#3c424b}
a{color:#5b8cff;text-decoration:none;font-size:12.5px}
a:hover{text-decoration:underline}
</style></head><body><div class="wrap">
 <div class="brand"><span class="mark">M</span><span class="name">Marlin</span><span class="ver">hub</span></div>
 <h1>Private add-on server</h1>
 <p>Manifest keys are issued from the console. Install one in Stremio or Nuvio to start streaming.</p>
 <ul><li>per-device keys — pause, expire or revoke</li><li>no debrid — Marlin fetches and streams itself</li></ul>
 <a href="/admin">Open the console →</a>
</div></body></html>`
