package main

import "testing"

func TestBaseClientConfigDefaultListens(t *testing.T) {
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

func TestBaseClientConfigNoListen(t *testing.T) {
	cc := baseClientConfig(Config{NoListen: true})
	if cc.AcceptPeerConnections {
		t.Fatal("no-listen config must refuse inbound peer connections")
	}
	if !cc.NoDefaultPortForwarding {
		t.Fatal("no-listen config must not request a UPnP pinhole it will not use")
	}
}
