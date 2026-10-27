package main

import (
	"bytes"
	"encoding/hex"
	"io"
	"net"
	"net/http"
	"testing"
	"time"

	"github.com/anacrolix/torrent"
)

// End to end over one shared port: a real HTTP server answers on it, and a
// real torrent client behind the mux answers a raw BitTorrent handshake on
// the very same port.
func TestSharePortEndToEnd(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	m := newPortMux(ln)
	go m.Serve()
	defer m.Close()

	httpSrv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte("ok"))
	})}
	go httpSrv.Serve(m.HTTPListener())
	defer httpSrv.Close()

	cc := torrent.NewDefaultClientConfig()
	cc.DataDir = t.TempDir()
	cc.ListenPort = 0
	cc.NoDHT = true // keep the test hermetic; DHT is not what the mux moves
	cl, err := torrent.NewClient(cc)
	if err != nil {
		t.Fatal(err)
	}
	defer cl.Close()
	cl.AddListener(m.TorrentListener())

	ihHex := "0123456789abcdef0123456789abcdef01234567"
	ihBytes, err := hex.DecodeString(ihHex)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := cl.AddMagnet("magnet:?xt=urn:btih:" + ihHex); err != nil {
		t.Fatal(err)
	}

	addr := ln.Addr().String()

	// The web side answers on the shared port.
	resp, err := http.Get("http://" + addr + "/healthz")
	if err != nil {
		t.Fatalf("http over shared port: %v", err)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if resp.StatusCode != 200 || string(body) != "ok" {
		t.Fatalf("http over shared port: got %d %q", resp.StatusCode, body)
	}

	// A peer handshake over the same port reaches the torrent client, which
	// answers with its own handshake for the same infohash.
	c, err := net.DialTimeout("tcp", addr, 3*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	hs := make([]byte, 0, 68)
	hs = append(hs, byte(len(btProtocolString)))
	hs = append(hs, btProtocolString...)
	hs = append(hs, make([]byte, 8)...)
	hs = append(hs, ihBytes...)
	hs = append(hs, bytes.Repeat([]byte{0x42}, 20)...)
	if _, err := c.Write(hs); err != nil {
		t.Fatal(err)
	}
	_ = c.SetReadDeadline(time.Now().Add(10 * time.Second))
	reply := make([]byte, 68)
	if _, err := io.ReadFull(c, reply); err != nil {
		t.Fatalf("no handshake answer through the mux: %v", err)
	}
	if reply[0] != byte(len(btProtocolString)) || string(reply[1:20]) != btProtocolString {
		t.Fatalf("bad handshake prefix: %q", reply[:20])
	}
	if !bytes.Equal(reply[28:48], ihBytes) {
		t.Fatalf("handshake infohash = %x, want %x", reply[28:48], ihBytes)
	}
}
