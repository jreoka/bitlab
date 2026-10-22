package main

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// privateAddress reports whether an address must not be reached by outbound
// indexer/metadata requests. Loopback and link-local are the ones that matter
// most (kubelet, the pod network, and cloud metadata endpoints).
// These are not covered by net.IP.IsPrivate: carrier-grade NAT (Tailscale and
// several cloud internals), benchmark/documentation ranges and the reserved
// 240/4 block. Refusing them keeps an indexer URL from reaching something the
// public internet cannot.
var reservedNets = func() []*net.IPNet {
	cidrs := []string{
		"0.0.0.0/8", "100.64.0.0/10", "192.0.0.0/24", "192.0.2.0/24",
		"198.18.0.0/15", "198.51.100.0/24", "203.0.113.0/24", "240.0.0.0/4",
		"2001:db8::/32", "100::/64",
	}
	out := make([]*net.IPNet, 0, len(cidrs))
	for _, c := range cidrs {
		_, n, err := net.ParseCIDR(c)
		if err == nil {
			out = append(out, n)
		}
	}
	return out
}()

func privateAddress(ip net.IP) bool {
	if ip.IsLoopback() || ip.IsPrivate() || ip.IsLinkLocalUnicast() ||
		ip.IsLinkLocalMulticast() || ip.IsUnspecified() || ip.IsMulticast() {
		return true
	}
	for _, n := range reservedNets {
		if n.Contains(ip) {
			return true
		}
	}
	return false
}

// guardedTransport resolves names itself and dials only public addresses, so a
// configured indexer URL cannot be pointed at 127.0.0.1, 10.0.0.0/8 or
// 169.254.169.254. Redirects go through the same dialer.
func guardedDialContext(allowPrivate bool) func(context.Context, string, string) (net.Conn, error) {
	dialer := &net.Dialer{Timeout: 10 * time.Second, KeepAlive: 30 * time.Second}
	return func(ctx context.Context, network, addr string) (net.Conn, error) {
		host, port, err := net.SplitHostPort(addr)
		if err != nil {
			return nil, err
		}
		if allowPrivate {
			return dialer.DialContext(ctx, network, addr)
		}
		ips, err := net.DefaultResolver.LookupIPAddr(ctx, host)
		if err != nil {
			return nil, err
		}
		for _, ip := range ips {
			if privateAddress(ip.IP) {
				continue
			}
			// Dial the resolved address rather than the name to avoid a second,
			// rebindable lookup.
			return dialer.DialContext(ctx, network, net.JoinHostPort(ip.IP.String(), port))
		}
		return nil, fmt.Errorf("refusing to connect to a private address (%s)", host)
	}
}

func guardedTransport(allowPrivate bool) *http.Transport {
	tr, _ := http.DefaultTransport.(*http.Transport)
	if tr == nil {
		tr = &http.Transport{}
	} else {
		tr = tr.Clone()
	}
	tr.DialContext = guardedDialContext(allowPrivate)
	return tr
}

// checkOutboundURL rejects URLs that are not plain web URLs or that point at a
// literal private address. Name resolution is enforced by guardedTransport.
func checkOutboundURL(raw string) error {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || u.Host == "" {
		return fmt.Errorf("invalid URL")
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return fmt.Errorf("only http and https URLs are allowed")
	}
	if ip := net.ParseIP(u.Hostname()); ip != nil && privateAddress(ip) {
		return fmt.Errorf("private addresses are not allowed")
	}
	return nil
}
