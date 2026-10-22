package main

// A small neural network that predicts how long a release takes to start
// playing, measured in seconds.
//
// Why a network at all: the inputs are a dozen tabular facts (reported
// seeders, size, resolution, source, indexer, the last measurement of the
// same infohash) and the live behaviour of a swarm is not a straight line in
// any of them. `estimatedMbps` in indexers.go is a hand-fitted curve with
// production numbers baked into a comment; this learns the same relation from
// the probes instead of from one engineer's afternoon of measuring.
//
// Shape: two hidden layers of 24 and 16 units over 16 numeric features plus
// two learned embeddings (indexer, release group), and a heteroscedastic
// output — the mean and variance of log-seconds. The variance is not
// decoration: a point estimate in seconds is a lie on a torrent swarm, and the
// row prints a range instead.
//
// The whole thing is ~2k parameters and trains in well under a second on a few
// thousand probes, so it runs in-process, stores its weights in SQLite, and
// keeps the single-binary deployment. No Python, no ONNX, no sidecar.

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"math/rand"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

const (
	mInNum      = 16
	mIxDim      = 4
	mIxBuckets  = 32
	mGrpDim     = 4
	mGrpBuckets = 256
	mH1         = 24
	mH2         = 16
	mIn         = mInNum + mIxDim + mGrpDim // numeric + both embeddings
)

// Sample is one completed probe in training form.
type Sample struct {
	X      []float32
	CatIx  int
	CatGrp int
	Y      float64 // log(seconds)
	Weight float64 // recency weight; older probes count less
}

// MLP holds the weights, the standardisation constants they were trained with,
// and the layout offsets that let every parameter live in one flat slice.
type MLP struct {
	Mean []float64 `json:"mean"`
	Std  []float64 `json:"std"`
	P    []float64 `json:"p"`

	OffW1   int `json:"off_w1"`
	OffB1   int `json:"off_b1"`
	OffW2   int `json:"off_w2"`
	OffB2   int `json:"off_b2"`
	OffWo   int `json:"off_wo"`
	OffBo   int `json:"off_bo"`
	OffEIx  int `json:"off_eix"`
	OffEGrp int `json:"off_egrp"`

	Samples  int     `json:"samples"`
	MAE      float64 `json:"mae"`
	Within2x float64 `json:"within2x"`
	// ConstantMAE is what the same holdout costs with no model at all: every
	// release predicted at the training mean. It is the number that says
	// whether the network is earning its keep yet, and on a few dozen samples
	// it often is not.
	ConstantMAE    float64 `json:"constant_mae"`
	ConstantWithin float64 `json:"constant_within2x"`
	TrainedAt      int64   `json:"trained_at"`
	Loss           float64 `json:"loss"`
}

func layout(n *MLP) {
	n.OffW1 = 0
	n.OffB1 = n.OffW1 + mH1*mIn
	n.OffW2 = n.OffB1 + mH1
	n.OffB2 = n.OffW2 + mH2*mH1
	n.OffWo = n.OffB2 + mH2
	n.OffBo = n.OffWo + 2*mH2
	n.OffEIx = n.OffBo + 2
	n.OffEGrp = n.OffEIx + mIxBuckets*mIxDim
}

func paramCount() int {
	return (mH1*mIn + mH1) + (mH2*mH1 + mH2) + (2*mH2 + 2) + mIxBuckets*mIxDim + mGrpBuckets*mGrpDim
}

