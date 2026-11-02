package main

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/hex"
	"fmt"
	"io"
	"log"
	"net"
	"net/netip"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/anacrolix/dht/v2"
	"github.com/anacrolix/utp"
	"golang.zx2c4.com/wireguard/conn"
	"golang.zx2c4.com/wireguard/device"
	"golang.zx2c4.com/wireguard/tun"
	"golang.zx2c4.com/wireguard/tun/netstack"
)

// WireGuardConfig holds parsed WireGuard tunnel settings.
type WireGuardConfig struct {
	PrivateKey string // 32-byte hex string
	Addresses  []netip.Addr
	DNS        []netip.Addr
	MTU        int
	ListenPort int
	Peers      []WireGuardPeer
}

// WireGuardPeer represents a single peer in the WireGuard configuration.
type WireGuardPeer struct {
	PublicKey           string // 32-byte hex string
	PresharedKey        string // 32-byte hex string (optional)
	Endpoint            string // resolved "ip:port" string
	AllowedIPs          []string
	PersistentKeepalive int
}

// WireGuardTunnel holds the running userspace WireGuard device and netstack.
type WireGuardTunnel struct {
	Dev      *device.Device
	Tun      tun.Device
	Net      *netstack.Net
	LocalIPs []netip.Addr
	Config   *WireGuardConfig
	Logger   *log.Logger
}

// Close shuts down the userspace WireGuard device.
func (w *WireGuardTunnel) Close() error {
	if w.Dev != nil {
		w.Dev.Close()
	} else if w.Tun != nil {
		_ = w.Tun.Close()
	}
	return nil
}

// ListenUDP opens a UDP PacketConn bound to the WireGuard interface.
func (w *WireGuardTunnel) ListenUDP(port int) (net.PacketConn, error) {
	var laddr *net.UDPAddr
	if len(w.LocalIPs) > 0 {
		laddr = &net.UDPAddr{
			IP:   w.LocalIPs[0].AsSlice(),
			Port: port,
		}
	} else {
		laddr = &net.UDPAddr{
			IP:   net.IPv4zero,
			Port: port,
		}
	}
	return w.Net.ListenUDP(laddr)
}

// ListenTCP opens a TCP net.Listener bound to the WireGuard interface.
func (w *WireGuardTunnel) ListenTCP(port int) (net.Listener, error) {
	var laddr *net.TCPAddr
	if len(w.LocalIPs) > 0 {
		laddr = &net.TCPAddr{
			IP:   w.LocalIPs[0].AsSlice(),
			Port: port,
		}
	} else {
		laddr = &net.TCPAddr{
			IP:   net.IPv4zero,
			Port: port,
		}
	}
	return w.Net.ListenTCP(laddr)
}

// parseKey validates and converts a WireGuard key (base64 or hex) to a 32-byte hex string.
func parseKey(raw string) (string, error) {
	s := strings.TrimSpace(raw)
	if len(s) == 64 {
		if _, err := hex.DecodeString(s); err == nil {
			return strings.ToLower(s), nil
		}
	}
	b, err := base64.StdEncoding.DecodeString(s)
	if err != nil {
		return "", fmt.Errorf("invalid base64 key: %w", err)
	}
	if len(b) != 32 {
		return "", fmt.Errorf("wireguard key must be 32 bytes, got %d", len(b))
	}
	return hex.EncodeToString(b), nil
}

