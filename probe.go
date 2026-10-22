package main

// Measuring a release before anyone clicks it.
//
// The expensive part of pressing play is not decoding: it is waiting for the
// metadata to arrive from a swarm that may have one peer, then waiting for the
// first piece of whichever file the release carries. Those two waits are what
// the predictor learns, and this file is how they are observed.
//
// The rule that keeps the measurement honest: a probe may only train the model
// when the bytes actually came off the network. A read served from the pod's
// own cache measures the volume, not the swarm, and training on it would teach
// the model that anything recently probed starts instantly — which is true on
// the test set and false on every row the user sees.

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"math/rand"
	"sync"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"
)

const (
	probeReadBytes   = 512 << 10 // enough for a header plus a real throughput sample
	probeMetaWait    = 45 * time.Second
	probeReadWait    = 40 * time.Second
	probeCatCooldown = 2 * time.Hour
)

// Candidate is one release the predictor may measure: a row an indexer
// returned, remembered so the sampler has a population to draw from.
type Candidate struct {
	InfoHash  string
	Title     string
	Indexer   string
	Seeders   int
	Size      int64
	Typ       string
	Season    int
	FirstSeen int64
	LastProbe int64
	LastMs    int64
}

// ProbeRow is one measurement, as stored and as trained on.
type ProbeRow struct {
	InfoHash      string
	At            int64
	MetaMs        int64
	FirstMs       int64
	TotalMs       int64
	Mbps          float64
	Bytes         int64
	Cached        bool
	Cold          bool
	SeedersReport int
	SeedersSeen   int
	Size          int64
	Indexer       string
	Title         string
	OK            bool
	Err           string
	Feat          string
}

// HasInfo reports whether the client already holds this torrent's metadata.
// That is what "cold" means for a player: a hash the pod has never loaded
// pays the metadata wait, one already loaded does not.
func (e *Engine) HasInfo(ih string) bool {
	t, ok := e.cl.Torrent(metainfo.NewHashFromHex(ih))
	if !ok {
		return false
	}
	select {
	case <-t.GotInfo():
		return true
	default:
		return false
	}
}

// peerBytes is the payload this torrent has actually pulled from other peers.
// Read on the probe's own goroutine, so the two samples bracket the read.
func peerBytes(t *torrent.Torrent) int64 {
	st := t.Stats()
	return st.BytesReadUsefulData.Int64()
}

