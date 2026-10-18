package main

import (
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
	"time"
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

	if err := s.store.SetKeyStatus(k.ID, "paused"); err != nil {
		t.Fatal(err)
	}
	if rec := get("/k/" + k.Token + "/manifest.json"); rec.Code != http.StatusForbidden {
		t.Fatalf("paused key: want 403, got %d", rec.Code)
	}
	if rec := get("/k/" + k.Token + "/play/" + "0123456789abcdef0123456789abcdef01234567"); rec.Code != http.StatusForbidden {
		t.Fatalf("paused play: want 403, got %d", rec.Code)
	}

	// Resuming puts the same URL back in service — no reinstall on the player.
	if err := s.store.SetKeyStatus(k.ID, "active"); err != nil {
		t.Fatal(err)
	}
	if rec := get("/k/" + k.Token + "/manifest.json"); rec.Code != 200 {
		t.Fatalf("resumed key: want 200, got %d", rec.Code)
	}

	// Revoke deletes: the URL and the row both go.
	if err := s.store.DeleteKey(k.ID); err != nil {
		t.Fatal(err)
	}
	if rec := get("/k/" + k.Token + "/manifest.json"); rec.Code != http.StatusUnauthorized {
		t.Fatalf("revoked key: want 401, got %d", rec.Code)
	}
	keys, _ := s.store.ListKeys()
	if len(keys) != 0 {
		t.Fatalf("revoke should delete the row, %d left", len(keys))
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
	// Using an expired key deletes it, not just refuses it.
	if keys, _ := s.store.ListKeys(); len(keys) != 0 {
		t.Fatalf("expired key should be purged on use, %d rows left", len(keys))
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

func TestSessionCookieAndRevocation(t *testing.T) {
	st := newTestStore(t)
	s := &Server{store: st, sessionKey: []byte("k")}
	sess, err := st.CreateSession("test", "127.0.0.1", "ua", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	withCookie := func(v string) *http.Request {
		r := httptest.NewRequest("GET", "/admin", nil)
		r.AddCookie(&http.Cookie{Name: "marlin_session", Value: v})
		return r
	}
	if _, ok := s.currentSession(withCookie(sess.ID + "." + s.sessionMAC(sess.ID))); !ok {
		t.Fatal("valid session rejected")
	}
	if _, ok := s.currentSession(withCookie(sess.ID + "." + "deadbeef")); ok {
		t.Fatal("tampered signature accepted")
	}
	if _, ok := s.currentSession(withCookie("no-dot-here")); ok {
		t.Fatal("malformed cookie accepted")
	}
	// Server-side revocation is the point of tracked sessions.
	st.DeleteSession(sess.ID)
	if _, ok := s.currentSession(withCookie(sess.ID + "." + s.sessionMAC(sess.ID))); ok {
		t.Fatal("revoked session still valid")
	}
}
