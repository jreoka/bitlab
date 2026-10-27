package main

import (
	"bytes"
	"io"
	"net"
	"sync"
	"time"
)

// Port sharing: the HTTP server and the torrent client share the service
// port unconditionally — there is nothing to configure, because the right
// behavior depends on the connection, not on the deployment. A BitTorrent handshake starts with the
// byte 0x13 followed by "BitTorrent protocol"; an HTTP request starts with a
// method token. The mux peeks at those first bytes of every connection and
// hands the connection — prefix included — to the matching side.
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
	for {
		c, err := m.ln.Accept()
		if err != nil {
			m.http.Close()
			m.torrent.Close()
			return
		}
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

// sniff reads the classification prefix: a single byte for the web side, the
// full 20-byte protocol string for the torrent side. A leading 0x13 that is
// not followed by the protocol string is neither protocol — drop it.
func (m *portMux) sniff(c net.Conn) (*chanListener, []byte, bool) {
	_ = c.SetReadDeadline(time.Now().Add(m.timeout))
	first := make([]byte, 1)
	if _, err := io.ReadFull(c, first); err != nil {
		return nil, nil, false
	}
	if first[0] != byte(len(btProtocolString)) {
		return m.http, first, true
	}
	rest := make([]byte, len(btProtocolString))
	if _, err := io.ReadFull(c, rest); err != nil {
		return nil, nil, false
	}
	if string(rest) != btProtocolString {
		return nil, nil, false
	}
	return m.torrent, append(first, rest...), true
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
