package main

import (
	"crypto/hmac"
	crand "crypto/rand"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base32"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math"
	"math/big"
	"net/http"
	"strconv"
	"strings"
	"time"

	qrcode "github.com/skip2/go-qrcode"
)

const (
	totpPeriod = 30 // seconds
	totpDigits = 6
	totpWindow = 1 // accept ±1 step for clock drift
)

var b32std = base32.StdEncoding.WithPadding(base32.NoPadding)

// --- TOTP (RFC 6238, SHA-1) ---

func totpCode(secret string, t time.Time) (string, error) {
	key, err := b32std.DecodeString(strings.ToUpper(strings.TrimRight(secret, "=")))
	if err != nil {
		return "", err
	}
	counter := uint64(t.Unix() / totpPeriod)
	var msg [8]byte
	binary.BigEndian.PutUint64(msg[:], counter)
	mac := hmac.New(sha1.New, key)
	mac.Write(msg[:])
	sum := mac.Sum(nil)
	off := sum[len(sum)-1] & 0x0f
	bin := (uint32(sum[off])&0x7f)<<24 | uint32(sum[off+1])<<16 | uint32(sum[off+2])<<8 | uint32(sum[off+3])
	code := bin % 1_000_000
	return fmt.Sprintf("%06d", code), nil
}

func verifyTOTPStep(secret, code string) (int64, bool) {
	code = strings.TrimSpace(code)
	if len(code) != totpDigits {
		return 0, false
	}
	now := time.Now()
	for i := -totpWindow; i <= totpWindow; i++ {
		want, err := totpCode(secret, now.Add(time.Duration(i)*totpPeriod*time.Second))
		if err != nil {
			return 0, false
		}
		if subtle.ConstantTimeCompare([]byte(want), []byte(code)) == 1 {
			return now.Unix()/totpPeriod + int64(i), true
		}
	}
	return 0, false
}

func verifyTOTP(secret, code string) bool {
	_, ok := verifyTOTPStep(secret, code)
	return ok
}

func newTOTPSecret() string {
	b := make([]byte, 20)
	if _, err := crand.Read(b); err != nil {
		panic(err)
	}
	return b32std.EncodeToString(b)
}

func otpauthURI(secret, issuer, account string) string {
	label := issuer + ":" + account
	return fmt.Sprintf("otpauth://totp/%s?secret=%s&issuer=%s&period=%d&digits=%d&algorithm=SHA1",
		urlEscape(label), secret, urlEscape(issuer), totpPeriod, totpDigits)
}

func urlEscape(s string) string {
	r := strings.NewReplacer(" ", "%20", ":", "%3A", "/", "%2F", "@", "%40", "?", "%3F", "&", "%26", "=", "%3D", "#", "%23")
	return r.Replace(s)
}

// qrDataURI renders a QR code as an inline PNG data URI.
func qrDataURI(text string) (string, error) {
	png, err := qrcode.Encode(text, qrcode.Medium, 320)
	if err != nil {
		return "", err
	}
	return "data:image/png;base64," + base64.StdEncoding.EncodeToString(png), nil
}

// --- recovery codes ---

const recoveryAlphabet = "ABCDEFGHJKMNPQRSTUVWXYZ23456789" // no 0/1/I/L/O

func hashRecoveryCode(code string) string {
	norm := strings.ToUpper(strings.Map(func(r rune) rune {
		if (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') {
			return r
		}
		return -1
	}, code))
	sum := sha256.Sum256([]byte(norm))
	return hex.EncodeToString(sum[:])
}

func generateRecoveryCodes(n int) (plain, hashes []string) {
	for i := 0; i < n; i++ {
		var b strings.Builder
		for j := 0; j < 8; j++ {
			n, _ := crand.Int(crand.Reader, big.NewInt(int64(len(recoveryAlphabet))))
			b.WriteByte(recoveryAlphabet[n.Int64()])
		}
		code := b.String()
		plain = append(plain, code[:4]+"-"+code[4:])
		hashes = append(hashes, hashRecoveryCode(code))
	}
	return
}

