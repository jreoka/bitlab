package main

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
	"time"
)

func TestValidInfohash(t *testing.T) {
	good := "0123456789abcdef0123456789abcdef01234567"
	if !validInfohash(good) {
		t.Fatal("valid hash rejected")
	}
	bad := []string{
		"",
		"0123456789abcdef0123456789abcdef0123456",   // 39
		"0123456789abcdef0123456789abcdef012345678", // 41
		"0123456789ABCDEF0123456789abcdef01234567",  // upper
		"aaaaaaaa&ws=http://169.254.169.254/latest", // injection
		"0123456789abcdef0123456789abcdef012345zz",  // non-hex
		"0123456789abcdef0123456789abcdef0123456\n", // newline
	}
	for _, ih := range bad {
		if validInfohash(ih) {
			t.Errorf("invalid hash accepted: %q", ih)
		}
	}
}

func TestClientIPIgnoresSpoofedFirstHop(t *testing.T) {
	s := &Server{cfg: Config{TrustProxy: true}}
	r := httptest.NewRequest("POST", "/admin/api/login", nil)
	r.Header.Set("X-Forwarded-For", "1.2.3.4, 203.0.113.9")
	if got := s.clientIP(r); got != "203.0.113.9" {
		t.Fatalf("XFF: got %q, want last hop", got)
	}
	r.Header.Set("X-Real-IP", "198.51.100.7")
	if got := s.clientIP(r); got != "198.51.100.7" {
		t.Fatalf("X-Real-IP: got %q", got)
	}
	// Without a trusted proxy the forwarding headers are ignored entirely.
	direct := &Server{}
	r.Header.Set("X-Real-IP", "198.51.100.7")
	if got := direct.clientIP(r); got == "198.51.100.7" {
		t.Fatal("untrusted proxy header accepted")
	}
}

func TestCSRFMiddleware(t *testing.T) {
	s := &Server{cfg: Config{BaseURL: "https://stream.example"}}
	h := s.middleware(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	}))

	req := httptest.NewRequest("POST", "https://stream.example/admin/api/keys", strings.NewReader("{}"))
	req.Header.Set("Origin", "https://evil.example")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != 403 {
		t.Fatalf("cross-origin POST: got %d, want 403", rec.Code)
	}

	req = httptest.NewRequest("POST", "https://stream.example/admin/api/keys", strings.NewReader("{}"))
	req.Header.Set("Origin", "https://stream.example")
	req.Header.Set("Content-Type", "application/json")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("same-origin POST: got %d, want 200", rec.Code)
	}

	req = httptest.NewRequest("POST", "https://stream.example/admin/api/keys", strings.NewReader("{}"))
	req.Header.Set("Sec-Fetch-Site", "cross-site")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != 403 {
		t.Fatalf("cross-site fetch: got %d, want 403", rec.Code)
	}
}

func TestCORSOnlyOnAddon(t *testing.T) {
	s := &Server{cfg: Config{BaseURL: "https://stream.example"}}
	h := s.middleware(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("GET", "https://stream.example/admin/api/keys", nil))
	if rec.Header().Get("Access-Control-Allow-Origin") != "" {
		t.Fatal("admin API advertises CORS")
	}
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("GET", "https://stream.example/k/bit_x/manifest.json", nil))
	if rec.Header().Get("Access-Control-Allow-Origin") != "*" {
		t.Fatal("addon surface is missing CORS")
	}
}

func TestTOTPCodeIsSingleUse(t *testing.T) {
	st := newTestStore(t)
	s := &Server{store: st}
	secret := newTOTPSecret()
	st.SetSetting("totp_secret", secret)
	code, err := totpCode(secret, time.Now())
	if err != nil {
		t.Fatal(err)
	}
	if !s.checkSecondFactor(code) {
		t.Fatal("first use rejected")
	}
	if s.checkSecondFactor(code) {
		t.Fatal("replayed code accepted")
	}
}

func TestPlayTokenBindsKeyTorrentAndEpisode(t *testing.T) {
	s := &Server{sessionKey: []byte("secret")}
	ih := "0123456789abcdef0123456789abcdef01234567"
	tk, exp := s.signedPlayToken("key1", ih, 2, 5)
	req := httptest.NewRequest("GET", "/x?tk="+tk+"&exp="+strconv.FormatInt(exp, 10), nil)
	if !s.validPlayToken(req, "key1", ih, 2, 5) {
		t.Fatal("valid token rejected")
	}
	if s.validPlayToken(req, "key2", ih, 2, 5) {
		t.Fatal("token accepted for another key")
	}
	if s.validPlayToken(req, "key1", ih, 2, 6) {
		t.Fatal("token accepted for another episode")
	}
	expired := httptest.NewRequest("GET", "/x?tk="+tk+"&exp=1", nil)
	if s.validPlayToken(expired, "key1", ih, 2, 5) {
		t.Fatal("expired token accepted")
	}
}

func TestTrackerFilter(t *testing.T) {
	for _, good := range []string{"udp://tracker.opentrackr.org:1337/announce", "https://tracker.example.com/a"} {
		if !validTracker(good) {
			t.Errorf("rejected %q", good)
		}
	}
	for _, bad := range []string{"http://127.0.0.1/a", "udp://10.0.0.1:80/a", "http://localhost/a", "http://metadata.internal/a", "http://prowlarr/a", "file:///etc/passwd"} {
		if validTracker(bad) {
			t.Errorf("accepted %q", bad)
		}
	}
}

func TestGuardedTransportRefusesLoopback(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	}))
	defer srv.Close()
	c := &http.Client{Transport: guardedTransport(false), Timeout: 5 * time.Second}
	if _, err := c.Get(srv.URL); err == nil {
		t.Fatal("guarded transport reached loopback")
	}
}

func TestIndexerErrorRedactsAPIKey(t *testing.T) {
	s := &Server{http: &http.Client{Timeout: 2 * time.Second}}
	ix := Indexer{Kind: "torznab", Name: "x", URL: "http://127.0.0.1:1/api", APIKey: "SUPERSECRET123"}
	_, err := s.searchIndexer(context.Background(), ix, "q", "movie", 0, 0, "")
	if err == nil {
		t.Fatal("expected an error")
	}
	if strings.Contains(err.Error(), "SUPERSECRET123") {
		t.Fatalf("API key leaked: %v", err)
	}
}

func TestOutboundURLRejectsPrivate(t *testing.T) {
	for _, raw := range []string{
		"http://127.0.0.1:8090/admin",
		"http://169.254.169.254/latest/meta-data",
		"http://10.0.0.1/",
		"file:///etc/passwd",
		"gopher://example.com/",
	} {
		if err := checkOutboundURL(raw); err == nil {
			t.Errorf("accepted %q", raw)
		}
	}
	if err := checkOutboundURL("https://torrentio.strem.fun/x"); err != nil {
		t.Errorf("public https rejected: %v", err)
	}
}