func newMLP(seed int64, meanY float64) *MLP {
	n := &MLP{Mean: make([]float64, mInNum), Std: make([]float64, mInNum)}
	layout(n)
	n.P = make([]float64, paramCount())
	rng := rand.New(rand.NewSource(seed))
	norm := func(dst []float64, fanIn int) {
		s := math.Sqrt(2.0 / float64(fanIn))
		for i := range dst {
			dst[i] = rng.NormFloat64() * s
		}
	}
	norm(n.P[n.OffW1:n.OffB1], mIn)
	norm(n.P[n.OffW2:n.OffB2], mH1)
	norm(n.P[n.OffWo:n.OffBo], mH2)
	// Embeddings start near zero so the numeric path does the early learning.
	for i := n.OffEIx; i < n.OffEGrp; i++ {
		n.P[i] = rng.NormFloat64() * 0.02
	}
	for i := n.OffEGrp; i < len(n.P); i++ {
		n.P[i] = rng.NormFloat64() * 0.02
	}
	// Start the output at the average load time with a wide sigma, so the
	// first epoch is already a sane prediction instead of zero seconds.
	n.P[n.OffBo] = meanY
	n.P[n.OffBo+1] = 0
	return n
}

// ws is a reusable forward-pass workspace: training touches the same sizes
// thousands of times and the allocations are the slow part.
type ws struct {
	v      []float64
	z1, h1 []float64
	z2, h2 []float64
	out    []float64
}

func newWS() *ws {
	return &ws{
		v:   make([]float64, mIn),
		z1:  make([]float64, mH1),
		h1:  make([]float64, mH1),
		z2:  make([]float64, mH2),
		h2:  make([]float64, mH2),
		out: make([]float64, 2),
	}
}

func (n *MLP) input(x []float32, ix, grp int, w *ws) []float64 {
	for i := 0; i < mInNum; i++ {
		w.v[i] = (float64(x[i]) - n.Mean[i]) / n.Std[i]
	}
	copy(w.v[mInNum:mInNum+mIxDim], n.P[n.OffEIx+ix*mIxDim:n.OffEIx+(ix+1)*mIxDim])
	copy(w.v[mInNum+mIxDim:], n.P[n.OffEGrp+grp*mGrpDim:n.OffEGrp+(grp+1)*mGrpDim])
	return w.v
}

// forward returns the two outputs: mu (log seconds) and v (log variance).
func (n *MLP) forward(v []float64, w *ws) (float64, float64) {
	for j := 0; j < mH1; j++ {
		s := n.P[n.OffB1+j]
		row := n.P[n.OffW1+j*mIn : n.OffW1+(j+1)*mIn]
		for i := 0; i < mIn; i++ {
			s += row[i] * v[i]
		}
		w.z1[j] = s
		if s > 0 {
			w.h1[j] = s
		} else {
			w.h1[j] = 0
		}
	}
	for j := 0; j < mH2; j++ {
		s := n.P[n.OffB2+j]
		row := n.P[n.OffW2+j*mH1 : n.OffW2+(j+1)*mH1]
		for i := 0; i < mH1; i++ {
			s += row[i] * w.h1[i]
		}
		w.z2[j] = s
		if s > 0 {
			w.h2[j] = s
		} else {
			w.h2[j] = 0
		}
	}
	for j := 0; j < 2; j++ {
		s := n.P[n.OffBo+j]
		row := n.P[n.OffWo+j*mH2 : n.OffWo+(j+1)*mH2]
		for i := 0; i < mH2; i++ {
			s += row[i] * w.h2[i]
		}
		w.out[j] = s
	}
	return w.out[0], w.out[1]
}

const (
	vFloor = -8.0 // log variance bounds: sigma in [0.018s, 20x]
	vCeil  = 6.0
)

// nll is the Gaussian negative log-likelihood of the target in log space.
func nll(mu, v, y float64) float64 {
	if v < vFloor {
		v = vFloor
	}
	if v > vCeil {
		v = vCeil
	}
	d := y - mu
	return 0.5*d*d/math.Exp(v) + 0.5*v
}

func (n *MLP) zeroGrad(g []float64) {
	for i := range g {
		g[i] = 0
	}
}