// --- 2FA state ---

func (s *Server) twoFactorEnabled() bool {
	return s.store.GetSetting("totp_enabled", "") == "1" && s.store.GetSetting("totp_secret", "") != ""
}

func (s *Server) recoveryHashes() []string {
	var out []string
	json.Unmarshal([]byte(s.store.GetSetting("totp_recovery", "[]")), &out)
	return out
}

func (s *Server) saveRecoveryHashes(h []string) {
	b, _ := json.Marshal(h)
	s.store.SetSetting("totp_recovery", string(b))
}

func (s *Server) checkPassword(pw string) bool {
	return subtle.ConstantTimeCompare([]byte(pw), []byte(s.cfg.AdminPassword)) == 1
}

// checkSecondFactor accepts a TOTP code or consumes a recovery code.
func (s *Server) checkSecondFactor(code string) bool {
	code = strings.TrimSpace(code)
	if code == "" {
		return false
	}
	if step, ok := verifyTOTPStep(s.store.GetSetting("totp_secret", ""), code); ok {
		// A code is one-use: reject a step that was already accepted (RFC 6238
		// §5.2), otherwise a phished code works for the whole 90-second window.
		last, _ := strconv.ParseInt(s.store.GetSetting("totp_last_step", "0"), 10, 64)
		if step <= last {
			return false
		}
		s.store.SetSetting("totp_last_step", strconv.FormatInt(step, 10))
		return true
	}
	norm := strings.ToUpper(strings.TrimSpace(code))
	if len(strings.ReplaceAll(norm, "-", "")) < 8 {
		return false
	}
	want := hashRecoveryCode(norm)
	hashes := s.recoveryHashes()
	for i, h := range hashes {
		if subtle.ConstantTimeCompare([]byte(h), []byte(want)) == 1 {
			s.saveRecoveryHashes(append(hashes[:i:i], hashes[i+1:]...))
			return true
		}
	}
	return false
}

// --- session helpers ---

func sessionName(ua string) string {
	l := strings.ToLower(ua)
	browser := "Browser"
	switch {
	case strings.Contains(l, "firefox"):
		browser = "Firefox"
	case strings.Contains(l, "edg/"):
		browser = "Edge"
	case strings.Contains(l, "chrome"):
		browser = "Chrome"
	case strings.Contains(l, "safari"):
		browser = "Safari"
	case strings.Contains(l, "curl"):
		browser = "curl"
	}
	os := ""
	switch {
	case strings.Contains(l, "iphone"), strings.Contains(l, "ipad"):
		os = "iOS"
	case strings.Contains(l, "android"):
		os = "Android"
	case strings.Contains(l, "mac os"):
		os = "macOS"
	case strings.Contains(l, "windows"):
		os = "Windows"
	case strings.Contains(l, "linux"):
		os = "Linux"
	}
	if os == "" {
		return browser
	}
	return browser + " on " + os
}

func (s *Server) issueCookie(w http.ResponseWriter, r *http.Request, sess Session) {
	exp := time.Unix(sess.ExpiresAt, 0)
	http.SetCookie(w, &http.Cookie{
		Name:     "bitlab_session",
		Value:    sess.ID + "." + s.sessionMAC(sess.ID),
		Path:     "/",
		HttpOnly: true,
		SameSite: http.SameSiteLaxMode,
		Secure:   r.TLS != nil || strings.HasPrefix(s.cfg.BaseURL, "https://"),
		Expires:  exp,
	})
}

func (s *Server) sessionMAC(id string) string {
	mac := hmac.New(sha256.New, s.sessionKey)
	mac.Write([]byte(id))
	return hex.EncodeToString(mac.Sum(nil))
}

// currentSession resolves the cookie to a live, server-side session record.
func (s *Server) currentSession(r *http.Request) (Session, bool) {
	c, err := r.Cookie("bitlab_session")
	if err != nil {
		return Session{}, false
	}
	id, sig, ok := strings.Cut(c.Value, ".")
	if !ok || !hmac.Equal([]byte(s.sessionMAC(id)), []byte(sig)) {
		return Session{}, false
	}
	sess, ok := s.store.GetSession(id)
	if !ok {
		return Session{}, false
	}
	s.store.TouchSession(id)
	return sess, true
}

