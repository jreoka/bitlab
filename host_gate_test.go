package main

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

// The host gate: with BITLAB_BASE_URL set, only that host may access the
// app. Node addresses and platform subdomains get a 403; the probe
// endpoints stay reachable by any host so healthchecks keep working.
func TestHostGate(t *testing.T) {
	s := &Server{cfg: Config{BaseURL: "https://stream.example.com"}}
	h := s.middleware(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	}))
	cases := []struct {
		host, path string
		want       int
	}{
		{"stream.example.com", "/", 200},
		{"STREAM.example.com", "/", 200},
		{"stream.example.com:443", "/", 200},
		{"93.19.135.116:34222", "/", 403},
		{"dillbitlab.app.runonflux.io", "/", 403},
		{"stream.example.com.evil.com", "/", 403},
		{"127.0.0.1:8090", "/healthz", 200},
		{"127.0.0.1:8090", "/readyz", 200},
		{"127.0.0.1:8090", "/admin", 403},
	}
	for _, c := range cases {
		req := httptest.NewRequest("GET", "http://"+c.host+c.path, nil)
		req.Host = c.host
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		if rec.Code != c.want {
			t.Errorf("host %q path %q: got %d, want %d", c.host, c.path, rec.Code, c.want)
		}
	}
}

func TestHostGateOffWithoutBaseURL(t *testing.T) {
	s := &Server{cfg: Config{}}
	h := s.middleware(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	}))
	req := httptest.NewRequest("GET", "http://anything.example/", nil)
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("no base url: got %d, want 200", rec.Code)
	}
}