// accumulate adds one sample's gradient into g. The caller divides by the
// batch size. weight scales both the loss and the gradient, which is how a
// censored probe counts for less than a measured one.
func (n *MLP) accumulate(g []float64, x []float32, ix, grp int, y, weight float64, w *ws) float64 {
	if weight <= 0 {
		weight = 1
	}
	v := n.input(x, ix, grp, w)
	mu, lv := n.forward(v, w)
	clamped := lv
	if clamped < vFloor {
		clamped = vFloor
	}
	if clamped > vCeil {
		clamped = vCeil
	}
	s2 := math.Exp(clamped)
	d := y - mu
	loss := weight * (0.5*d*d/s2 + 0.5*clamped)

	// dL/dmu and dL/dv. A clamped variance passes no gradient.
	do := [2]float64{-weight * d / s2, 0}
	if lv == clamped {
		do[1] = weight * (0.5 - 0.5*d*d/s2)
	}
	// Clip the output gradient: a single outlier with a confidently wrong
	// variance can otherwise move the whole network in one step.
	gnorm := math.Hypot(do[0], do[1])
	if gnorm > 10 {
		do[0] *= 10 / gnorm
		do[1] *= 10 / gnorm
	}

	dh2 := make([]float64, mH2)
	for j := 0; j < 2; j++ {
		g[n.OffBo+j] += do[j]
		for i := 0; i < mH2; i++ {
			g[n.OffWo+j*mH2+i] += do[j] * w.h2[i]
			dh2[i] += n.P[n.OffWo+j*mH2+i] * do[j]
		}
	}
	dh1 := make([]float64, mH1)
	for j := 0; j < mH2; j++ {
		if w.z2[j] <= 0 {
			continue // ReLU
		}
		g[n.OffB2+j] += dh2[j]
		for i := 0; i < mH1; i++ {
			g[n.OffW2+j*mH1+i] += dh2[j] * w.h1[i]
			dh1[i] += n.P[n.OffW2+j*mH1+i] * dh2[j]
		}
	}
	for j := 0; j < mH1; j++ {
		if w.z1[j] <= 0 {
			continue
		}
		g[n.OffB1+j] += dh1[j]
		for i := 0; i < mIn; i++ {
			g[n.OffW1+j*mIn+i] += dh1[j] * v[i]
		}
	}
	for i := 0; i < mIxDim; i++ {
		for j := 0; j < mH1; j++ {
			if w.z1[j] > 0 {
				g[n.OffEIx+ix*mIxDim+i] += dh1[j] * n.P[n.OffW1+j*mIn+mInNum+i]
			}
		}
	}
	for i := 0; i < mGrpDim; i++ {
		for j := 0; j < mH1; j++ {
			if w.z1[j] > 0 {
				g[n.OffEGrp+grp*mGrpDim+i] += dh1[j] * n.P[n.OffW1+j*mIn+mInNum+mIxDim+i]
			}
		}
	}
	return loss
}

// standardise fills Mean/Std from the training inputs.
func (n *MLP) standardise(xs [][]float32) {
	for k := 0; k < mInNum; k++ {
		var sum float64
		for _, x := range xs {
			sum += float64(x[k])
		}
		m := sum / float64(len(xs))
		var sq float64
		for _, x := range xs {
			d := float64(x[k]) - m
			sq += d * d
		}
		sd := math.Sqrt(sq / float64(len(xs)))
		if sd < 1e-6 {
			sd = 1
		}
		n.Mean[k] = m
		n.Std[k] = sd
	}
}