// resolveEndpoint ensures the endpoint is an "ip:port" string, resolving hostnames via DNS if needed.
func resolveEndpoint(ep string) (string, error) {
	ep = strings.TrimSpace(ep)
	host, port, err := net.SplitHostPort(ep)
	if err != nil {
		return "", fmt.Errorf("invalid endpoint %q: %w", ep, err)
	}
	if ip := net.ParseIP(host); ip != nil {
		return net.JoinHostPort(ip.String(), port), nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	ips, err := net.DefaultResolver.LookupIP(ctx, "ip", host)
	if err != nil {
		return "", fmt.Errorf("resolve wireguard endpoint %q: %w", host, err)
	}
	if len(ips) == 0 {
		return "", fmt.Errorf("no IP address found for wireguard endpoint %q", host)
	}
	return net.JoinHostPort(ips[0].String(), port), nil
}

// ParseWireGuardConfig parses a standard WireGuard .conf file / string.
func ParseWireGuardConfig(confStr string) (*WireGuardConfig, error) {
	scanner := bufio.NewScanner(strings.NewReader(confStr))
	cfg := &WireGuardConfig{
		MTU: 1420,
	}

	var currentSection string
	var currentPeer *WireGuardPeer

	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") || strings.HasPrefix(line, ";") {
			continue
		}
		// Strip inline comments if prefixed with space or tab
		if idx := strings.Index(line, " #"); idx != -1 {
			line = strings.TrimSpace(line[:idx])
		}
		if idx := strings.Index(line, "\t#"); idx != -1 {
			line = strings.TrimSpace(line[:idx])
		}

		if strings.HasPrefix(line, "[") && strings.HasSuffix(line, "]") {
			sec := strings.ToLower(strings.TrimSpace(line[1 : len(line)-1]))
			if sec == "peer" {
				if currentPeer != nil {
					cfg.Peers = append(cfg.Peers, *currentPeer)
				}
				currentPeer = &WireGuardPeer{}
			} else if sec != "interface" {
				return nil, fmt.Errorf("unknown section [%s]", sec)
			}
			currentSection = sec
			continue
		}

		eq := strings.IndexAny(line, "=:")
		if eq == -1 {
			continue
		}
		key := strings.ToLower(strings.TrimSpace(line[:eq]))
		val := strings.TrimSpace(line[eq+1:])

		switch currentSection {
		case "interface":
			switch key {
			case "privatekey":
				k, err := parseKey(val)
				if err != nil {
					return nil, fmt.Errorf("PrivateKey: %w", err)
				}
				cfg.PrivateKey = k
			case "address":
				for _, part := range strings.Split(val, ",") {
					part = strings.TrimSpace(part)
					if part == "" {
						continue
					}
					var addr netip.Addr
					if strings.Contains(part, "/") {
						prefix, err := netip.ParsePrefix(part)
						if err != nil {
							return nil, fmt.Errorf("invalid Address %q: %w", part, err)
						}
						addr = prefix.Addr()
					} else {
						var err error
						addr, err = netip.ParseAddr(part)
						if err != nil {
							return nil, fmt.Errorf("invalid Address %q: %w", part, err)
						}
					}
					cfg.Addresses = append(cfg.Addresses, addr)
				}
			case "dns":
				for _, part := range strings.Split(val, ",") {
					part = strings.TrimSpace(part)
					if part == "" {
						continue
					}
					addr, err := netip.ParseAddr(part)
					if err == nil {
						cfg.DNS = append(cfg.DNS, addr)
					}
				}
			case "mtu":
				if n, err := strconv.Atoi(val); err == nil && n > 0 {
					cfg.MTU = n
				}
			case "listenport":
				if n, err := strconv.Atoi(val); err == nil && n > 0 {
					cfg.ListenPort = n
				}
			}
		case "peer":
			if currentPeer == nil {
				currentPeer = &WireGuardPeer{}
			}
			switch key {
			case "publickey":
				k, err := parseKey(val)
				if err != nil {
					return nil, fmt.Errorf("Peer PublicKey: %w", err)
				}
				currentPeer.PublicKey = k
			case "presharedkey":
				k, err := parseKey(val)
				if err != nil {
					return nil, fmt.Errorf("Peer PresharedKey: %w", err)
				}
				currentPeer.PresharedKey = k
			case "endpoint":
				ep, err := resolveEndpoint(val)
				if err != nil {
					return nil, fmt.Errorf("Peer Endpoint: %w", err)
				}
				currentPeer.Endpoint = ep
			case "allowedips":
				for _, part := range strings.Split(val, ",") {
					part = strings.TrimSpace(part)
					if part != "" {
						currentPeer.AllowedIPs = append(currentPeer.AllowedIPs, part)
					}
				}
			case "persistentkeepalive":
				if n, err := strconv.Atoi(val); err == nil && n >= 0 {
					currentPeer.PersistentKeepalive = n
				}
			}
		}
	}
	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("read wireguard config: %w", err)
	}
	if currentPeer != nil {
		cfg.Peers = append(cfg.Peers, *currentPeer)
	}

	if cfg.PrivateKey == "" {
		return nil, fmt.Errorf("wireguard config missing PrivateKey in [Interface]")
	}
	if len(cfg.Addresses) == 0 {
		return nil, fmt.Errorf("wireguard config missing Address in [Interface]")
	}
	if len(cfg.DNS) == 0 {
		// Provide default DNS resolvers if none configured
		cfg.DNS = []netip.Addr{
			netip.MustParseAddr("1.1.1.1"),
			netip.MustParseAddr("8.8.8.8"),
		}
	}
	return cfg, nil
}

