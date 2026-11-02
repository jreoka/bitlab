package main

import (
	"crypto/rand"
	"encoding/base64"
	"io"
	"log"
	"net"
	"net/http"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func genKey(t *testing.T) string {
	t.Helper()
	b := make([]byte, 32)
	if _, err := io.ReadFull(rand.Reader, b); err != nil {
		t.Fatal(err)
	}
	return base64.StdEncoding.EncodeToString(b)
}

func TestParseWireGuardConfigValid(t *testing.T) {
	privKey := genKey(t)
	pubKey := genKey(t)
	psk := genKey(t)

	raw := `
[Interface]
# Local private key
PrivateKey = ` + privKey + `
Address = 10.2.0.2/32, fd00::2/128
DNS = 1.1.1.1, 8.8.8.8
MTU = 1380
ListenPort = 51820

[Peer]
PublicKey = ` + pubKey + `
PresharedKey = ` + psk + `
Endpoint = 127.0.0.1:51820
AllowedIPs = 0.0.0.0/0, ::/0
PersistentKeepalive = 25
`

	cfg, err := ParseWireGuardConfig(raw)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	if cfg.MTU != 1380 {
		t.Errorf("expected MTU 1380, got %d", cfg.MTU)
	}
	if cfg.ListenPort != 51820 {
		t.Errorf("expected ListenPort 51820, got %d", cfg.ListenPort)
	}
	if len(cfg.Addresses) != 2 {
		t.Fatalf("expected 2 addresses, got %d", len(cfg.Addresses))
	}
	if cfg.Addresses[0] != netip.MustParseAddr("10.2.0.2") {
		t.Errorf("expected 10.2.0.2, got %v", cfg.Addresses[0])
	}
	if cfg.Addresses[1] != netip.MustParseAddr("fd00::2") {
		t.Errorf("expected fd00::2, got %v", cfg.Addresses[1])
	}
	if len(cfg.DNS) != 2 {
		t.Fatalf("expected 2 DNS servers, got %d", len(cfg.DNS))
	}
	if len(cfg.Peers) != 1 {
		t.Fatalf("expected 1 peer, got %d", len(cfg.Peers))
	}
	peer := cfg.Peers[0]
	if peer.Endpoint != "127.0.0.1:51820" {
		t.Errorf("expected endpoint 127.0.0.1:51820, got %s", peer.Endpoint)
	}
	if peer.PersistentKeepalive != 25 {
		t.Errorf("expected keepalive 25, got %d", peer.PersistentKeepalive)
	}
	if len(peer.AllowedIPs) != 2 {
		t.Errorf("expected 2 allowed IPs, got %d", len(peer.AllowedIPs))
	}

	uapi := cfg.ToUAPI()
	if !strings.Contains(uapi, "private_key=") {
		t.Errorf("UAPI missing private_key: %s", uapi)
	}
	if !strings.Contains(uapi, "public_key=") {
		t.Errorf("UAPI missing public_key: %s", uapi)
	}
	if !strings.Contains(uapi, "preshared_key=") {
		t.Errorf("UAPI missing preshared_key: %s", uapi)
	}
	if !strings.Contains(uapi, "endpoint=127.0.0.1:51820") {
		t.Errorf("UAPI missing endpoint: %s", uapi)
	}
	if !strings.Contains(uapi, "allowed_ip=0.0.0.0/0") {
		t.Errorf("UAPI missing allowed_ip: %s", uapi)
	}
}

func TestParseWireGuardConfigErrors(t *testing.T) {
	privKey := genKey(t)
	pubKey := genKey(t)

	// Missing PrivateKey
	_, err := ParseWireGuardConfig(`
[Interface]
Address = 10.0.0.1/32
[Peer]
PublicKey = ` + pubKey + `
Endpoint = 127.0.0.1:51820
`)
	if err == nil {
		t.Fatal("expected error for missing PrivateKey")
	}

	// Missing Address
	_, err = ParseWireGuardConfig(`
[Interface]
PrivateKey = ` + privKey + `
[Peer]
PublicKey = ` + pubKey + `
Endpoint = 127.0.0.1:51820
`)
	if err == nil {
		t.Fatal("expected error for missing Address")
	}

	// Invalid Base64 Key
	_, err = ParseWireGuardConfig(`
[Interface]
PrivateKey = NotValidBase64!
Address = 10.0.0.1/32
`)
	if err == nil {
		t.Fatal("expected error for invalid PrivateKey base64")
	}

	// Invalid Key Length
	_, err = ParseWireGuardConfig(`
[Interface]
PrivateKey = ` + base64.StdEncoding.EncodeToString([]byte("short-key")) + `
Address = 10.0.0.1/32
`)
	if err == nil {
		t.Fatal("expected error for short key")
	}
}

func TestLoadWireGuardConf(t *testing.T) {
	tempDir := t.TempDir()
	confPath := filepath.Join(tempDir, "wg0.conf")
	sample := "[Interface]\nPrivateKey = " + genKey(t) + "\nAddress = 10.0.0.2/32\n"
	if err := os.WriteFile(confPath, []byte(sample), 0o600); err != nil {
		t.Fatal(err)
	}

	// 1. Load by explicit path
	loaded, err := loadWireGuardConf(confPath, tempDir)
	if err != nil {
		t.Fatalf("load by path: %v", err)
	}
	if strings.TrimSpace(loaded) != strings.TrimSpace(sample) {
		t.Errorf("loaded != sample: got %q, want %q", loaded, sample)
	}

	// 2. Load inline content
	inline, err := loadWireGuardConf(sample, tempDir)
	if err != nil {
		t.Fatalf("load inline: %v", err)
	}
	if strings.TrimSpace(inline) != strings.TrimSpace(sample) {
		t.Errorf("inline != sample: got %q, want %q", inline, sample)
	}

	// 3. Auto-load from dataDir wireguard.conf
	dataDir := t.TempDir()
	autoConfPath := filepath.Join(dataDir, "wireguard.conf")
	if err := os.WriteFile(autoConfPath, []byte(sample), 0o600); err != nil {
		t.Fatal(err)
	}
	autoLoaded, err := loadWireGuardConf("", dataDir)
	if err != nil {
		t.Fatalf("auto-load: %v", err)
	}
	if strings.TrimSpace(autoLoaded) != strings.TrimSpace(sample) {
		t.Errorf("autoLoaded != sample: got %q, want %q", autoLoaded, sample)
	}
}

func TestStartWireGuardAndBaseClientConfig(t *testing.T) {
	privKey := genKey(t)
	pubKey := genKey(t)

	raw := `
[Interface]
PrivateKey = ` + privKey + `
Address = 10.99.0.2/32
DNS = 1.1.1.1
MTU = 1420

[Peer]
PublicKey = ` + pubKey + `
Endpoint = 127.0.0.1:58120
AllowedIPs = 0.0.0.0/0
`
	lg := log.New(io.Discard, "", 0)
	tunnel, err := StartWireGuard(raw, lg)
	if err != nil {
		t.Fatalf("StartWireGuard failed: %v", err)
	}
	defer tunnel.Close()

	if len(tunnel.LocalIPs) != 1 || tunnel.LocalIPs[0] != netip.MustParseAddr("10.99.0.2") {
		t.Fatalf("unexpected tunnel local IPs: %v", tunnel.LocalIPs)
	}

	cfg := Config{
		TorrentPort:   51413,
		WireGuardConf: raw,
	}
	cc := baseClientConfig(cfg, tunnel)

	// Verify all traffic is routed through tunnel and host networking is disabled
	if !cc.DisableTCP {
		t.Error("expected DisableTCP = true")
	}
	if !cc.DisableUTP {
		t.Error("expected DisableUTP = true")
	}
	if !cc.NoDHT {
		t.Error("expected NoDHT = true")
	}
	if !cc.NoDefaultPortForwarding {
		t.Error("expected NoDefaultPortForwarding = true")
	}
	if cc.DialForPeerConns {
		t.Error("expected DialForPeerConns = false")
	}
	if cc.ListenPort != 0 {
		t.Errorf("expected ListenPort = 0, got %d", cc.ListenPort)
	}
	if cc.TrackerDialContext == nil {
		t.Error("expected TrackerDialContext to be configured")
	}
	if cc.HTTPDialContext == nil {
		t.Error("expected HTTPDialContext to be configured")
	}
	if cc.TrackerListenPacket == nil {
		t.Error("expected TrackerListenPacket to be configured")
	}
	if cc.MetainfoSourcesClient == nil {
		t.Error("expected MetainfoSourcesClient to be configured")
	}

	// Verify TrackerListenPacket produces a working UDP PacketConn
	pc, err := cc.TrackerListenPacket("udp", ":0")
	if err != nil {
		t.Fatalf("TrackerListenPacket failed: %v", err)
	}
	defer pc.Close()
	if pc.LocalAddr() == nil {
		t.Error("expected non-nil LocalAddr for packet conn")
	}
}

func TestEngineWithWireGuard(t *testing.T) {
	privKey := genKey(t)
	pubKey := genKey(t)

	raw := `
[Interface]
PrivateKey = ` + privKey + `
Address = 10.99.0.5/32
DNS = 1.1.1.1
MTU = 1420

[Peer]
PublicKey = ` + pubKey + `
Endpoint = 127.0.0.1:58120
AllowedIPs = 0.0.0.0/0
`
	tempDir := t.TempDir()
	store, err := OpenStore(filepath.Join(tempDir, "bitlab.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	cfg := Config{
		DataDir:       tempDir,
		TorrentPort:   0,
		WireGuardConf: raw,
	}

	lg := log.New(io.Discard, "", 0)
	eng, err := NewEngine(cfg, store, lg)
	if err != nil {
		t.Fatalf("NewEngine with WireGuard failed: %v", err)
	}
	defer eng.Close()

	if !eng.HasWireGuard() {
		t.Error("expected eng.HasWireGuard() to be true")
	}

	if cnt := eng.TorrentCount(); cnt != 0 {
		t.Errorf("expected 0 torrents, got %d", cnt)
	}
}

func TestWireGuardServerServing(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()

	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})}

	// When WireGuard is active, the HTTP server serves ln directly without closing it.
	errCh := make(chan error, 1)
	go func() {
		errCh <- srv.Serve(ln)
	}()

	client := &http.Client{Timeout: 2 * time.Second}
	resp, err := client.Get("http://" + ln.Addr().String() + "/healthz")
	if err != nil {
		t.Fatalf("failed to make request: %v", err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("expected 200 OK, got %d", resp.StatusCode)
	}

	_ = srv.Close()
	select {
	case err := <-errCh:
		if err != nil && err != http.ErrServerClosed {
			t.Fatalf("unexpected serve error: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("server close timed out")
	}
}