// TrainMLP fits the network. The split is by sample order after shuffling, and
// the best epoch by held-out loss is kept — with a few hundred rows a network
// this size will overfit if you let it run.
func TrainMLP(samples []Sample, seed int64, epochs int) (*MLP, error) {
	if len(samples) < 20 {
		return nil, fmt.Errorf("need at least 20 probes to train, have %d", len(samples))
	}
	rng := rand.New(rand.NewSource(seed))
	idx := rng.Perm(len(samples))
	shuffled := make([]Sample, len(samples))
	for i, j := range idx {
		shuffled[i] = samples[j]
	}
	valN := len(shuffled) / 7
	if valN < 4 {
		valN = 4
	}
	val := shuffled[:valN]
	train := shuffled[valN:]

	xs := make([][]float32, len(train))
	var meanY, wsum float64
	for i, s := range train {
		xs[i] = s.X
		w := s.Weight
		if w <= 0 {
			w = 1
		}
		meanY += s.Y * w
		wsum += w
	}
	meanY /= wsum

	n := newMLP(seed, meanY)
	n.standardise(xs)

	g := make([]float64, len(n.P))
	m := make([]float64, len(n.P))
	v := make([]float64, len(n.P))
	w := newWS()

	const lr0, lrEnd, beta1, beta2, eps, wd = 0.01, 0.001, 0.9, 0.999, 1e-8, 1e-4
	batch := 32
	if batch > len(train)/2 {
		batch = max(8, len(train)/2)
	}
	bestLoss := math.Inf(1)
	bestP := make([]float64, len(n.P))
	bestEpoch := 0
	step := 0
	for ep := 0; ep < epochs; ep++ {
		frac := float64(ep) / float64(epochs)
		lr := lr0 * math.Pow(lrEnd/lr0, frac)
		perm := rng.Perm(len(train))
		for b := 0; b < len(train); b += batch {
			end := b + batch
			if end > len(train) {
				end = len(train)
			}
			n.zeroGrad(g)
			for _, k := range perm[b:end] {
				s := train[k]
				n.accumulate(g, s.X, s.CatIx, s.CatGrp, s.Y, s.Weight, w)
			}
			scale := 1 / float64(end-b)
			step++
			bc1 := 1 - math.Pow(beta1, float64(step))
			bc2 := 1 - math.Pow(beta2, float64(step))
			for i := range n.P {
				gi := g[i]*scale + wd*n.P[i]
				m[i] = beta1*m[i] + (1-beta1)*gi
				v[i] = beta2*v[i] + (1-beta2)*gi*gi
				n.P[i] -= lr * (m[i] / bc1) / (math.Sqrt(v[i]/bc2) + eps)
			}
		}
		loss := evalLoss(n, val, w)
		if loss < bestLoss-1e-4 {
			bestLoss = loss
			bestEpoch = ep
			copy(bestP, n.P)
		} else if ep-bestEpoch > 40 {
			break
		}
	}
	copy(n.P, bestP)
	n.Loss = bestLoss
	n.Samples = len(samples)
	n.TrainedAt = time.Now().Unix()
	n.measure(val)
	return n, nil
}

func evalLoss(n *MLP, set []Sample, w *ws) float64 {
	var total float64
	for _, s := range set {
		v := n.input(s.X, s.CatIx, s.CatGrp, w)
		mu, lv := n.forward(v, w)
		total += nll(mu, lv, s.Y)
	}
	return total / float64(len(set))
}

// measure fills the human-scale error stats from held-out samples, and the
// same stats for a predictor that ignores every feature.
func (n *MLP) measure(set []Sample) {
	var absErr, within float64
	var baseAbs, baseWithin float64
	// The constant predictor is exp(mean of the training targets), which is
	// where the output bias started before any learning.
	constant := math.Exp(n.P[n.OffBo])
	for _, s := range set {
		p50, _ := n.Predict(s.X, s.CatIx, s.CatGrp)
		want := math.Exp(s.Y)
		absErr += math.Abs(p50 - want)
		if ratio := p50 / math.Max(want, 0.1); ratio >= 0.5 && ratio <= 2 {
			within++
		}
		baseAbs += math.Abs(constant - want)
		if ratio := constant / math.Max(want, 0.1); ratio >= 0.5 && ratio <= 2 {
			baseWithin++
		}
	}
	n.MAE = absErr / float64(len(set))
	n.Within2x = within / float64(len(set))
	n.ConstantMAE = baseAbs / float64(len(set))
	n.ConstantWithin = baseWithin / float64(len(set))
}

