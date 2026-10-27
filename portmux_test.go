package main

import (
	"bytes"
	"io"
	"net"
	"net/http"
	"testing"
	"time"
)

func startTestMux(t *testing.T) (*portMux, string) {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	m := newPortMux(ln)
	m.timeout = 300 * time.Millisecond
	go m.Serve()
	t.Cleanup(func() { m.Close() })
	return m, ln.Addr().String()
}

func acceptWithin(t *testing.T, l net.Listener, d time.Duration) net.Conn {
	t.Helper()
	type res struct {
		c   net.Conn
		err error
	}
	ch := make(chan res, 1)
	go func() {
		c, err := l.Accept()
		ch <- res{c, err}
	}()
	select {
	case r := <-ch:
		if r.err != nil {
			t.Fatalf("accept: %v", r.err)
		}
		return r.c
	case <-time.After(d):
		t.Fatal("accept timed out")
		return nil
	}
}

func dial(t *testing.T, addr string) net.Conn {
	t.Helper()
	c, err := net.DialTimeout("tcp", addr, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close() })
	return c
}

func TestMuxRoutesHTTP(t *testing.T) {
	m, addr := startTestMux(t)
	c := dial(t, addr)
	req := []byte("GET /admin HTTP/1.1\r\nHost: example\r\n\r\n")
	if _, err := c.Write(req); err != nil {
		t.Fatal(err)
	}
	srv := acceptWithin(t, m.HTTPListener(), 2*time.Second)
	defer srv.Close()
	_ = srv.SetReadDeadline(time.Now().Add(2 * time.Second))
	got := make([]byte, len(req))
	if _, err := io.ReadFull(srv, got); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, req) {
		t.Fatalf("http side got %q, want %q", got, req)
	}
}

func btHandshake() []byte {
	hs := make([]byte, 0, 68)
	hs = append(hs, byte(len(btProtocolString)))
	hs = append(hs, btProtocolString...)
	hs = append(hs, make([]byte, 8)...)                // reserved
	hs = append(hs, bytes.Repeat([]byte{0xAB}, 20)...) // infohash
	hs = append(hs, bytes.Repeat([]byte{0xCD}, 20)...) // peer id
	return hs
}

func TestMuxRoutesBitTorrent(t *testing.T) {
	m, addr := startTestMux(t)
	c := dial(t, addr)
	hs := btHandshake()
	if _, err := c.Write(hs); err != nil {
		t.Fatal(err)
	}
	srv := acceptWithin(t, m.TorrentListener(), 2*time.Second)
	defer srv.Close()
	_ = srv.SetReadDeadline(time.Now().Add(2 * time.Second))
	got := make([]byte, len(hs))
	if _, err := io.ReadFull(srv, got); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, hs) {
		t.Fatalf("torrent side got %x, want %x", got, hs)
	}
	// The peer's real address must survive the mux: the torrent client
	// tracks peers by it.
	if srv.RemoteAddr().String() != c.LocalAddr().String() {
		t.Fatalf("remote addr = %s, want %s", srv.RemoteAddr(), c.LocalAddr())
	}
}

func TestMuxDropsFakeHandshake(t *testing.T) {
	m, addr := startTestMux(t)
	c := dial(t, addr)
	fake := append([]byte{byte(len(btProtocolString))}, []byte("BitTorrent protocoX")...)
	if _, err := c.Write(fake); err != nil {
		t.Fatal(err)
	}
	// The mux must close the connection without routing it anywhere.
	_ = c.SetReadDeadline(time.Now().Add(2 * time.Second))
	if _, err := c.Read(make([]byte, 1)); err == nil {
		t.Fatal("fake handshake connection was not closed")
	}
	select {
	case conn := <-m.http.ch:
		t.Fatalf("fake handshake leaked to http side: %v", conn.RemoteAddr())
	case conn := <-m.torrent.ch:
		t.Fatalf("fake handshake leaked to torrent side: %v", conn.RemoteAddr())
	case <-time.After(150 * time.Millisecond):
	}
}

func TestMuxDropsSilentConnection(t *testing.T) {
	_, addr := startTestMux(t)
	c := dial(t, addr)
	// Say nothing: the sniff deadline must reap the connection.
	_ = c.SetReadDeadline(time.Now().Add(3 * time.Second))
	if _, err := c.Read(make([]byte, 1)); err == nil {
		t.Fatal("silent connection was not closed")
	}
}

func TestMuxServesRealHTTPServer(t *testing.T) {
	m, addr := startTestMux(t)
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
		_, _ = w.Write([]byte("ok"))
	})}
	go srv.Serve(m.HTTPListener())
	defer srv.Close()
	resp, err := http.Get("http://" + addr + "/healthz")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != 200 || string(body) != "ok" {
		t.Fatalf("got %d %q, want 200 ok", resp.StatusCode, body)
	}
}
