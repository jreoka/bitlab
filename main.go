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

const version = "1.4.1"

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
	mux.HandleFunc("POST /admin/api/keys/{id}/revoke", s.requireAdmin(s.handleRevokeKey))
	mux.HandleFunc("DELETE /admin/api/keys/{id}", s.requireAdmin(s.handleDeleteKey))
	mux.HandleFunc("POST /admin/api/indexers", s.requireAdmin(s.handleSaveIndexer))
	mux.HandleFunc("DELETE /admin/api/indexers/{id}", s.requireAdmin(s.handleDeleteIndexer))
	mux.HandleFunc("POST /admin/api/indexers/test", s.requireAdmin(s.handleTestIndexer))
	mux.HandleFunc("GET /admin/api/search", s.requireAdmin(s.handleSearch))
	mux.HandleFunc("POST /admin/api/settings", s.requireAdmin(s.handleSettings))
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
	keys, _ := s.store.ListKeys()
	var rows strings.Builder
	for _, k := range keys {
		if k.Revoked {
			continue
		}
		u := s.baseURL(r) + "/k/" + k.Token + "/manifest.json"
		rows.WriteString(fmt.Sprintf("<li><b>%s</b><br><code>%s</code></li>", htmlEscape(k.Label), htmlEscape(u)))
	}
	if rows.Len() == 0 {
		rows.WriteString("<li>No keys yet — sign in and create one.</li>")
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	fmt.Fprintf(w, landingHTML, rows.String())
}

func htmlEscape(s string) string {
	r := strings.NewReplacer("&", "&amp;", "<", "&lt;", ">", "&gt;", `"`, "&quot;")
	return r.Replace(s)
}

const landingHTML = `<!doctype html>
<html><head><meta charset="utf-8"><title>Marlin</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
:root{color-scheme:dark}
body{margin:0;min-height:100vh;background:#0b0e14;color:#d7dce5;font:15px/1.6 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;display:flex;align-items:center;justify-content:center;padding:24px}
.card{max-width:720px;width:100%%;background:#11151d;border:1px solid #1f2632;border-radius:16px;padding:32px}
h1{margin:0 0 4px;font-size:26px;letter-spacing:-.02em}
p{color:#8b95a7;margin:0 0 24px}
ul{list-style:none;padding:0;display:grid;gap:12px}
li{background:#0e1219;border:1px solid #1c2431;border-radius:10px;padding:12px 14px;font-size:13px;word-break:break-all}
code{color:#7ee2b8;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
a.btn{display:inline-block;margin-top:22px;background:#2b6cf6;color:#fff;text-decoration:none;padding:10px 16px;border-radius:9px;font-weight:600;font-size:14px}
</style></head><body><div class="card">
<h1>Marlin</h1><p>Self-hosted streaming hub. Install one of these manifests in Stremio or Nuvio.</p>
<ul>%s</ul>
<a class="btn" href="/admin">Admin console</a>
</div></body></html>`
