package main

import (
	"bytes"
	"errors"
	"io"
	"net"
	"strings"
	"sync"
	"time"
)

// Port sharing: the HTTP server and the torrent client share the service
// port unconditionally — there is nothing to configure, because the right
// behavior depends on the connection, not on the deployment. An HTTP
// request is recognised positively, by its method token; everything else
// goes to the torrent client, which already knows how to sort plaintext
// handshakes from MSE-encrypted ones (whose key material looks like
// noise) and from plain garbage, exactly as it does on its own listener.
//
// This only helps where the port is raw TCP end to end (a VPS, a direct
// tunnel). Behind an HTTP-only ingress the BitTorrent bytes never arrive,
// and nothing breaks: those connections simply never happen, which is
// where an unreachable client ends up anyway. UDP is untouched either
// way: uTP and DHT keep their own socket on the torrent port, reachable or
// not, exactly as before.

const btProtocolString = "BitTorrent protocol"

// sniffTimeout bounds how long a connection may sit silent before its first
// bytes. HTTP clients and BitTorrent peers both speak first and immediately;
// anything slower is a scanner or a stuck client, and dropping it is free.
const sniffTimeout = 15 * time.Second

type portMux struct {
	ln      net.Listener
	http    *chanListener
	torrent *chanListener
	timeout time.Duration
}

func newPortMux(ln net.Listener) *portMux {
	m := &portMux{ln: ln, timeout: sniffTimeout}
	m.http = newChanListener(ln)
	m.torrent = newChanListener(ln)
	return m
}

// HTTPListener is the net.Listener http.Server serves the web side from.
func (m *portMux) HTTPListener() net.Listener { return m.http }

// TorrentListener is registered with the torrent client via AddListener; the
// client's Listener interface is Accept + Addr.
func (m *portMux) TorrentListener() *chanListener { return m.torrent }

// Serve accepts and classifies connections until the shared listener closes.
func (m *portMux) Serve() {
	var backoff time.Duration
	for {
		c, err := m.ln.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				m.http.Close()
				m.torrent.Close()
				return
			}
			// A transient accept failure (running out of file
			// descriptors next to a torrent client's socket churn, a
			// temporary network error) must not stop dispatch for
			// good: back off and keep accepting, as http.Server does.
			if backoff == 0 {
				backoff = 5 * time.Millisecond
			} else if backoff < time.Second {
				backoff *= 2
			}
			time.Sleep(backoff)
			continue
		}
		backoff = 0
		go m.dispatch(c)
	}
}

// Close shuts the shared listener, which ends Serve and both sides.
func (m *portMux) Close() error { return m.ln.Close() }

func (m *portMux) dispatch(c net.Conn) {
	target, prefix, ok := m.sniff(c)
	if !ok {
		c.Close()
		return
	}
	// Hand over a connection with no deadline: each side applies its own
	// timeouts from here (the HTTP server's, the torrent client's).
	_ = c.SetReadDeadline(time.Time{})
	pc := &prefixConn{Conn: c, r: io.MultiReader(bytes.NewReader(prefix), c)}
	select {
	case target.ch <- pc:
	case <-target.done:
		c.Close()
	}
}

// httpMethods are the request-line tokens an HTTP client can open with.
// A connection is web traffic only by positively matching one; the
// trailing space matters (it separates the method from the target).
var httpMethods = []string{
	"GET ", "HEAD ", "POST ", "PUT ", "PATCH ", "DELETE ", "OPTIONS ",
	"TRACE ", "CONNECT ", "PRI ",
}

// sniff reads the classification prefix and picks a side. A BitTorrent
// handshake (0x13 + the protocol string) needs no full validation here:
// bytes that start 0x13 are the torrent client's to judge, as are all
// bytes that do not positively match an HTTP method — an MSE-encrypted
// handshake begins with a Diffie-Hellman key, indistinguishable from
// random bytes, and only the client's own handshake logic can sort it
// from garbage.
func (m *portMux) sniff(c net.Conn) (*chanListener, []byte, bool) {
	_ = c.SetReadDeadline(time.Now().Add(m.timeout))
	var buf []byte
	one := make([]byte, 1)
	read := func() bool {
		if _, err := io.ReadFull(c, one); err != nil {
			return false
		}
		buf = append(buf, one[0])
		return true
	}
	if !read() {
		return nil, nil, false
	}
	if buf[0] == byte(len(btProtocolString)) {
		for len(buf) < 1+len(btProtocolString) {
			if !read() {
				return nil, nil, false
			}
		}
		return m.torrent, buf, true
	}
	for {
		s := string(buf)
		possible := false
		for _, meth := range httpMethods {
			if s == meth {
				return m.http, buf, true
			}
			if strings.HasPrefix(meth, s) {
				possible = true
			}
		}
		if !possible || len(buf) >= 8 {
			return m.torrent, buf, true
		}
		if !read() {
			return nil, nil, false
		}
	}
}

// chanListener is a net.Listener fed by the mux. Closing either side closes
// the shared socket, which is what unwinds the other side at shutdown:
// http.Server closes its listener, the mux's Accept fails, and both channel
// listeners are closed in turn.
type chanListener struct {
	ch   chan net.Conn
	addr net.Addr
	done chan struct{}
	once sync.Once
	root net.Listener
}

func newChanListener(root net.Listener) *chanListener {
	return &chanListener{
		ch:   make(chan net.Conn, 64),
		addr: root.Addr(),
		done: make(chan struct{}),
		root: root,
	}
}

func (l *chanListener) Accept() (net.Conn, error) {
	select {
	case c := <-l.ch:
		return c, nil
	case <-l.done:
		return nil, net.ErrClosed
	}
}

func (l *chanListener) Addr() net.Addr { return l.addr }

func (l *chanListener) Close() error {
	l.once.Do(func() { close(l.done) })
	return l.root.Close()
}

// prefixConn replays the bytes the sniffer consumed before the connection's
// own stream continues, so neither side ever learns the mux exists.
type prefixConn struct {
	net.Conn
	r io.Reader
}

func (c *prefixConn) Read(b []byte) (int, error) { return c.r.Read(b) }
