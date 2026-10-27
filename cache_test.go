package main

import (
	"io"
	"log"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/anacrolix/torrent"
)

const gib = int64(1) << 30

func names(plan []cacheDecision) []string {
	out := make([]string, len(plan))
	for i, d := range plan {
		out[i] = d.Unit.Name
	}
	return out
}

func TestPlanCacheRetention(t *testing.T) {
	now := time.Now()
	units := []cacheUnit{
		{Name: "old", Bytes: 2 * gib, LastUse: now.Add(-30 * time.Hour)},
		{Name: "fresh", Bytes: 2 * gib, LastUse: now.Add(-1 * time.Hour)},
	}
	plan := planCache(units, 4*gib, 16*gib, 0, 10*gib, now, 10*time.Minute, 24*time.Hour)
	if len(plan) != 1 || plan[0].Unit.Name != "old" || plan[0].Reason != "retention" {
		t.Fatalf("want retention of old only, got %v", names(plan))
	}
}

func TestPlanCacheBudgetEvictsLRU(t *testing.T) {
	now := time.Now()
	units := []cacheUnit{
		{Name: "newest", Bytes: 4 * gib, LastUse: now.Add(-20 * time.Minute)},
		{Name: "older", Bytes: 4 * gib, LastUse: now.Add(-4 * time.Hour)},
		{Name: "oldest", Bytes: 4 * gib, LastUse: now.Add(-9 * time.Hour)},
	}
	// 12 GiB on disk against a 4 GiB budget: the two least-recently-used go.
	plan := planCache(units, 12*gib, 4*gib, 0, 100*gib, now, 10*time.Minute, 0)
	if got := names(plan); len(got) != 2 || got[0] != "oldest" || got[1] != "older" {
		t.Fatalf("want [oldest older], got %v", got)
	}
	if plan[0].Reason != "budget" {
		t.Fatalf("want budget reason, got %q", plan[0].Reason)
	}
}

func TestPlanCacheBudgetStopsWhenSatisfied(t *testing.T) {
	now := time.Now()
	units := []cacheUnit{
		{Name: "a", Bytes: 1 * gib, LastUse: now.Add(-3 * time.Hour)},
		{Name: "b", Bytes: 1 * gib, LastUse: now.Add(-2 * time.Hour)},
	}
	// 10 GiB used, 10 GiB budget: nothing to do.
	plan := planCache(units, 10*gib, 10*gib, 0, 100*gib, now, 10*time.Minute, 0)
	if len(plan) != 0 {
		t.Fatalf("want no evictions, got %v", names(plan))
	}
}

func TestPlanCacheReserveUnderBudget(t *testing.T) {
	now := time.Now()
	units := []cacheUnit{
		{Name: "a", Bytes: 2 * gib, LastUse: now.Add(-3 * time.Hour)},
		{Name: "b", Bytes: 2 * gib, LastUse: now.Add(-1 * time.Hour)},
	}
	// Comfortably under budget but only 1 GiB free with a 2 GiB reserve.
	plan := planCache(units, 6*gib, 16*gib, 2*gib, 1*gib, now, 10*time.Minute, 0)
	if got := names(plan); len(got) != 1 || got[0] != "a" {
		t.Fatalf("want [a], got %v", got)
	}
	if plan[0].Reason != "reserve" {
		t.Fatalf("want reserve reason, got %q", plan[0].Reason)
	}
}

func TestPlanCacheProtectsReadersAndGrace(t *testing.T) {
	now := time.Now()
	units := []cacheUnit{
		{Name: "playing", Bytes: 10 * gib, LastUse: now.Add(-5 * time.Hour), Readers: 1},
		{Name: "just-touched", Bytes: 10 * gib, LastUse: now.Add(-2 * time.Minute)},
		{Name: "old", Bytes: 1 * gib, LastUse: now.Add(-2 * time.Hour)},
	}
	plan := planCache(units, 21*gib, 2*gib, 0, 0, now, 10*time.Minute, 0)
	if got := names(plan); len(got) != 1 || got[0] != "old" {
		t.Fatalf("want only the unprotected entry, got %v", got)
	}
}

func TestPlanCacheZeroRulesDisable(t *testing.T) {
	now := time.Now()
	units := []cacheUnit{{Name: "a", Bytes: 5 * gib, LastUse: now.Add(-500 * time.Hour)}}
	plan := planCache(units, 5*gib, 0, 0, 0, now, 10*time.Minute, 0)
	if len(plan) != 0 {
		t.Fatalf("want no evictions with both rules off, got %v", names(plan))
	}
}

func TestPlanCacheUnknownFreeSkipsReserve(t *testing.T) {
	now := time.Now()
	units := []cacheUnit{{Name: "a", Bytes: 5 * gib, LastUse: now.Add(-2 * time.Hour)}}
	plan := planCache(units, 5*gib, 16*gib, 2*gib, -1, now, 10*time.Minute, 0)
	if len(plan) != 0 {
		t.Fatalf("unknown free space must not trigger the reserve, got %v", names(plan))
	}
}