// predictRaw returns the network's mean and spread in log-seconds.
func (n *MLP) predictRaw(x []float32, ix, grp int) (mu, sigma float64) {
	w := newWS()
	v := n.input(x, ix, grp, w)
	m, lv := n.forward(v, w)
	if lv < vFloor {
		lv = vFloor
	}
	if lv > vCeil {
		lv = vCeil
	}
	return m, math.Sqrt(math.Exp(lv))
}

// Predict returns the median and p90 load times in seconds.
func (n *MLP) Predict(x []float32, ix, grp int) (float64, float64) {
	mu, sigma := n.predictRaw(x, ix, grp)
	p50 := math.Exp(mu)
	return p50, math.Exp(mu + 1.2816*sigma)
}

func (n *MLP) Marshal() ([]byte, error) { return json.Marshal(n) }

func UnmarshalMLP(b []byte) (*MLP, error) {
	var n MLP
	if err := json.Unmarshal(b, &n); err != nil {
		return nil, err
	}
	layout(&n)
	if len(n.P) != paramCount() || len(n.Mean) != mInNum || len(n.Std) != mInNum {
		return nil, errors.New("model shape does not match this build")
	}
	return &n, nil
}

// ---- features ----

// entryFacts is everything the network may see. Every field is available at
// list time, before anything has been measured: the indexer's reported seeder
// count, not the live swarm, because that is what the row will have when it is
// drawn.
type entryFacts struct {
	Title     string
	Size      int64
	Seeders   int
	Indexer   string
	FirstSeen int64 // unix, when the candidate pool first saw it
	LastProbe int64 // unix, 0 if never
	LastMs    int64 // total load ms at that probe, 0 if never
	Cold      bool  // metadata not held locally at prediction time
	Typ       string
	Season    int
}

func (n *MLP) Features(e entryFacts, now int64) ([]float32, int, int) {
	x := make([]float32, mInNum)
	x[0] = float32(math.Log1p(float64(e.Seeders)))
	x[1] = float32(math.Log1p(float64(e.Size)))
	needMbps := 0.0
	if e.Size > 0 {
		needMbps = float64(e.Size) * 8 / 7200 / 1e6
	}
	x[2] = float32(math.Log1p(needMbps))
	x[3] = float32(resolutionRung(e.Title)) / 2160
	x[4] = float32(sourceTier(e.Title)) / 4
	if reHEVC.MatchString(e.Title) {
		x[5] = 1
	}
	if isPackName(e.Title) && e.Typ == "series" {
		x[6] = 1
	}
	if y := yearInName(e.Title); y > 0 {
		x[7] = float32(y-2000) / 30
	}
	if e.Cold {
		x[8] = 1
	}
	hours := 999.0
	if e.LastProbe > 0 {
		hours = float64(now-e.LastProbe) / 3600
	}
	x[9] = float32(math.Log1p(hours))
	x[10] = float32(math.Log1p(float64(e.LastMs) / 1000))
	if e.FirstSeen > 0 {
		x[11] = float32(math.Log1p(float64(now-e.FirstSeen) / 3600))
	}
	h := float64(now%86400) / 86400 * 2 * math.Pi
	x[12] = float32(math.Sin(h))
	x[13] = float32(math.Cos(h))
	if e.Seeders <= 0 {
		x[14] = 1
	}
	if reCAM.MatchString(e.Title) {
		x[15] = 1
	}
	return x, bucketIndexer(e.Indexer), bucketGroup(releaseGroup(e.Title))
}

func bucketIndexer(s string) int {
	if s == "" {
		return 0
	}
	if i, ok := knownIndexers[s]; ok {
		return i
	}
	return 1 + fnv(s)%(mIxBuckets-2)
}

