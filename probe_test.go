package main

import (
	"path/filepath"
	"testing"
	"time"
)

func testStore(t *testing.T) *Store {
	t.Helper()
	s, err := OpenStore(filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("open store: %v", err)
	}
	t.Cleanup(func() { s.Close() })
	return s
}

func TestObserveCandidatesKeepsFirstSeenAndCooldown(t *testing.T) {
	s := testStore(t)
	if err := s.ObserveCandidates([]Candidate{
		{InfoHash: "aa", Title: "One", Indexer: "apibay", Seeders: 10, Size: 1 << 30},
	}); err != nil {
		t.Fatalf("observe: %v", err)
	}
	first, _, _, ok := s.FactsFor("aa")
	if !ok || first == 0 {
		t.Fatalf("candidate not stored: %v %d", ok, first)
	}
	if err := s.RecordProbe(ProbeRow{InfoHash: "aa", At: time.Now().Unix(), TotalMs: 4000, OK: true, Feat: encodeFeat(make([]float32, mInNum), 2, 3)}); err != nil {
		t.Fatalf("record: %v", err)
	}
	// Re-observing the same release must not reset when it was first seen or
	// when it was last probed: the sampler's cooldown depends on both.
	if err := s.ObserveCandidates([]Candidate{
		{InfoHash: "aa", Title: "One (better name)", Indexer: "apibay", Seeders: 99, Size: 2 << 30},
	}); err != nil {
		t.Fatalf("re-observe: %v", err)
	}
	first2, probeAt, lastMs, _ := s.FactsFor("aa")
	if first2 != first {
		t.Errorf("first_seen moved from %d to %d", first, first2)
	}
	if probeAt == 0 || lastMs != 4000 {
		t.Errorf("probe memory lost: at=%d ms=%d", probeAt, lastMs)
	}
	pool, err := s.ProbePool(time.Now().Add(-time.Hour).Unix(), 10)
	if err != nil {
		t.Fatalf("pool: %v", err)
	}
	if len(pool) != 0 {
		t.Errorf("a just-probed release is due again: %+v", pool)
	}
	pool, err = s.ProbePool(time.Now().Add(time.Hour).Unix(), 10)
	if err != nil || len(pool) != 1 {
		t.Fatalf("pool after cooldown: %d rows, %v", len(pool), err)
	}
	if pool[0].Seeders != 99 || pool[0].Size != 2<<30 {
		t.Errorf("pool served stale row: %+v", pool[0])
	}
}

func TestProbeTrainingLabelsAreHonest(t *testing.T) {
	s := testStore(t)
	feat := encodeFeat(make([]float32, mInNum), 2, 3)
	now := time.Now().Unix()
	rows := []ProbeRow{
		{InfoHash: "ok", At: now, TotalMs: 8000, FirstMs: 8000, Bytes: 500 << 10, OK: true, Feat: feat},
		// a local-cache read measures the volume, not the swarm
		{InfoHash: "cached", At: now, TotalMs: 90, FirstMs: 90, OK: true, Cached: true, Feat: feat},
		// a probe that reached no data is censored, not thrown away
		{InfoHash: "dead", At: now, TotalMs: 45000, OK: false, Err: "metadata timeout", Feat: feat},
		// no feature vector recorded: cannot be reconstructed without leakage
		{InfoHash: "nofeat", At: now, TotalMs: 5000, OK: true},
	}
	for _, r := range rows {
		if err := s.RecordProbe(r); err != nil {
			t.Fatalf("record %s: %v", r.InfoHash, err)
		}
	}
	got, err := s.ProbeTraining(100)
	if err != nil {
		t.Fatalf("training: %v", err)
	}
	// The dead release is a censored sample ("nothing after the wait") and is
	// kept: training only on survivors is what makes a model promise a fast
	// start for a swarm that will never deliver. The cache hit is not a swarm
	// measurement and the featurless row cannot be reconstructed.
	if len(got) != 2 {
		t.Fatalf("training set = %d rows, want 2 (ok + censored): %+v", len(got), got)
	}
	seen := map[string]ProbeRow{}
	for _, r := range got {
		seen[r.InfoHash] = r
	}
	if _, ok := seen["ok"]; !ok {
		t.Error("the clean sample was dropped")
	}
	if _, ok := seen["cached"]; ok {
		t.Error("a local-cache read reached the training set")
	}
	if _, ok := seen["nofeat"]; ok {
		t.Error("a sample without a recorded feature vector reached the training set")
	}
	if seen["ok"].TotalMs != 8000 {
		t.Errorf("label = %d ms, want 8000", seen["ok"].TotalMs)
	}
	if d := seen["dead"]; d.OK || d.TotalMs <= 0 {
		t.Errorf("censored sample is not labelable: ok=%v total=%d", d.OK, d.TotalMs)
	}
}