func TestScanCacheAggregatesAndSkipsState(t *testing.T) {
	root := t.TempDir()
	mk := func(rel string, size int) string {
		p := filepath.Join(root, rel)
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(p, make([]byte, size), 0o644); err != nil {
			t.Fatal(err)
		}
		return p
	}
	mk(filepath.Join("Season 1", "S01E01.mkv.part"), 4096)
	mk(filepath.Join("Season 1", "S01E02.mkv.part"), 8192)
	mk("Movie.2024.1080p.mkv.part", 16384)
	mk("Movie.2024.1080p.srt", 512) // a finished sidecar
	mk("bitlab.db", 4096)           // engine state, never cache
	mk(filepath.Join(".hidden", "x"), 4096)

	entries, err := scanCache(root)
	if err != nil {
		t.Fatal(err)
	}
	by := map[string]cacheEntry{}
	for _, e := range entries {
		by[e.Name] = e
	}
	if len(entries) != 3 {
		t.Fatalf("want 3 media entries, got %d: %+v", len(entries), entries)
	}
	dir := by["Season 1"]
	if !dir.Dir || !dir.Partial || dir.Bytes < 12288 {
		t.Fatalf("bad Season 1 entry: %+v", dir)
	}
	if movie := by["Movie.2024.1080p.mkv.part"]; movie.Dir || !movie.Partial {
		t.Fatalf("bad single-file entry: %+v", movie)
	}
	if _, ok := by["bitlab.db"]; ok {
		t.Fatal("database file must not be cache")
	}
}

// newTestEngine builds just enough of an Engine for the sweeper tests: a real
// (idle) torrent client, a real store and a temp data directory.
func newTestEngine(t *testing.T, dir string) *Engine {
	t.Helper()
	store, err := OpenStore(filepath.Join(dir, "bitlab.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { store.Close() })
	cc := torrent.NewDefaultClientConfig()
	cc.DataDir = dir
	cc.NoDHT = true
	cc.DisableTrackers = true
	cc.NoDefaultPortForwarding = true
	cc.Seed = false
	cl, err := torrent.NewClient(cc)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { cl.Close() })
	e := &Engine{
		cl:        cl,
		dataDir:   dir,
		store:     store,
		act:       map[string]*activity{},
		log:       log.New(io.Discard, "", 0),
		graceFor:  10 * time.Minute,
		retainFor: 24 * time.Hour,
	}
	e.maxCacheBytes.Store(16 * gib)
	return e
}

// seedCacheEntry writes a .part file under a torrent-named entry and gives the
// whole entry an age. Streaming leaves exactly this shape behind.
func seedCacheEntry(t *testing.T, dir, name string, age time.Duration) string {
	t.Helper()
	root := filepath.Join(dir, name)
	if err := os.MkdirAll(root, 0o755); err != nil {
		t.Fatal(err)
	}
	blob := make([]byte, 1<<20)
	for i := range blob {
		blob[i] = byte(i*17 + 3)
	}
	p := filepath.Join(root, "payload.mkv.part")
	if err := os.WriteFile(p, blob, 0o644); err != nil {
		t.Fatal(err)
	}
	when := time.Now().Add(-age)
	if err := os.Chtimes(p, when, when); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(root, when, when); err != nil {
		t.Fatal(err)
	}
	return root
}

// TestSweepCacheReclaimsAbandonedPartials is the regression that matters: a
// stream that was never completed used to be invisible to eviction, so its
// .part files stayed on disk forever.
func TestSweepCacheReclaimsAbandonedPartials(t *testing.T) {
	dir := t.TempDir()
	e := newTestEngine(t, dir)
	stale := seedCacheEntry(t, dir, "Old Movie (2019)", 48*time.Hour)
	fresh := seedCacheEntry(t, dir, "New Movie (2026)", 2*time.Minute)

	e.sweepCache(time.Now())

	if _, err := os.Stat(stale); !os.IsNotExist(err) {
		t.Fatalf("abandoned partial survived the sweep: %v", err)
	}
	if _, err := os.Stat(fresh); err != nil {
		t.Fatalf("fresh entry was deleted inside the grace window: %v", err)
	}
	if e.stats.Used <= 0 || e.stats.Entries != 1 {
		t.Fatalf("stats not updated: %+v", e.stats)
	}
}

// TestSweepCacheEvictsPartialsUnderBudget proves the budget now applies to
// incomplete downloads too, and that it goes least-recently-used first.
func TestSweepCacheEvictsPartialsUnderBudget(t *testing.T) {
	dir := t.TempDir()
	e := newTestEngine(t, dir)
	e.maxCacheBytes.Store(1) // force pressure
	older := seedCacheEntry(t, dir, "Older (2018)", 3*time.Hour)
	newer := seedCacheEntry(t, dir, "Newer (2025)", 1*time.Hour)

	e.sweepCache(time.Now())

	if _, err := os.Stat(older); !os.IsNotExist(err) {
		t.Fatalf("older entry survived pressure eviction: %v", err)
	}
	if _, err := os.Stat(newer); !os.IsNotExist(err) {
		t.Fatalf("newer entry survived pressure eviction: %v", err)
	}
	if e.stats.EvictedCount != 2 || e.stats.Used != 0 {
		t.Fatalf("stats wrong after eviction: %+v", e.stats)
	}
}