// Probe measures one release without serving it. A torrent with an active
// reader is never touched: the probe would compete with a real stream.
func (e *Engine) Probe(ctx context.Context, c Candidate, feat string) (ProbeRow, bool) {
	row := ProbeRow{
		InfoHash: c.InfoHash, At: time.Now().Unix(), MetaMs: -1, FirstMs: -1,
		TotalMs: -1, SeedersReport: c.Seeders, SeedersSeen: -1,
		Size: c.Size, Indexer: c.Indexer, Title: c.Title, Feat: feat,
	}
	if !validInfohash(c.InfoHash) {
		row.Err = "bad infohash"
		return row, false
	}
	if a := e.activity(c.InfoHash); func() bool {
		e.mu.Lock()
		defer e.mu.Unlock()
		return a.readers > 0
	}() {
		return row, false
	}
	// One probe per hash at a time: two readers on the same torrent halve the
	// throughput each one sees and teach the model twice as little.
	if _, busy := e.probing.LoadOrStore(c.InfoHash, struct{}{}); busy {
		return row, false
	}
	defer e.probing.Delete(c.InfoHash)

	row.Cold = !e.HasInfo(c.InfoHash)
	started := time.Now()
	t, err := e.ensure(c.InfoHash, c.Title, nil)
	if err != nil {
		row.Err = err.Error()
		return row, true
	}
	waitCtx, cancel := context.WithTimeout(ctx, probeMetaWait)
	err = waitInfo(waitCtx, t)
	cancel()
	if err != nil {
		// A release that never produced metadata is a censored observation,
		// not a missing one: the honest label is "still nothing after the
		// wait". Dropping these would train the model only on releases that
		// worked, so it would promise a fast start for a dead swarm.
		row.Err = "metadata: " + err.Error()
		row.TotalMs = time.Since(started).Milliseconds()
		return row, true
	}
	if row.Cold {
		row.MetaMs = time.Since(started).Milliseconds()
	} else {
		row.MetaMs = 0
	}
	e.recordMedia(t)

	files := t.Files()
	if len(files) == 0 {
		row.Err = "no files"
		return row, true
	}
	paths := make([]string, len(files))
	sizes := make([]int64, len(files))
	for i, f := range files {
		paths[i] = f.DisplayPath()
		sizes[i] = f.Length()
	}
	idx, err := chooseFile(paths, sizes, -1)
	if err != nil {
		row.Err = "choose file: " + err.Error()
		return row, true
	}
	f := files[idx]
	// Peer payload bytes, not BytesCompleted: a piece is only "complete" after
	// it hashes, so a fast network read still finishes this probe with the
	// counter behind by the pieces that have not verified yet — which made a
	// genuine swarm read look like a cache hit and threw the sample away.
	before := peerBytes(t)

	rctx, rcancel := context.WithTimeout(ctx, probeReadWait)
	defer rcancel()
	r := f.NewReader()
	r.SetReadahead(2 << 20)
	r.SetResponsive()
	r.SetContext(rctx)
	defer r.Close()

	buf := make([]byte, 64<<10)
	var total int64
	firstAt := time.Now()
	t0 := firstAt
	for total < probeReadBytes {
		n, rerr := r.Read(buf)
		if n > 0 {
			if row.FirstMs < 0 {
				row.FirstMs = time.Since(firstAt).Milliseconds()
			}
			total += int64(n)
		}
		if rerr != nil {
			if row.FirstMs < 0 && rerr != io.EOF {
				row.Err = rerr.Error()
			}
			break
		}
	}
	elapsed := time.Since(t0).Seconds()
	row.Bytes = total
	row.SeedersSeen = t.Stats().ConnectedSeeders
	if total > 0 && elapsed > 0 {
		row.Mbps = float64(total) * 8 / elapsed / 1e6
	}
	// Bytes that did not come off the network are not a swarm measurement.
	row.Cached = peerBytes(t)-before < total/2
	if row.FirstMs >= 0 {
		row.TotalMs = row.FirstMs
		if row.Cold && row.MetaMs > 0 {
			row.TotalMs += row.MetaMs
		}
	}
	row.OK = total >= probeReadBytes/2
	if !row.OK && row.Err == "" {
		row.Err = fmt.Sprintf("short read: %d of %d bytes", total, probeReadBytes)
	}
	if row.TotalMs < 0 {
		// Same censoring rule for a probe that never delivered a byte: the
		// label is how long we waited before giving up.
		row.TotalMs = time.Since(started).Milliseconds()
	}
	return row, true
}

// ---- features at probe time ----

type featBlob struct {
	X []float32 `json:"x"`
	I int       `json:"i"`
	G int       `json:"g"`
}

func encodeFeat(x []float32, ix, grp int) string {
	b, err := json.Marshal(featBlob{X: x, I: ix, G: grp})
	if err != nil {
		return ""
	}
	return string(b)
}

func decodeFeat(s string) (featBlob, bool) {
	var f featBlob
	if s == "" || json.Unmarshal([]byte(s), &f) != nil || len(f.X) != mInNum {
		return f, false
	}
	return f, true
}

// ---- the predictor ----

// ModelStats is what the console and the log line report.
type ModelStats struct {
	Ready      bool    `json:"ready"`
	Samples    int     `json:"samples"`
	MAE        float64 `json:"mae"`
	Within2x   float64 `json:"within2x"`
	TrainedAt  int64   `json:"trained_at"`
	Probes     int     `json:"probes"`
	Hashes     int     `json:"hashes"`
	Pool       int     `json:"pool"`
	LastHour   int     `json:"last_hour"`
	MBLastHour float64 `json:"mb_last_hour"`
	Failed     int     `json:"failed"`
	Budget     int     `json:"budget"`
}

// Predictor owns the background probe loop and the model.
type Predictor struct {
	s *Server

	mu      sync.RWMutex
	net     *MLP
	trained int64
	loaded  int
	since   int

	trainMu sync.Mutex
	closed  chan struct{}
}

func NewPredictor(s *Server) *Predictor {
	p := &Predictor{s: s, closed: make(chan struct{})}
	blob, samples, mae, within, at, ok := s.store.LoadModel()
	if ok {
		if n, err := UnmarshalMLP(blob); err == nil {
			p.net = n
			p.trained = at
			p.loaded = samples
			s.log.Printf("predictor: loaded model (%d probes, MAE %.1fs, %.0f%% within 2x)", samples, mae, within*100)
		} else {
			s.log.Printf("predictor: discarding stored model: %v", err)
		}
	}
	return p
}

// Start loads a first model if there is nothing usable, then runs the loop.
func (p *Predictor) Start(ctx context.Context) {
	go p.run(ctx)
}

func (p *Predictor) Close() {
	select {
	case <-p.closed:
	default:
		close(p.closed)
	}
}