// --- 2FA + session endpoints ---

func (s *Server) handleSecurity(w http.ResponseWriter, r *http.Request) {
	sessions, _ := s.store.ListSessions()
	cur, _ := s.currentSession(r)
	writeJSON(w, 200, map[string]any{
		"two_factor":    s.twoFactorEnabled(),
		"recovery_left": len(s.recoveryHashes()),
		"sessions":      sessions,
		"current":       cur.ID,
	})
}

func (s *Server) handle2FABegin(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Password string `json:"password"`
	}
	if !decodeJSON(w, r, &body) {
		return
	}
	ip := s.clientIP(r)
	if loginWindow.blocked(ip) {
		httpError(w, 429, "too many attempts, try again later")
		return
	}
	if !s.checkPassword(body.Password) {
		loginWindow.fail(ip)
		httpError(w, 401, "wrong password")
		return
	}
	loginWindow.reset(ip)
	if s.twoFactorEnabled() {
		httpError(w, 409, "two-factor is already enabled")
		return
	}
	secret := newTOTPSecret()
	s.store.SetSetting("totp_pending", secret)
	uri := otpauthURI(secret, "Bitlab", "admin")
	qr, err := qrDataURI(uri)
	if err != nil {
		httpError(w, 500, err.Error())
		return
	}
	writeJSON(w, 200, map[string]any{"secret": secret, "otpauth": uri, "qr": qr})
}

func (s *Server) handle2FAEnable(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Code     string `json:"code"`
		Password string `json:"password"`
	}
	if !decodeJSON(w, r, &body) {
		return
	}
	ip := s.clientIP(r)
	if loginWindow.blocked(ip) {
		httpError(w, 429, "too many attempts, try again later")
		return
	}
	if !s.checkPassword(body.Password) {
		loginWindow.fail(ip)
		httpError(w, 401, "wrong password")
		return
	}
	loginWindow.reset(ip)
	pending := s.store.GetSetting("totp_pending", "")
	if pending == "" {
		httpError(w, 400, "start setup first")
		return
	}
	step, ok := verifyTOTPStep(pending, body.Code)
	if !ok {
		httpError(w, 400, "that code did not verify")
		return
	}
	plain, hashes := generateRecoveryCodes(10)
	s.store.SetSetting("totp_secret", pending)
	s.store.SetSetting("totp_last_step", strconv.FormatInt(step, 10))
	s.store.SetSetting("totp_enabled", "1")
	s.store.DeleteSetting("totp_pending")
	s.saveRecoveryHashes(hashes)
	if cur, ok := s.currentSession(r); ok {
		s.store.DeleteOtherSessions(cur.ID)
	}
	s.log.Printf("two-factor enabled")
	writeJSON(w, 200, map[string]any{"ok": true, "recovery_codes": plain})
}

func (s *Server) handle2FADisable(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Password string `json:"password"`
		Code     string `json:"code"`
	}
	if !decodeJSON(w, r, &body) {
		return
	}
	ip := s.clientIP(r)
	if loginWindow.blocked(ip) {
		httpError(w, 429, "too many attempts, try again later")
		return
	}
	if !s.checkPassword(body.Password) {
		loginWindow.fail(ip)
		httpError(w, 401, "wrong password")
		return
	}
	if !s.checkSecondFactor(body.Code) {
		loginWindow.fail(ip)
		httpError(w, 401, "a current two-factor code or recovery code is required")
		return
	}
	loginWindow.reset(ip)
	s.store.DeleteSetting("totp_secret")
	s.store.DeleteSetting("totp_enabled")
	s.store.DeleteSetting("totp_pending")
	s.saveRecoveryHashes(nil)
	s.store.SetSetting("totp_last_step", "0")
	if cur, ok := s.currentSession(r); ok {
		s.store.DeleteOtherSessions(cur.ID)
	}
	s.log.Printf("two-factor disabled")
	writeJSON(w, 200, map[string]any{"ok": true})
}

