package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const version = "2.10.5"

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

// envIntClamped keeps a typo or a hostile env value from overflowing a
// duration/shift or producing a negative size.
func envIntClamped(k string, def, lo, hi int) int {
	n := envInt(k, def)
	if n < lo {
		return lo
	}
	if n > hi {
		return hi
	}
	return n
}

func loadConfig() Config {
	dataDir := env("BITLAB_DATA", "./data")
	cfg := Config{
		Addr:              env("BITLAB_ADDR", ":8090"),
		DataDir:           dataDir,
		BaseURL:           env("BITLAB_BASE_URL", ""),
		CacheMaxBytes:     int64(envIntClamped("BITLAB_CACHE_GB", 40, 0, 1_000_000)) << 30,
		CacheReserveBytes: int64(envIntClamped("BITLAB_CACHE_RESERVE_GB", 2, 0, 1_000_000)) << 30,
		CacheRetain:       time.Duration(envIntClamped("BITLAB_CACHE_RETAIN_HOURS", 24, 0, 24*365)) * time.Hour,
		CacheGrace:        time.Duration(envIntClamped("BITLAB_CACHE_GRACE_MINUTES", 10, 0, 24*60)) * time.Minute,
		IdleTimeout:       time.Duration(envIntClamped("BITLAB_IDLE_MINUTES", 20, 1, 7*24*60)) * time.Minute,
		InfoTimeout:       time.Duration(envIntClamped("BITLAB_INFO_TIMEOUT", 45, 5, 600)) * time.Second,
		ReadaheadBytes:    int64(envIntClamped("BITLAB_READAHEAD_MB", 64, 1, 1024)) << 20,
		PrebufferMB:       int64(envIntClamped("BITLAB_PREBUFFER_MB", 8, 0, 256)),
		TorrentPort:       envIntClamped("BITLAB_TORRENT_PORT", 0, 0, 65535),
		MaxTorrents:       envIntClamped("BITLAB_MAX_TORRENTS", 40, 1, 500),
		AdminPassword:     env("BITLAB_ADMIN_PASSWORD", ""),
		TrustProxy:        os.Getenv("BITLAB_TRUST_PROXY") == "1",

		// The predictor measures releases nobody clicked, so its cost is
		// bandwidth the operator must be able to bound. Both caps are hard:
		// the loop stops for the rest of the hour when either is hit.
		ProbeEnabled:  env("BITLAB_PROBE_ENABLED", "on") != "off",
		ProbeInterval: time.Duration(envIntClamped("BITLAB_PROBE_INTERVAL", 60, 15, 86400)) * time.Second,
		ProbePerHour:  envIntClamped("BITLAB_PROBE_PER_HOUR", 30, 0, 600),
		ProbeMBHour:   int64(envIntClamped("BITLAB_PROBE_MB_HOUR", 120, 0, 100000)),
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

	if err := os.MkdirAll(cfg.DataDir, 0o700); err != nil {
		lg.Fatalf("data dir: %v", err)
	}
	_ = os.Chmod(cfg.DataDir, 0o700)
	if cfg.AdminPassword != "" {
		// An env password supersedes the bootstrap file; do not leave a second
		// long-lived credential on the volume.
		_ = os.Remove(filepath.Join(cfg.DataDir, "ADMIN_PASSWORD"))
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
		cfg:    cfg,
		store:  store,
		engine: eng,
		http: &http.Client{
			Timeout:   25 * time.Second,
			Transport: guardedTransport(os.Getenv("BITLAB_ALLOW_PRIVATE_INDEXERS") == "1"),
		},
		log:        lg,
		start:      time.Now(),
		sessionKey: []byte(secret),
	}
	s.pred = NewPredictor(s)
	s.pred.Start(context.Background())

	mux := http.NewServeMux()
	s.routes(mux)
	handler := s.middleware(mux)

	srv := &http.Server{
		Addr:              cfg.Addr,
		Handler:           handler,
		ReadHeaderTimeout: 20 * time.Second,
		ReadTimeout:       60 * time.Second,
		IdleTimeout:       120 * time.Second,
		// WriteTimeout stays 0: a movie stream lasts longer than any useful
		// deadline. Slow readers are bounded by the handler's request context.
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
			if pm := s.pred.Stats(); pm.Probes > 0 {
				lg.Printf("predictor: probes=%d hashes=%d pool=%d last_hour=%d/%d mb=%.1f/%d samples=%d mae=%.1fs within2x=%.0f%%",
					pm.Probes, pm.Hashes, pm.Pool, pm.LastHour, pm.Budget, pm.MBLastHour,
					pm.Budget*4, pm.Samples, pm.MAE, pm.Within2x*100)
			}
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
	s.pred.Close()
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
			"ok":       true,
			"degraded": free >= 0 && free < healthFreeFloor,
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
		writeJSON(w, status, map[string]any{"ready": !degraded})
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

// sameOrigin compares an Origin header with the configured public origin (or
// the Host the request arrived on when no origin is pinned).
func (s *Server) sameOrigin(r *http.Request, origin string) bool {
	u, err := url.Parse(origin)
	if err != nil || u.Host == "" {
		return false
	}
	want := r.Host
	wantScheme := "http"
	if r.TLS != nil {
		wantScheme = "https"
	}
	if s.cfg.BaseURL != "" {
		if b, err := url.Parse(s.cfg.BaseURL); err == nil {
			want = b.Host
			wantScheme = b.Scheme
		}
	}
	return strings.EqualFold(u.Host, want) && strings.EqualFold(u.Scheme, wantScheme)
}

func (s *Server) middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// CORS is for the addon surface only. The admin API is cookie-authenticated
		// and must not advertise cross-origin access.
		if strings.HasPrefix(r.URL.Path, "/k/") {
			w.Header().Set("Access-Control-Allow-Origin", "*")
			w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Range")
			w.Header().Set("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS")
			w.Header().Set("Access-Control-Expose-Headers", "Content-Length, Content-Range, Accept-Ranges")
		}
		w.Header().Set("X-Content-Type-Options", "nosniff")
		w.Header().Set("Referrer-Policy", "no-referrer")
		w.Header().Set("X-Frame-Options", "DENY")
		w.Header().Set("Cross-Origin-Opener-Policy", "same-origin")
		w.Header().Set("Permissions-Policy", "camera=(), microphone=(), geolocation=()")
		if s.cfg.BaseURL != "" && strings.HasPrefix(s.cfg.BaseURL, "https://") {
			w.Header().Set("Strict-Transport-Security", "max-age=31536000; includeSubDomains")
		}
		if r.Method == http.MethodOptions {
			w.WriteHeader(204)
			return
		}
		// SameSite=Lax does not stop a same-site sibling subdomain, so mutating
		// requests must come from this origin. Requests without either header
		// (curl, scripts) are still allowed; browsers always send Origin on POST.
		switch r.Method {
		case http.MethodPost, http.MethodPut, http.MethodPatch, http.MethodDelete:
			if site := r.Header.Get("Sec-Fetch-Site"); site != "" && site != "same-origin" && site != "none" {
				httpError(w, 403, "cross-site request refused")
				return
			}
			if origin := r.Header.Get("Origin"); origin != "" && !s.sameOrigin(r, origin) {
				httpError(w, 403, "origin does not match this site")
				return
			}
			if r.Method == http.MethodPost && strings.HasPrefix(r.URL.Path, "/admin/api/") {
				// A cross-site HTML form cannot set this content type without a
				// CORS preflight, which this server never grants to other origins.
				if ct := r.Header.Get("Content-Type"); !strings.HasPrefix(ct, "application/json") {
					httpError(w, 415, "application/json required")
					return
				}
			}
		}
		next.ServeHTTP(w, r)
	})
}