// knownIndexers pins the configured sources to fixed buckets so the embedding
// learned for "apibay" cannot migrate to a different hash on a rebuild.
var knownIndexers = func() map[string]int {
	names := []string{
		"knaben", "apibay", "nyaa", "eztv", "yts", "animetosho",
		"torrentio/1337x", "torrentio/tgx", "torrentio/rarbg", "torrentio/rutracker",
		"torrentio/magnetdl", "torrentio/prbay", "torrentio/torlock", "torrentio/ololo",
	}
	m := map[string]int{}
	for i, n := range names {
		if i+2 >= mIxBuckets {
			break
		}
		m[n] = i + 2
	}
	return m
}()

func bucketGroup(g string) int {
	if g == "" {
		return 0
	}
	return 1 + fnv(g)%(mGrpBuckets-1)
}

func fnv(s string) int {
	const off, prime = 2166136261, 16777619
	h := uint32(off)
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c >= 'A' && c <= 'Z' {
			c += 'a' - 'A'
		}
		h ^= uint32(c)
		h *= prime
	}
	return int(h & 0x7fffffff)
}

// ---- name facts the network reads ----

var (
	reHEVC      = regexp.MustCompile(`(?i)\b(x26[45]|hevc|h\.?26[45])\b`)
	reCAM       = regexp.MustCompile(`(?i)\b(cam|hdcam|tsrip|telesync|hdts|\bts\b|screener|scr)\b`)
	reBluray    = regexp.MustCompile(`(?i)\b(remux|bluray|blu-ray|bdrip|brrip|bd)\b`)
	reWeb       = regexp.MustCompile(`(?i)\b(web-dl|webdl|webrip|web)\b`)
	reLQ        = regexp.MustCompile(`(?i)\b(hdrip|dvdrip|dvd|hdtv|tvrip)\b`)
	reGroupTail = regexp.MustCompile(`[-_]\s*([A-Za-z][A-Za-z0-9]{1,19})$`)
	reGroupHead = regexp.MustCompile(`^\[([^\]]{2,24})\]`)
	reDigits    = regexp.MustCompile(`^[0-9]+$`)
)

// resolutionRung maps a release name's resolution token to the pixel height a
// user thinks in, the same rungs the container header probe reports.
func resolutionRung(title string) int {
	switch strings.ToLower(reResToken.FindString(title)) {
	case "2160p", "4k", "uhd":
		return 2160
	case "1080p":
		return 1080
	case "720p":
		return 720
	case "480p":
		return 480
	}
	return 0
}

// sourceTier ranks where the picture came from. 0 means "cannot be trusted",
// which is what a cam or a screener is however many seeders it has.
func sourceTier(title string) int {
	switch {
	case reCAM.MatchString(title):
		return 0
	case reBluray.MatchString(title):
		return 3
	case reWeb.MatchString(title):
		return 2
	case reLQ.MatchString(title):
		return 1
	}
	return 1
}

// isPackName reports a season pack: it names a season but no single episode.
func isPackName(title string) bool {
	return reSeason.MatchString(title) && !reSxxExx.MatchString(title)
}

func yearInName(title string) int {
	m := reYearInName.FindString(title)
	if m == "" {
		return 0
	}
	n := 0
	for _, c := range m {
		n = n*10 + int(c-'0')
	}
	return n
}

// releaseGroup is the uploader tag: a leading [Group] on anime, or the token
// after the last dash on scene releases. A trailing token that is a year or a
// resolution is not a group.
func releaseGroup(title string) string {
	name := strings.TrimSpace(title)
	// Only a real container extension is stripped: in a scene name every dot
	// is a separator, so filepath.Ext would cut "…x264-GROUP" down to
	// "…WEB-DL" and report the source as the group.
	if ext := strings.ToLower(filepath.Ext(name)); videoExts[ext] {
		name = strings.TrimSuffix(name, filepath.Ext(name))
	}
	if m := reGroupHead.FindStringSubmatch(name); m != nil {
		return m[1]
	}
	m := reGroupTail.FindStringSubmatch(name)
	if m == nil {
		return ""
	}
	g := m[1]
	if reDigits.MatchString(g) || reYearInName.MatchString(g) {
		return ""
	}
	return g
}
