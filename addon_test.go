package main

import (
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
)

func newTestServer(t *testing.T) (*Server, *http.ServeMux) {
	t.Helper()
	st, err := OpenStore(filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("store: %v", err)
	}
	t.Cleanup(func() { st.Close() })
	s := &Server{store: st, cfg: Config{AdminPassword: "pw"}, sessionKey: []byte("secret")}
	mux := http.NewServeMux()
	mux.HandleFunc("GET /k/{token}/manifest.json", s.handleManifest)
	mux.HandleFunc("GET /k/{token}/play/{infohash}", s.handlePlay)
	return s, mux
}

func TestManifestLifecycle(t *testing.T) {
	s, mux := newTestServer(t)
	k, err := s.store.CreateKey("living room", "tv", 0)
	if err != nil {
		t.Fatal(err)
	}

	get := func(path string) *httptest.ResponseRecorder {
		rec := httptest.NewRecorder()
		mux.ServeHTTP(rec, httptest.NewRequest("GET", path, nil))
		return rec
	}

	rec := get("/k/" + k.Token + "/manifest.json")
	if rec.Code != 200 {
		t.Fatalf("active key: got %d", rec.Code)
	}
	var m Manifest
	if err := jsonUnmarshal(rec.Body.Bytes(), &m); err != nil {
		t.Fatalf("manifest json: %v", err)
	}
	if m.ID != "org.marlin."+k.ID || len(m.Resources) != 1 || m.Resources[0] != "stream" {
		t.Fatalf("bad manifest: %+v", m)
	}

	if err := s.store.SetKeyRevoked(k.ID, true); err != nil {
		t.Fatal(err)
	}
	if rec := get("/k/" + k.Token + "/manifest.json"); rec.Code != http.StatusGone {
		t.Fatalf("revoked key: want 410, got %d", rec.Code)
	}
	if rec := get("/k/" + k.Token + "/play/" + "0123456789abcdef0123456789abcdef01234567"); rec.Code != http.StatusGone {
		t.Fatalf("revoked play: want 410, got %d", rec.Code)
	}

	// Restoring puts the same URL back in service — no reinstall on the player.
	if err := s.store.SetKeyRevoked(k.ID, false); err != nil {
		t.Fatal(err)
	}
	if rec := get("/k/" + k.Token + "/manifest.json"); rec.Code != 200 {
		t.Fatalf("restored key: want 200, got %d", rec.Code)
	}

	if err := s.store.DeleteKey(k.ID); err != nil {
		t.Fatal(err)
	}
	if rec := get("/k/" + k.Token + "/manifest.json"); rec.Code != http.StatusUnauthorized {
		t.Fatalf("deleted key: want 401, got %d", rec.Code)
	}
}

func TestUnknownAndExpiredKeys(t *testing.T) {
	s, mux := newTestServer(t)
	rec := httptest.NewRecorder()
	mux.ServeHTTP(rec, httptest.NewRequest("GET", "/k/mln_nope/manifest.json", nil))
	if rec.Code != 401 {
		t.Fatalf("unknown key: want 401, got %d", rec.Code)
	}

	k, _ := s.store.CreateKey("expiring", "", 0)
	s.store.db.Exec(`UPDATE keys SET expires_at=? WHERE id=?`, 1, k.ID)
	rec = httptest.NewRecorder()
	mux.ServeHTTP(rec, httptest.NewRequest("GET", "/k/"+k.Token+"/manifest.json", nil))
	if rec.Code != http.StatusGone {
		t.Fatalf("expired key: want 410, got %d", rec.Code)
	}
}

func TestParseMediaID(t *testing.T) {
	cases := []struct {
		in         string
		base       string
		season, ep int
	}{
		{"tt0111161", "tt0111161", 0, 0},
		{"tt0944947:1:1", "tt0944947", 1, 1},
		{"tt0944947:4:10.json", "tt0944947", 4, 10},
	}
	for _, c := range cases {
		b, s, e := parseMediaID(c.in)
		if b != c.base || s != c.season || e != c.ep {
			t.Errorf("parseMediaID(%q) = %q,%d,%d", c.in, b, s, e)
		}
	}
}

func TestReleaseLabel(t *testing.T) {
	cases := map[string]string{
		"Movie.2020.2160p.BluRay.REMUX.x265": "2160p",
		"Movie.2020.1080p.WEB-DL.x264":       "1080p",
		"Movie.2020.720p.HDTV":               "720p",
		"Movie.CAM.rip":                      "SD",
	}
	for title, want := range cases {
		if _, q := releaseLabel(title); q != want {
			t.Errorf("releaseLabel(%q) quality = %q, want %q", title, q, want)
		}
	}
}

func TestSessionSigning(t *testing.T) {
	s := &Server{sessionKey: []byte("k")}
	v := s.signSession(4102444800)
	r := httptest.NewRequest("GET", "/admin", nil)
	r.AddCookie(&http.Cookie{Name: "marlin_session", Value: v})
	if !s.validSession(r) {
		t.Fatal("valid session rejected")
	}
	r2 := httptest.NewRequest("GET", "/admin", nil)
	r2.AddCookie(&http.Cookie{Name: "marlin_session", Value: v + "x"})
	if s.validSession(r2) {
		t.Fatal("tampered session accepted")
	}
	r3 := httptest.NewRequest("GET", "/admin", nil)
	r3.AddCookie(&http.Cookie{Name: "marlin_session", Value: s.signSession(1000)})
	if s.validSession(r3) {
		t.Fatal("expired session accepted")
	}
}
