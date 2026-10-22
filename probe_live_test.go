package main

import (
	"context"
	"io"
	"log"
	"os"
	"testing"
	"time"
)

// TestLiveProbe measures a real swarm. It is skipped unless BITLAB_LIVE_PROBE
// is set, because it needs outbound UDP and a public torrent: the point is to
// prove the measurement path against actual peers, which no fixture can do.
//
//	BITLAB_LIVE_PROBE=1 go test -run TestLiveProbe -v -timeout 240s .
func TestLiveProbe(t *testing.T) {
	if os.Getenv("BITLAB_LIVE_PROBE") == "" {
		t.Skip("set BITLAB_LIVE_PROBE=1 to measure a real swarm")
	}
	store := testStore(t)
	cfg := Config{
		DataDir:       t.TempDir(),
		Trackers:      defaultTrackers,
		MaxTorrents:   20,
		IdleTimeout:   20 * time.Minute,
		CacheMaxBytes: 16 << 30,
	}
	eng, err := NewEngine(cfg, store, log.New(io.Discard, "", 0))
	if err != nil {
		t.Fatalf("engine: %v", err)
	}
	defer eng.Close()

	// Sintel, the WebTorrent reference torrent: permanently seeded and small.
	c := Candidate{
		InfoHash: "08ada5a7a6183aae1e09d831df6748d566095a10",
		Title:    "Sintel (2010) 1080p",
		Indexer:  "test",
		Seeders:  100,
		Size:     129 << 20,
		Typ:      "movie",
	}
	ctx, cancel := context.WithTimeout(context.Background(), 150*time.Second)
	defer cancel()
	row, attempted := eng.Probe(ctx, c, encodeFeat(make([]float32, mInNum), 2, 3))
	if !attempted {
		t.Fatal("probe was refused")
	}
	t.Logf("meta=%dms first=%dms total=%dms mbps=%.2f bytes=%d peers=%d cold=%v cached=%v ok=%v err=%q",
		row.MetaMs, row.FirstMs, row.TotalMs, row.Mbps, row.Bytes, row.SeedersSeen,
		row.Cold, row.Cached, row.OK, row.Err)
	if !row.OK {
		t.Fatalf("probe did not reach the swarm: %s", row.Err)
	}
	if row.TotalMs <= 0 {
		t.Fatalf("no total label: %+v", row)
	}
	if row.Cached {
		t.Fatalf("a first probe reported cached bytes: %+v", row)
	}
}