// minSamplesForBadge is the point at which a number on a row is worth showing.
// Below it the model has not seen enough swarms to be better than a guess, and
// a confident-looking green dot would be worse than no dot. It is a floor, not
// a promise: the console reports the held-out error, which is the number that
// says how much to trust the row.
const minSamplesForBadge = 40

// Estimate answers the row's question: how long until this plays, in seconds.
// ok is false while the model has too little data to say anything honest.
func (p *Predictor) Estimate(ih, title, indexer string, seeders int, size int64, typ string, season int) (p50, p90 float64, ok bool) {
	if p == nil || p.s == nil || p.s.engine == nil {
		return 0, 0, false
	}
	p.mu.RLock()
	net := p.net
	p.mu.RUnlock()
	if net == nil || net.Samples < minSamplesForBadge {
		return 0, 0, false
	}
	firstSeen, lastProbe, lastMs, _ := p.s.store.FactsFor(ih)
	e := entryFacts{
		Title: title, Size: size, Seeders: seeders, Indexer: indexer,
		FirstSeen: firstSeen, LastProbe: lastProbe, LastMs: lastMs,
		Cold: !p.s.engine.HasInfo(ih), Typ: typ, Season: season,
	}
	x, ix, grp := net.Features(e, time.Now().Unix())
	mu, sigma := net.predictRaw(x, ix, grp)
	p50 = math.Exp(clampLog(mu))
	p90 = math.Exp(clampLog(mu + 1.2816*sigma))
	if p90 < p50 {
		p90 = p50
	}
	return p50, p90, true
}

// clampLog keeps an untrained or badly-behaved output inside the range a
// person can act on: 0.13s to 665s.
func clampLog(v float64) float64 {
	if v > 6.5 {
		return 6.5
	}
	if v < -2 {
		return -2
	}
	return v
}

// Observe records the releases a search returned so the sampler has a pool.
// This is the only thing a search does for the predictor, and it is what stops
// the model from being trained only on titles Cross happened to click.
func (p *Predictor) Observe(rs []Result, typ string, season int) {
	if p == nil || p.s == nil || p.s.store == nil {
		return
	}
	cs := make([]Candidate, 0, len(rs))
	for _, r := range rs {
		if !validInfohash(r.InfoHash) {
			continue
		}
		cs = append(cs, Candidate{
			InfoHash: r.InfoHash, Title: r.Title, Indexer: r.Indexer,
			Seeders: r.Seeders, Size: r.Size, Typ: typ, Season: season,
		})
	}
	if err := p.s.store.ObserveCandidates(cs); err != nil {
		p.s.log.Printf("predictor: record candidates: %v", err)
	}
}

// ---- the loop ----

func (p *Predictor) run(ctx context.Context) {
	if !p.s.cfg.ProbeEnabled {
		return
	}
	// A restart must not lose the model, but it also must not rebuild it every
	// boot when there is nothing new to learn from.
	if p.net == nil {
		p.maybeTrain(true)
	}
	t := time.NewTicker(p.s.cfg.ProbeInterval)
	defer t.Stop()
	// A short first delay keeps boot fast and lets peers connect: a probe on a
	// cold client measures 0.3 Mbps and would teach the model fiction.
	time.Sleep(45 * time.Second)
	for {
		select {
		case <-ctx.Done():
			return
		case <-p.closed:
			return
		case <-t.C:
			p.tick(ctx)
		}
	}
}

// probeWorkers is how many probes are in flight at once. A probe spends almost
// all of its time waiting on strangers' peers, so two in flight costs nothing
// locally and halves the time a 45-second metadata timeout takes to clear.
const probeWorkers = 2

func (p *Predictor) tick(ctx context.Context) {
	now := time.Now()
	n, bytes := p.s.store.ProbeWindow(now.Unix() - 3600)
	if n >= p.s.cfg.ProbePerHour || bytes >= p.s.cfg.ProbeMBHour<<20 {
		return
	}
	picks := p.pick(probeWorkers)
	if len(picks) == 0 {
		// An empty pool is normal before the first search; say so once in a
		// while rather than every tick.
		if now.Unix()%600 < int64(p.s.cfg.ProbeInterval.Seconds()) {
			p.s.log.Printf("predictor: pool empty — waiting for searches to populate it")
		}
		return
	}
	type outcome struct {
		row       ProbeRow
		attempted bool
	}
	// The probes run concurrently, but rows are written here on one goroutine:
	// the store has a single connection and serialising the writes is both
	// cheaper and easier to reason about than relying on the driver's queue.
	results := make(chan outcome, len(picks))
	for _, c := range picks {
		go func(c Candidate) {
			row, attempted := p.s.engine.Probe(ctx, c, p.featFor(c, now.Unix()))
			results <- outcome{row, attempted}
		}(c)
	}
	for range picks {
		o := <-results
		if !o.attempted {
			continue
		}
		if err := p.s.store.RecordProbe(o.row); err != nil {
			p.s.log.Printf("predictor: record probe: %v", err)
			continue
		}
		p.s.log.Printf("predictor: probed %s %q: cold=%v meta=%dms first=%dms %.1fMbps %s cached=%v ok=%v err=%q",
			shortHash(o.row.InfoHash), truncate(o.row.Title, 60), o.row.Cold, o.row.MetaMs,
			o.row.FirstMs, o.row.Mbps, humanBytes(o.row.Bytes), o.row.Cached, o.row.OK, o.row.Err)
	}
	p.maybeTrain(false)
}

