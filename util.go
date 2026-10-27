package main

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base32"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// base32 lower-case, no padding: URL-safe without escaping.
var b32 = base32.NewEncoding("abcdefghijklmnopqrstuvwxyz234567").WithPadding(base32.NoPadding)

func randToken(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		panic(err)
	}
	return strings.ToLower(b32.EncodeToString(b))
}

func randHex(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b)
}

// baseURL derives the externally visible origin for addon links.
func (s *Server) baseURL(r *http.Request) string {
	if s.cfg.BaseURL != "" {
		return strings.TrimRight(s.cfg.BaseURL, "/")
	}
	scheme := "http"
	if r.TLS != nil {
		scheme = "https"
	}
	// Without a configured origin, trust only the Host this connection was
	// addressed to; X-Forwarded-* are caller-controlled and would put the
	// manifest key in links pointing at an attacker's host.
	return scheme + "://" + r.Host
}

// readaheadBytes is the live read-ahead size. The console can rewrite it
// while plays are being served, so the live value is atomic; cfg carries
// only the boot value.
func (s *Server) readaheadBytes() int64 {
	if v := s.readahead.Load(); v > 0 {
		return v
	}
	return s.cfg.ReadaheadBytes
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

// decodeJSON bounds request bodies. Every admin mutation used to decode an
// unlimited body, so one client could allocate without limit.
func decodeJSON(w http.ResponseWriter, r *http.Request, dst any) bool {
	r.Body = http.MaxBytesReader(w, r.Body, 1<<20)
	if err := json.NewDecoder(r.Body).Decode(dst); err != nil {
		httpError(w, 400, "bad json")
		return false
	}
	return true
}

// validInfohash accepts the 40-hex form Stremio clients and indexers use. It
// also keeps the value safe to splice into a magnet URI: an unvalidated string
// could add &ws=/&xs= parameters and make the torrent client fetch any URL.
func validInfohash(ih string) bool {
	if len(ih) != 40 {
		return false
	}
	for i := 0; i < len(ih); i++ {
		c := ih[i]
		if (c < '0' || c > '9') && (c < 'a' || c > 'f') {
			return false
		}
	}
	return true
}

// playToken binds a play URL to the key, torrent and episode that the stream
// list minted it for. Without it any manifest key could play any infohash.
func (s *Server) playToken(keyID, ih string, season, ep int, exp int64) string {
	mac := hmac.New(sha256.New, s.sessionKey)
	fmt.Fprintf(mac, "play|%s|%s|%d|%d|%d", keyID, ih, season, ep, exp)
	return base64.RawURLEncoding.EncodeToString(mac.Sum(nil))
}

func (s *Server) signedPlayToken(keyID, ih string, season, ep int) (string, int64) {
	exp := time.Now().Add(24 * time.Hour).Unix()
	return s.playToken(keyID, ih, season, ep, exp), exp
}

func (s *Server) validPlayToken(r *http.Request, keyID, ih string, season, ep int) bool {
	tk := r.URL.Query().Get("tk")
	if tk == "" {
		return false
	}
	exp, err := strconv.ParseInt(r.URL.Query().Get("exp"), 10, 64)
	if err != nil || exp < time.Now().Unix() || exp > time.Now().Add(48*time.Hour).Unix() {
		return false
	}
	want := s.playToken(keyID, ih, season, ep, exp)
	return hmac.Equal([]byte(want), []byte(tk))
}

// validTracker keeps only announce URLs a torrent client should contact.
func validTracker(raw string) bool {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || u.Host == "" {
		return false
	}
	switch strings.ToLower(u.Scheme) {
	case "http", "https", "udp":
	default:
		return false
	}
	host := strings.ToLower(u.Hostname())
	if host == "" || host == "localhost" || strings.HasSuffix(host, ".local") ||
		strings.HasSuffix(host, ".internal") || strings.HasSuffix(host, ".localdomain") {
		return false
	}
	if ip := net.ParseIP(host); ip != nil {
		return !privateAddress(ip)
	}
	// Single-label names are almost always cluster-internal.
	return strings.Contains(host, ".")
}

func httpError(w http.ResponseWriter, code int, msg string) {
	writeJSON(w, code, map[string]string{"error": msg})
}

func humanBytes(n int64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}
	div, exp := int64(unit), 0
	for m := n / unit; m >= unit; m /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %s", float64(n)/float64(div), []string{"KiB", "MiB", "GiB", "TiB"}[exp])
}

func readAllLimit(r io.Reader, n int64) ([]byte, error) {
	return io.ReadAll(io.LimitReader(r, n))
}