func TestProbeBudgetWindow(t *testing.T) {
	s := testStore(t)
	now := time.Now().Unix()
	s.RecordProbe(ProbeRow{InfoHash: "a", At: now, Bytes: 500 << 10, OK: true})
	s.RecordProbe(ProbeRow{InfoHash: "b", At: now - 7200, Bytes: 900 << 10, OK: true})
	n, bytes := s.ProbeWindow(now - 3600)
	if n != 1 {
		t.Errorf("window counted %d probes, want 1", n)
	}
	if bytes != 500<<10 {
		t.Errorf("window bytes = %d, want %d", bytes, 500<<10)
	}
}

func TestModelSurvivesRestart(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "test.db")
	s, err := OpenStore(path)
	if err != nil {
		t.Fatal(err)
	}
	n, err := TrainMLP(synthetic(300, 4), 2, 30)
	if err != nil {
		t.Fatal(err)
	}
	blob, _ := n.Marshal()
	if err := s.SaveModel(blob, n.Samples, n.MAE, n.Within2x); err != nil {
		t.Fatal(err)
	}
	s.Close()

	// The same file opens again after a restart and still has the weights.
	s2, err := OpenStore(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer s2.Close()
	back, samples, _, _, _, ok := s2.LoadModel()
	if !ok {
		t.Fatal("model not found after restart")
	}
	if _, err := UnmarshalMLP(back); err != nil {
		t.Fatalf("stored model does not load: %v", err)
	}
	if samples != n.Samples {
		t.Errorf("samples = %d, want %d", samples, n.Samples)
	}
}

func TestEtaBadgeColours(t *testing.T) {
	cases := []struct {
		p90  float64
		want string
	}{
		{2, "🟢 ~2s"},
		{9.9, "🟢 ~10s"},
		{10.5, "🟡 ~11s"},
		{30, "🟡 ~30s"},
		{31, "🔴 ~31s"},
		{95, "🔴 ~2m"},
	}
	for _, c := range cases {
		if got := etaBadge(c.p90); got != c.want {
			t.Errorf("etaBadge(%.1f) = %q, want %q", c.p90, got, c.want)
		}
	}
}

func TestFeatBlobRoundTrip(t *testing.T) {
	x := make([]float32, mInNum)
	for i := range x {
		x[i] = float32(i) * 0.5
	}
	f, ok := decodeFeat(encodeFeat(x, 7, 11))
	if !ok {
		t.Fatal("feature blob did not decode")
	}
	if f.I != 7 || f.G != 11 || len(f.X) != mInNum || f.X[3] != 1.5 {
		t.Fatalf("decoded wrong: %+v", f)
	}
	if _, ok := decodeFeat(""); ok {
		t.Fatal("empty blob decoded as valid")
	}
	if _, ok := decodeFeat(`{"x":[1,2,3],"i":1,"g":1}`); ok {
		t.Fatal("short feature vector decoded as valid")
	}
}