func (s *Server) handleDeleteSession(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	if len(id) < 8 {
		httpError(w, 404, "session not found")
		return
	}
	if err := s.store.DeleteSession(id); err != nil {
		httpError(w, 500, err.Error())
		return
	}
	if cur, ok := s.currentSession(r); ok && cur.ID == id {
		http.SetCookie(w, &http.Cookie{Name: "bitlab_session", Value: "", Path: "/", MaxAge: -1})
	}
	s.log.Printf("session %s revoked", id[:8])
	writeJSON(w, 200, map[string]any{"ok": true})
}

func (s *Server) handleRevokeOtherSessions(w http.ResponseWriter, r *http.Request) {
	cur, ok := s.currentSession(r)
	if !ok {
		httpError(w, 401, "not signed in")
		return
	}
	s.store.DeleteOtherSessions(cur.ID)
	writeJSON(w, 200, map[string]any{"ok": true})
}

// --- key expiry parsing ---

// parseTTL accepts a duration ("12h", "3d", "2w", "3m", "2y", "1.5y",
// "hours"/"days"/"months" spelled out) or an absolute date ("2027-06-01").
// Bare m means months — this is a key lifetime, not a timeout.
func parseTTL(v string) (time.Duration, error) {
	raw := strings.ToLower(strings.TrimSpace(v))
	if raw == "" || raw == "never" || raw == "none" || raw == "-" || raw == "0" {
		return 0, nil
	}
	for _, layout := range []string{"2006-01-02", "2006/01/02", "02-01-2006", "2006-01-02 15:04", time.RFC3339} {
		if d, err := time.Parse(layout, raw); err == nil {
			until := time.Until(d)
			if until <= 0 {
				return 0, fmt.Errorf("that date has already passed")
			}
			return until, nil
		}
	}
	i := 0
	for i < len(raw) && (raw[i] == '.' || (raw[i] >= '0' && raw[i] <= '9')) {
		i++
	}
	num, unit := strings.TrimSpace(raw[:i]), strings.TrimSpace(raw[i:])
	if num == "" || unit == "" {
		return 0, fmt.Errorf("use a duration like 3d, 2w, 3m, 1y or a date like 2027-06-01")
	}
	n, err := strconv.ParseFloat(num, 64)
	if err != nil || n <= 0 {
		return 0, fmt.Errorf("invalid amount %q", num)
	}
	mults := map[string]time.Duration{
		"min": time.Minute, "mins": time.Minute, "minute": time.Minute, "minutes": time.Minute,
		"h": time.Hour, "hr": time.Hour, "hrs": time.Hour, "hour": time.Hour, "hours": time.Hour,
		"d": 24 * time.Hour, "day": 24 * time.Hour, "days": 24 * time.Hour,
		"w": 7 * 24 * time.Hour, "wk": 7 * 24 * time.Hour, "week": 7 * 24 * time.Hour, "weeks": 7 * 24 * time.Hour,
		"m": 30 * 24 * time.Hour, "mo": 30 * 24 * time.Hour, "mon": 30 * 24 * time.Hour,
		"month": 30 * 24 * time.Hour, "months": 30 * 24 * time.Hour,
		"y": 365 * 24 * time.Hour, "yr": 365 * 24 * time.Hour, "year": 365 * 24 * time.Hour, "years": 365 * 24 * time.Hour,
	}
	mult, ok := mults[unit]
	if !ok {
		return 0, fmt.Errorf("unknown unit %q — use h, d, w, m (months) or y", unit)
	}
	secs := n * float64(mult)
	if math.IsNaN(secs) || math.IsInf(secs, 0) || secs <= 0 {
		return 0, fmt.Errorf("invalid duration")
	}
	d := time.Duration(secs)
	if d <= 0 || d > 50*365*24*time.Hour {
		return 0, fmt.Errorf("that is longer than 50 years")
	}
	return d, nil
}
