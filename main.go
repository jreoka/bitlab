package main

import (
	"context"
	"encoding/json"
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

const version = "2.9.6"

// healthFreeFloor is the free space below which /readyz reports not-ready. The
// cache sweep already holds BITLAB_CACHE_RESERVE_GB, so crossing this means the
// volume is genuinely out of room.
const healthFreeFloor = int64(200) << 20

var defaultTrackers = []string{
	// HTTP(S) first: some hosts block outbound UDP, which kills both DHT and
	// every udp:// announce — then an HTTP tracker is the only way to find peers.
	"http://nyaa.tracker.wf:7777/announce",
	"http://tracker.opentrackr.org:1337/announce",
	"http://tracker.openbittorrent.com:80/announce",
	"https://tracker.tamersunion.org:443/announce",
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
	dataDir := env("BITLAB_DATA", "./data")
	cfg := Config{
		Addr:              env("BITLAB_ADDR", ":8090"),
		DataDir:           dataDir,
		BaseURL:           env("BITLAB_BASE_URL", ""),
		CacheMaxBytes:     int64(envInt("BITLAB_CACHE_GB", 40)) << 30,
		CacheReserveBytes: int64(envInt("BITLAB_CACHE_RESERVE_GB", 2)) << 30,
		CacheRetain:       time.Duration(envInt("BITLAB_CACHE_RETAIN_HOURS", 24)) * time.Hour,
		CacheGrace:        time.Duration(envInt("BITLAB_CACHE_GRACE_MINUTES", 10)) * time.Minute,
		IdleTimeout:       time.Duration(envInt("BITLAB_IDLE_MINUTES", 20)) * time.Minute,
		InfoTimeout:       time.Duration(envInt("BITLAB_INFO_TIMEOUT", 45)) * time.Second,
		ReadaheadBytes:    int64(envInt("BITLAB_READAHEAD_MB", 64)) << 20,
		PrebufferMB:       int64(envInt("BITLAB_PREBUFFER_MB", 8)),
		TorrentPort:       envInt("BITLAB_TORRENT_PORT", 0),
		AdminPassword:     env("BITLAB_ADMIN_PASSWORD", ""),
	}
	if t := os.Getenv("BITLAB_TRACKERS"); t != "" {
		for _, s := range strings.Split(t, ",") {
			if s = strings.TrimSpace(s); s != "" {
				cfg.Trackers = append(cfg.Trackers, s)
			}
		}
	}
	if raw := os.Getenv("BITLAB_INDEXERS"); raw != "" {
		// Declarative indexer config, applied on every boot:
		// [{"kind":"torznab","name":"prowlarr","url":"http://prowlarr:9696/1/api","api_key":"..."}]
		if err := json.Unmarshal([]byte(raw), &cfg.Indexers); err != nil {
			fmt.Fprintf(os.Stderr, "BITLAB_INDEXERS is not valid JSON: %v\n", err)
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
			fmt.Printf("bitlab %s\n", version)
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

	store, err := OpenStore(filepath.Join(cfg.DataDir, "bitlab.db"))
	if err != nil {
		lg.Fatalf("store: %v", err)
	}
	defer store.Close()

	if len(cfg.Indexers) > 0 {
		for _, ix := range cfg.Indexers {
			ix.Enabled = true
			if ix.Name == "" {
				ix.Name = ix.Kind
			}
			if err := store.UpsertIndexerByName(ix); err != nil {
				lg.Printf("indexer %q: %v", ix.Name, err)
				continue
			}
			lg.Printf("indexer from BITLAB_INDEXERS: %s (%s)", ix.Name, ix.Kind)
		}
	}

	// An explicit BITLAB_INDEXERS list is the whole configuration: do not add
	// public defaults on top of it.
	if len(cfg.Indexers) == 0 && env("BITLAB_SEED_INDEXER", "on") != "off" {
		// knaben on first run, then the public extras once each: anime needs its
		// own index, and EZTV/YTS carry TV and movie releases that knaben's
		// single remaining TPB feed does not.
		list, _ := store.ListIndexers()
		if len(list) == 0 {
			if _, err := store.SaveIndexer(Indexer{Kind: "knaben", Name: "knaben", Enabled: true}); err != nil {
				lg.Printf("seed indexer: %v", err)
			}
		}
		for _, def := range []Indexer{
			{Kind: "nyaa", Name: "nyaa"},
			{Kind: "eztv", Name: "eztv"},
			{Kind: "yts", Name: "yts"},
			{Kind: "animetosho", Name: "animetosho"},
			// Torrentio aggregates the trackers the pod cannot reach directly
			// (1337x, TGx, Rutracker, RARBG's cache) and is keyed on imdb ids.
			{Kind: "torrentio", Name: "torrentio"},
			// apibay is TPB's own API: a second, deeper view of the catalogue
			// knaben already serves, worth having for older releases.
			{Kind: "apibay", Name: "apibay"},
		} {
			key := "seeded:" + def.Kind
			if store.GetSetting(key, "") != "" {
				continue
			}
			have := false
			for _, ix := range list {
				if ix.Kind == def.Kind {
					have = true
				}
			}
			if !have {
				def.Enabled = true
				if _, err := store.SaveIndexer(def); err == nil {
					lg.Printf("added %s indexer for wider coverage", def.Name)
				}
			}
			store.SetSetting(key, "1")
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

	// A periodic line beats a metrics stack on a box this size: it is the only
	// history of how the cache and the swarm behaved between deploys. One line
	// shortly after boot gives the baseline.
	go func() {
		logStats := func() {
			st := eng.CacheStats()
			free := "unknown"
			if st.Free >= 0 {
				free = humanBytes(st.Free)
			}
			lg.Printf("stats: torrents=%d cache=%s/%s free=%s entries=%d partial=%s reclaimed=%s",
				eng.TorrentCount(), humanBytes(st.Used), humanBytes(st.Budget), free,
				st.Entries, humanBytes(st.PartialBytes), humanBytes(st.EvictedBytes))
		}
		time.Sleep(15 * time.Second)
		logStats()
		t := time.NewTicker(10 * time.Minute)
		defer t.Stop()
		for range t.C {
			logStats()
		}
	}()

	go func() {
		lg.Printf("bitlab %s listening on %s (data=%s, cache=%s)", version, cfg.Addr, cfg.DataDir, humanBytes(cfg.CacheMaxBytes))
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
		// Always 200: this backs the liveness probe, and restarting the pod
		// never fixes a full disk. The numbers are the diagnosis.
		free := s.engine.DiskFree()
		writeJSON(w, 200, map[string]any{
			"ok":         true,
			"version":    version,
			"uptime":     int64(time.Since(s.start).Seconds()),
			"free_bytes": free,
			"degraded":   free >= 0 && free < healthFreeFloor,
			"torrents":   s.engine.TorrentCount(),
		})
	})
	mux.HandleFunc("GET /readyz", func(w http.ResponseWriter, r *http.Request) {
		// Readiness is where a full disk belongs: stop sending new plays to a
		// pod that cannot write, without killing it.
		free := s.engine.DiskFree()
		degraded := free >= 0 && free < healthFreeFloor
		status := 200
		if degraded {
			status = 503
		}
		writeJSON(w, status, map[string]any{"ready": !degraded, "free_bytes": free})
	})
	mux.HandleFunc("GET /manifest.json", func(w http.ResponseWriter, r *http.Request) {
		httpError(w, 401, "use your personal manifest URL: /k/<key>/manifest.json")
	})
	mux.HandleFunc("GET /", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		// No public landing page: the root is just a door to the console.
		http.Redirect(w, r, "/admin", http.StatusFound)
	})

	// icon (both names: some clients probe /favicon.ico directly)
	mux.HandleFunc("GET /favicon.svg", s.handleFavicon)
	mux.HandleFunc("GET /favicon.ico", s.handleFavicon)

	// admin
	mux.HandleFunc("GET /login", s.handleLoginPage)
	mux.HandleFunc("GET /admin", s.handleAdminPage)
	mux.HandleFunc("GET /admin/", s.handleAdminPage)
	mux.HandleFunc("POST /admin/api/login", s.handleLogin)
	mux.HandleFunc("POST /admin/api/logout", s.handleLogout)
	mux.HandleFunc("GET /admin/api/overview", s.requireAdmin(s.handleOverview))
	mux.HandleFunc("GET /admin/api/keys", s.requireAdmin(s.handleListKeys))
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
