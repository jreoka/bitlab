package main

import (
	"path/filepath"
	"testing"
	"time"
)

func newTestStore(t *testing.T) *Store {
	t.Helper()
	st, err := OpenStore(filepath.Join(t.TempDir(), "t.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })
	return st
}

func TestTOTPMatchesRFC6238(t *testing.T) {
	// RFC 6238 appendix B, SHA-1, T = 59s. Secret is the ASCII "12345678901234567890".
	secret := "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
	code, err := totpCode(secret, time.Unix(59, 0))
	if err != nil {
		t.Fatal(err)
	}
	if code != "287082" { // 94287082 truncated to 6 digits
		t.Fatalf("totp at T=59 = %q, want 287082", code)
	}
	if _, err := totpCode(secret, time.Unix(1111111109, 0)); err != nil {
		t.Fatal(err)
	}
	if verifyTOTP(secret, "000000") && code != "000000" {
		t.Fatal("wrong code accepted")
	}
}

func TestRecoveryCodesAreSingleUse(t *testing.T) {
	plain, hashes := generateRecoveryCodes(3)
	if len(plain) != 3 || len(hashes) != 3 {
		t.Fatalf("got %d/%d", len(plain), len(hashes))
	}
	s := &Server{store: newTestStore(t), sessionKey: []byte("k")}
	s.saveRecoveryHashes(hashes)
	if !s.checkSecondFactor(plain[1]) {
		t.Fatal("valid recovery code rejected")
	}
	if len(s.recoveryHashes()) != 2 {
		t.Fatalf("used code not consumed: %d left", len(s.recoveryHashes()))
	}
	if s.checkSecondFactor(plain[1]) {
		t.Fatal("recovery code reused")
	}
	if !s.checkSecondFactor(plain[0]) {
		t.Fatal("other code should still work")
	}
}

func TestParseTTL(t *testing.T) {
	day := 24 * time.Hour
	cases := map[string]time.Duration{
		"":         0,
		"never":    0,
		"12h":      12 * time.Hour,
		"7d":       7 * day,
		"3d":       3 * day,
		"2w":       14 * day,
		"30d":      30 * day,
		"3m":       90 * day,
		"2y":       730 * day,
		"1.5y":     547*day + 12*time.Hour,
		" 2 Y ":    730 * day,
		"2 months": 60 * day,
		"1 year":   365 * day,
		"6 mo":     180 * day,
	}
	for in, want := range cases {
		got, err := parseTTL(in)
		if err != nil {
			t.Fatalf("parseTTL(%q): %v", in, err)
		}
		if got != want {
			t.Errorf("parseTTL(%q) = %v, want %v", in, got, want)
		}
	}
	for _, bad := range []string{"banana", "10", "3x", "-5d", "0d"} {
		if _, err := parseTTL(bad); err == nil {
			t.Errorf("parseTTL(%q) should fail", bad)
		}
	}
	if _, err := parseTTL("2020-01-01"); err == nil {
		t.Error("past date should fail")
	}
	d, err := parseTTL("2030-06-01")
	if err != nil || d <= 0 {
		t.Errorf("future date: %v %v", d, err)
	}
}

func TestExpiredKeysArePurged(t *testing.T) {
	st := newTestStore(t)
	k, _ := st.CreateKey("temp", "", 0)
	st.db.Exec(`UPDATE keys SET expires_at=? WHERE id=?`, time.Now().Add(-time.Minute).Unix(), k.ID)
	if _, err := st.KeyByToken(k.Token); err != ErrKeyExpired {
		t.Fatalf("want ErrKeyExpired, got %v", err)
	}
	if n, _ := st.ListKeys(); len(n) != 0 {
		t.Fatalf("expired key not deleted (%d rows)", len(n))
	}

	k2, _ := st.CreateKey("temp2", "", 0)
	st.db.Exec(`UPDATE keys SET expires_at=? WHERE id=?`, time.Now().Add(-time.Hour).Unix(), k2.ID)
	if n := st.DeleteExpiredKeys(); n != 1 {
		t.Fatalf("sweeper removed %d, want 1", n)
	}
}

func TestSessionLifecycle(t *testing.T) {
	st := newTestStore(t)
	sess, err := st.CreateSession("Firefox on Linux", "1.2.3.4", "ua", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	if got, ok := st.GetSession(sess.ID); !ok || got.Name != "Firefox on Linux" {
		t.Fatalf("session lookup failed: %+v %v", got, ok)
	}
	if _, err := st.CreateSession("other", "5.6.7.8", "ua", time.Hour); err != nil {
		t.Fatal(err)
	}
	st.DeleteOtherSessions(sess.ID)
	if list, _ := st.ListSessions(); len(list) != 1 || list[0].ID != sess.ID {
		t.Fatalf("delete-others left %d sessions", len(list))
	}
	st.DeleteSession(sess.ID)
	if _, ok := st.GetSession(sess.ID); ok {
		t.Fatal("deleted session still resolves")
	}
}

func TestSessionName(t *testing.T) {
	cases := map[string]string{
		"Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101 Firefox/128.0":    "Firefox on Linux",
		"Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Safari/605.1.15": "Safari on macOS",
		"curl/8.5.0": "curl",
	}
	for ua, want := range cases {
		if got := sessionName(ua); got != want {
			t.Errorf("sessionName(%q) = %q, want %q", ua, got, want)
		}
	}
}
