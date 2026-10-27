package main

import "testing"

func TestBaseClientConfigDefaults(t *testing.T) {
	cc := baseClientConfig(Config{TorrentPort: 51413})
	if !cc.AcceptPeerConnections {
		t.Fatal("default config must accept inbound peer connections")
	}
	if cc.NoDefaultPortForwarding {
		t.Fatal("default config must leave port forwarding enabled")
	}
	if cc.ListenPort != 51413 {
		t.Fatalf("ListenPort = %d, want 51413", cc.ListenPort)
	}
}