// ToUAPI converts the parsed configuration into the WireGuard UAPI format string for IpcSet.
func (cfg *WireGuardConfig) ToUAPI() string {
	var b strings.Builder
	b.WriteString(fmt.Sprintf("private_key=%s\n", cfg.PrivateKey))
	if cfg.ListenPort > 0 {
		b.WriteString(fmt.Sprintf("listen_port=%d\n", cfg.ListenPort))
	}
	b.WriteString("replace_peers=true\n")
	for _, p := range cfg.Peers {
		b.WriteString(fmt.Sprintf("public_key=%s\n", p.PublicKey))
		if p.PresharedKey != "" {
			b.WriteString(fmt.Sprintf("preshared_key=%s\n", p.PresharedKey))
		}
		if p.Endpoint != "" {
			b.WriteString(fmt.Sprintf("endpoint=%s\n", p.Endpoint))
		}
		if p.PersistentKeepalive > 0 {
			b.WriteString(fmt.Sprintf("persistent_keepalive_interval=%d\n", p.PersistentKeepalive))
		}
		if len(p.AllowedIPs) == 0 {
			b.WriteString("allowed_ip=0.0.0.0/0\n")
			b.WriteString("allowed_ip=::/0\n")
		} else {
			for _, aip := range p.AllowedIPs {
				b.WriteString(fmt.Sprintf("allowed_ip=%s\n", aip))
			}
		}
	}
	return b.String()
}

// loadWireGuardConf resolves an optional WireGuard config string or file path.
func loadWireGuardConf(raw, dataDir string) (string, error) {
	raw = strings.TrimSpace(raw)
	if raw != "" {
		// Check if it's an existing file path
		if _, err := os.Stat(raw); err == nil {
			b, err := os.ReadFile(raw)
			if err != nil {
				return "", fmt.Errorf("read wireguard file %q: %w", raw, err)
			}
			return string(b), nil
		}
		// If it looks like inline configuration content
		if strings.Contains(raw, "[") || strings.Contains(raw, "\n") {
			return raw, nil
		}
		// Try reading as file path anyway in case stat had permission issue
		if b, err := os.ReadFile(raw); err == nil {
			return string(b), nil
		}
		return raw, nil
	}

	// If no explicit raw setting, look in dataDir for wireguard.conf or wg0.conf
	candidates := []string{
		filepath.Join(dataDir, "wireguard.conf"),
		filepath.Join(dataDir, "wg0.conf"),
	}
	for _, p := range candidates {
		if _, err := os.Stat(p); err == nil {
			b, err := os.ReadFile(p)
			if err == nil && len(b) > 0 {
				return string(b), nil
			}
		}
	}
	return "", nil
}

// StartWireGuard creates and activates a userspace WireGuard device with netstack.
func StartWireGuard(confStr string, lg *log.Logger) (*WireGuardTunnel, error) {
	cfg, err := ParseWireGuardConfig(confStr)
	if err != nil {
		return nil, fmt.Errorf("parse wireguard config: %w", err)
	}

	tunDev, tnet, err := netstack.CreateNetTUN(cfg.Addresses, cfg.DNS, cfg.MTU)
	if err != nil {
		return nil, fmt.Errorf("wireguard create tun: %w", err)
	}

	bind := conn.NewDefaultBind()
	dev := device.NewDevice(tunDev, bind, device.NewLogger(device.LogLevelError, "[wireguard] "))

	uapi := cfg.ToUAPI()
	if err := dev.IpcSet(uapi); err != nil {
		_ = tunDev.Close()
		dev.Close()
		return nil, fmt.Errorf("wireguard ipc set: %w", err)
	}
	if err := dev.Up(); err != nil {
		_ = tunDev.Close()
		dev.Close()
		return nil, fmt.Errorf("wireguard up: %w", err)
	}

	tunnel := &WireGuardTunnel{
		Dev:      dev,
		Tun:      tunDev,
		Net:      tnet,
		LocalIPs: cfg.Addresses,
		Config:   cfg,
		Logger:   lg,
	}
	return tunnel, nil
}

// netstackDialer adapts netstack.Net to torrent.Dialer for outgoing TCP peer connections.
type netstackDialer struct {
	tnet    *netstack.Net
	network string
}

func (d *netstackDialer) Dial(ctx context.Context, addr string) (net.Conn, error) {
	return d.tnet.DialContext(ctx, d.network, addr)
}

func (d *netstackDialer) DialerNetwork() string {
	return d.network
}

// utpDialer adapts utp.Socket to torrent.Dialer for outgoing uTP peer connections.
type utpDialer struct {
	sock *utp.Socket
}

func (u *utpDialer) Dial(ctx context.Context, addr string) (net.Conn, error) {
	return u.sock.DialContext(ctx, "udp", addr)
}

func (u *utpDialer) DialerNetwork() string {
	return "udp"
}

// dhtCloser closes both the DHT server and its underlying packet conn.
type dhtCloser struct {
	ds   *dht.Server
	conn io.Closer
}

func (d dhtCloser) Close() error {
	d.ds.Close()
	return d.conn.Close()
}