// featFor freezes the feature vector the model would have seen *before* the
// measurement. Storing it now is what keeps training free of leakage: rebuilt
// later, "hours since the last probe" would be zero for every sample.
func (p *Predictor) featFor(c Candidate, now int64) string {
	p.mu.RLock()
	net := p.net
	p.mu.RUnlock()
	e := entryFacts{
		Title: c.Title, Size: c.Size, Seeders: c.Seeders, Indexer: c.Indexer,
		FirstSeen: c.FirstSeen, LastProbe: c.LastProbe, LastMs: c.LastMs,
		Cold: !p.s.engine.HasInfo(c.InfoHash), Typ: c.Typ, Season: c.Season,
	}
	if net == nil {
		// No model yet: standardise with identity so the stored vector is the
		// raw features, and the first trainer can fit its own constants.
		fresh := newMLP(1, 0)
		for i := range fresh.Std {
			fresh.Std[i] = 1
		}
		x, ix, grp := fresh.Features(e, now)
		return encodeFeat(x, ix, grp)
	}
	x, ix, grp := net.Features(e, now)
	return encodeFeat(x, ix, grp)
}

// pick chooses what to measure. Uniform random is the base rate — that is what
// makes the training set look like the list the user sees — with a slice for
// what is being searched right now and a slice for whatever the model is least
// sure about, so the loop spends its budget where the uncertainty is.
func (p *Predictor) pick(n int) []Candidate {
	cutoff := time.Now().Add(-probeCatCooldown).Unix()
	pool, err := p.s.store.ProbePool(cutoff, 200)
	if err != nil || len(pool) == 0 {
		return nil
	}
	recentCut := time.Now().Add(-24 * time.Hour).Unix()
	var recent []Candidate
	for _, c := range pool {
		if c.FirstSeen > recentCut {
			recent = append(recent, c)
		}
	}
	out := make([]Candidate, 0, n)
	used := map[string]bool{}
	take := func(list []Candidate) {
		for _, c := range list {
			if used[c.InfoHash] {
				continue
			}
			used[c.InfoHash] = true
			out = append(out, c)
			return
		}
	}
	for len(out) < n && len(used) < len(pool) {
		switch r := rand.Intn(100); {
		case r < 20 && len(recent) > 0:
			take(recent)
		case r >= 90:
			take(p.uncertain(pool))
		default:
			take(pool)
		}
	}
	return out
}

// uncertain orders a sample of the pool by predicted variance, worst first.
func (p *Predictor) uncertain(pool []Candidate) []Candidate {
	p.mu.RLock()
	net := p.net
	p.mu.RUnlock()
	if net == nil {
		return pool
	}
	now := time.Now().Unix()
	type scored struct {
		c Candidate
		s float64
	}
	sc := make([]scored, 0, len(pool))
	for _, c := range pool {
		e := entryFacts{
			Title: c.Title, Size: c.Size, Seeders: c.Seeders, Indexer: c.Indexer,
			FirstSeen: c.FirstSeen, LastProbe: c.LastProbe, LastMs: c.LastMs,
			Cold: !p.s.engine.HasInfo(c.InfoHash), Typ: c.Typ, Season: c.Season,
		}
		x, ix, grp := net.Features(e, now)
		_, sigma := net.predictRaw(x, ix, grp)
		sc = append(sc, scored{c, sigma})
	}
	// insertion sort: this slice is small and the order is enough
	for i := 1; i < len(sc); i++ {
		for j := i; j > 0 && sc[j].s > sc[j-1].s; j-- {
			sc[j], sc[j-1] = sc[j-1], sc[j]
		}
	}
	out := make([]Candidate, len(sc))
	for i := range sc {
		out[i] = sc[i].c
	}
	return out
}

// ---- training ----

func (p *Predictor) maybeTrain(force bool) {
	rows, err := p.s.store.ProbeTraining(4000)
	if err != nil {
		p.s.log.Printf("predictor: load samples: %v", err)
		return
	}
	p.mu.RLock()
	loaded := p.loaded
	p.mu.RUnlock()
	if len(rows) < 20 {
		return
	}
	if !force && p.net != nil && len(rows) < loaded+15 {
		return
	}
	go func() {
		p.trainMu.Lock()
		defer p.trainMu.Unlock()
		net, err := p.fit(rows)
		if err != nil {
			p.s.log.Printf("predictor: train: %v", err)
			return
		}
		blob, err := net.Marshal()
		if err != nil {
			p.s.log.Printf("predictor: marshal: %v", err)
			return
		}
		if err := p.s.store.SaveModel(blob, net.Samples, net.MAE, net.Within2x); err != nil {
			p.s.log.Printf("predictor: save model: %v", err)
			return
		}
		// Keep the table bounded: 50k probes is years of measurements at the
		// default budget and the model only trains on the newest 4000.
		p.s.store.PruneProbes(50000)
		p.mu.Lock()
		p.net = net
		p.loaded = net.Samples
		p.trained = net.TrainedAt
		p.mu.Unlock()
		p.s.log.Printf("predictor: trained on %d probes — held-out MAE %.1fs, %.0f%% within 2x",
			net.Samples, net.MAE, net.Within2x*100)
	}()
}

func (p *Predictor) fit(rows []ProbeRow) (*MLP, error) {
	now := time.Now().Unix()
	samples := make([]Sample, 0, len(rows))
	for _, r := range rows {
		f, ok := decodeFeat(r.Feat)
		if !ok || r.TotalMs <= 0 {
			continue
		}
		// Recency weighting: a swarm measured a month ago says little about
		// the swarm tonight, and torrent health decays faster than the model
		// retrains.
		age := float64(now-r.At) / 86400
		w := 1 / (1 + age/14)
		if !r.OK {
			// A censored sample: we know it took at least this long and then
			// gave up, not how long it would eventually have taken.
			w *= 0.5
		}
		samples = append(samples, Sample{
			X: f.X, CatIx: f.I, CatGrp: f.G,
			Y: logSeconds(r.TotalMs), Weight: w,
		})
	}
	if len(samples) < 20 {
		return nil, fmt.Errorf("only %d usable samples", len(samples))
	}
	return TrainMLP(samples, 42, 220)
}

func shortHash(ih string) string {
	if len(ih) >= 8 {
		return ih[:8]
	}
	return ih
}

// etaBadge is the row's colour-coded load estimate. A Stremio/Nuvio stream
// title is plain text — there is no CSS to colour — so the colour has to be an
// emoji. The threshold is on the p90: the number a person should plan for, not
// the best case.
func etaBadge(p90 float64) string {
	secs := int(math.Ceil(p90))
	dot := "🟢"
	switch {
	case secs > 30:
		dot = "🔴"
	case secs > 10:
		dot = "🟡"
	}
	return fmt.Sprintf("%s ~%s", dot, shortDuration(secs))
}

func shortDuration(s int) string {
	switch {
	case s < 60:
		return fmt.Sprintf("%ds", s)
	case s < 3600:
		return fmt.Sprintf("%dm", (s+30)/60)
	}
	return fmt.Sprintf("%dh", (s+1800)/3600)
}

func logSeconds(ms int64) float64 {
	// seconds, floored at 0.1 so log stays finite
	s := float64(ms) / 1000
	if s < 0.1 {
		s = 0.1
	}
	return math.Log(s)
}

// Stats is the console's view of the loop.
func (p *Predictor) Stats() ModelStats {
	if p == nil || p.s == nil || p.s.store == nil {
		return ModelStats{}
	}
	st := ModelStats{Budget: p.s.cfg.ProbePerHour}
	n, hashes, okN := p.s.store.ProbeTotals()
	st.Probes, st.Hashes, st.Failed = n, hashes, n-okN
	st.Pool = p.s.store.CandidateCount()
	hourN, hourBytes := p.s.store.ProbeWindow(time.Now().Unix() - 3600)
	st.LastHour = hourN
	st.MBLastHour = float64(hourBytes) / (1 << 20)
	p.mu.RLock()
	defer p.mu.RUnlock()
	if p.net != nil {
		st.Ready = true
		st.Samples = p.net.Samples
		st.MAE = p.net.MAE
		st.Within2x = p.net.Within2x
		st.TrainedAt = p.net.TrainedAt
	}
	return st
}
