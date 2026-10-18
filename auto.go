package main

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"regexp"
	"sort"
	"strings"
	"sync"
	"time"
)

// Auto is the one row at the top of the stream list that picks for you. The
// list below it stays: auto is a recommendation, not a replacement.
//
// The ranking question is different from the list's. The list answers "which
// releases are good?", so it spreads across resolutions and lets the user
// choose. Auto answers "which one will actually play well tonight?", so
// throughput is a hard gate and the only releases that survive it are ranked
// by picture quality, source tier and track count.

// srcTier is where a release came from, low to high. A CAM is never a
// candidate, however many seeders it has.
type srcTier int

const (
	tierCam srcTier = iota
	tierTv
	tierRip
	tierWeb
	tierDisc
	tierUnknown
)

func (t srcTier) score() float64 {
	switch t {
	case tierTv:
		return 0.50
	case tierRip:
		return 0.65
	case tierWeb:
		return 0.85
	case tierDisc:
		return 1.00
	case tierCam:
		return 0
	default:
		return 0.60
	}
}

func (t srcTier) label() string {
	switch t {
	case tierTv:
		return "HDTV"
	case tierRip:
		return "WEBRip"
	case tierWeb:
		return "WEB-DL"
	case tierDisc:
		return "BluRay"
	case tierCam:
		return "CAM"
	default:
		return "unknown"
	}
}

var (
	// Order matters: check the bad news first. `ts` also catches broadcast
	// .ts captures; for auto that is fine, a transport-stream cap of a
	// channel is exactly the "telecast" being avoided.
	reCamRelease = regexp.MustCompile(`(?i)\b(cam|camrip|hdcam|hqcam|ts|tsrip|telesync|telecine|tc|scr|screener|dvdscr|bdscr|hdts|pdvd|ppvrip|hdcamrip)\b`)
	// 3D releases are half-side-by-side or over-under: a 2D player shows two
	// squashed frames side by side. They are 1080p BluRay by every other
	// measure, which is exactly why auto has to name them and refuse.
	re3DRelease = regexp.MustCompile(`(?i)\b(3d|hsbs|half-sbs|hou|half-ou|sbs)\b`)
	// Anime fansub names carry the source as a bare tag — "[BD][1080p]",
	// "[WEB][1080p]" — so the tag itself has to count.
	reDiscRel = regexp.MustCompile(`(?i)\b(remux|bluray|blu-ray|bdremux|bd25|bd50|bdmv|uhdbd|bdrip|bd)\b`)
	reWebDL   = regexp.MustCompile(`(?i)\b(web-?dl|webdl|web|dsnp|amzn|nf|hmax|atvp|pcok|itunes)\b`)
	reRipRel  = regexp.MustCompile(`(?i)\b(webrip|brrip|dvdrip|hdrip|dvd|xvid|divx)\b`)
	reTvRel   = regexp.MustCompile(`(?i)\b(hdtv|pdtv|dsr|sdtv|tvrip|hdtvrip)\b`)

	reRes2160 = regexp.MustCompile(`(?i)\b(2160p|4k|uhd)\b`)
	reRes1080 = regexp.MustCompile(`(?i)\b1080[pi]\b`)
	reRes720  = regexp.MustCompile(`(?i)\b720p\b`)
	reRes480  = regexp.MustCompile(`(?i)\b(480p|576p|sd)\b`)
)

func classifyTier(title string) srcTier {
	switch {
	case reCamRelease.MatchString(title), re3DRelease.MatchString(title):
		return tierCam
	case reDiscRel.MatchString(title):
		return tierDisc
	case reWebDL.MatchString(title):
		return tierWeb
	case reRipRel.MatchString(title):
		return tierRip
	case reTvRel.MatchString(title):
		return tierTv
	default:
		return tierUnknown
	}
}

func resolutionFromName(title string) int {
	switch {
	case reRes2160.MatchString(title):
		return 2160
	case reRes1080.MatchString(title):
		return 1080
	case reRes720.MatchString(title):
		return 720
	case reRes480.MatchString(title):
		return 480
	}
	return 0
}

func resScore(r int) float64 {
	switch {
	case r >= 2160:
		return 1.00
	case r >= 1080:
		return 0.80
	case r >= 720:
		return 0.50
	case r >= 576:
		return 0.35
	case r >= 480:
		return 0.22
	default:
		return 0.45 // unknown: neither trusted nor punished
	}
}

func clamp01(v float64) float64 {
	if v < 0 {
		return 0
	}
	if v > 1 {
		return 1
	}
	return v
}

// autoCand is one release as auto sees it: the indexer row plus whatever the
// container header and the swarm actually revealed.
type autoCand struct {
	Result
	tier     srcTier
	res      int     // rung, from the name and then corrected by the container
	need     float64 // Mbps required to play this file
	have     float64 // Mbps the swarm plausibly delivers
	tracks   TrackInfo
	probed   bool // tracks came from the container, not the filename
	measured bool // have came from a probe, not from a seeder estimate
	gated    bool // failed the gate on a real measurement
	score    float64
}

// evaluate turns the four factors into one comparable number. The gate lives
// in pickAuto, not here: a release below the line is not "worse", it is not a
// candidate.
func (c *autoCand) evaluate() {
	if c.need <= 0 {
		c.need = 8 // unknown size: assume a mid-bitrate 1080p
	}
	headroom := c.have / c.need

	// Picture quality: what it looks like, weighed by how trustworthy the
	// source is. A 2160p WEBRip is not a 2160p remux.
	quality := 0.55*resScore(c.res) + 0.45*c.tier.score()

	// Reliability: how much slack the swarm has over the bitrate. At the gate
	// (1.2x) this is zero, and it saturates at 2.3x, where a public swarm is as
	// good as it is going to get.
	reliability := clamp01((headroom - 1.2) / 1.1)

	// Options: audio and subtitle tracks, but only once we have measured them.
	// Unprobed candidates sit mid-table rather than being flattered or punished.
	options := 0.40
	if c.probed && !c.tracks.Partial {
		options = 0.65*clamp01(float64(c.tracks.Audio)/3) + 0.35*clamp01(float64(c.tracks.Subs)/6)
	}

	c.score = 100 * (0.55*quality + 0.30*reliability + 0.15*options)
}

// gateHeadroom is the slack a release needs over its own bitrate before auto
// will hand it to a player. Public swarms are bursty; anything closer than this
// stalls on the first slow window.
const (
	gateHeadroom = 1.25
	probeLimit   = 5
	// autoPickTTL is how long a pick is honoured before auto is willing to
	// reconsider. Long enough that a player's byte offset stays valid, short
	// enough that a release which has gone cold is eventually left behind.
	autoPickTTL = 2 * time.Hour
	// stallWindow is how long a demoted release stays demoted for that title.
	stallWindow = 6 * time.Hour
)

// neededMbps is the sustained bitrate a release must deliver to play without
// buffering, from its size and an assumed runtime.
func neededMbps(r Result, typ string, season int) float64 {
	if r.Size <= 0 {
		return 0
	}
	duration := 7200.0 // a feature
	size := r.Size
	if typ == "series" {
		duration = 3000 // ~50 minute episode
		// A batch is one episode's worth of streaming per file.
		if reSeason.MatchString(r.Title) && !reSxxExx.MatchString(r.Title) {
			size /= 12
		}
	}
	return float64(size) * 8 / duration / 1e6
}

// ---- the pick ----

type autoPick struct {
	InfoHash string
	Title    string
	Trackers []string
	Mbps     float64
	Label    string // e.g. "1080p WEB-DL · 3 audio · 12 subs"
	// Measured says the swarm answered a probe for this release. An unmeasured
	// pick gets a short metadata wait: if it cannot even supply that, the
	// player should be told quickly instead of buffering for a minute.
	Measured bool
}

// pickAuto assembles candidates, probes the plausible ones, and returns the
// release auto would play. Probes are the expensive part, so they are limited
// to the top few by cheap score and run in parallel under one deadline.
func (s *Server) pickAuto(ctx context.Context, typ, rawID, baseID string, season, ep int) (autoPick, error) {
	// A pick that is still good is reused. The player holds a byte offset into
	// the file it was handed, so re-deciding on every request can change the
	// release underneath it: the offset then points past the end of a smaller
	// file and playback strands there. Only a stall (recorded by the watchdog,
	// or by a range past EOF) makes auto choose again.
	if ih, label, at, ok := s.store.GetAutoPick(rawID); ok {
		if age := time.Since(time.Unix(at, 0)); age < autoPickTTL && !s.store.StalledHashes(rawID, stallWindow)[ih] {
			s.log.Printf("auto %s: reusing %s (%s, picked %s ago)", rawID, ih[:8], label, age.Round(time.Minute))
			name, _, _, _ := s.store.GetMedia(ih) // a magnet dn hint, if we have one
			return autoPick{InfoHash: ih, Title: name, Label: label}, nil
		}
	}

	// A decision for this item may already be running behind the list. It has
	// the same search results and the same probes, so waiting for it is cheaper
	// than running a second one from scratch.
	s.resolveMu.Lock()
	inflight := s.resolveBusy[rawID]
	s.resolveMu.Unlock()
	if inflight != nil {
		select {
		case <-inflight:
		case <-time.After(10 * time.Second):
		case <-ctx.Done():
			return autoPick{}, ctx.Err()
		}
		if ih, label, at, ok := s.store.GetAutoPick(rawID); ok {
			if time.Since(time.Unix(at, 0)) < autoPickTTL && !s.store.StalledHashes(rawID, stallWindow)[ih] {
				s.log.Printf("auto %s: using the decision made behind the list (%s)", rawID, ih[:8])
				name, _, _, _ := s.store.GetMedia(ih)
				return autoPick{InfoHash: ih, Title: name, Label: label}, nil
			}
		}
	}

	title, year, err := s.resolveMeta(ctx, typ, baseID)
	if err != nil {
		return autoPick{}, fmt.Errorf("meta: %w", err)
	}
	queries := s.streamQueries(ctx, typ, title, year, season, ep)
	results, errs := s.searchAll(ctx, queries, typ, season, ep, baseID)
	for _, e := range errs {
		s.log.Printf("auto %s: %s", rawID, e)
	}
	if len(results) == 0 {
		return autoPick{}, fmt.Errorf("no releases for %q", title)
	}

	cands := s.autoCands(results, typ, rawID, season)
	if len(cands) == 0 {
		return autoPick{}, fmt.Errorf("no candidates for %q", title)
	}
	best := s.decide(ctx, cands, rawID, true)
	if best == nil {
		return autoPick{}, fmt.Errorf("no usable release for %q", title)
	}
	s.store.SaveAutoPick(rawID, best.InfoHash, best.Label(), best.score)
	return autoPick{
		InfoHash: best.InfoHash,
		Title:    best.Title,
		Trackers: best.Trackers,
		Mbps:     best.have,
		Label:    best.Label(),
		Measured: best.measured,
	}, nil
}

// chooseAuto applies the throughput gate and returns the best release that
// clears it, plus the best of all of them as a last resort. onSkip reports the
// ones the gate rejected.
func chooseAuto(cands []*autoCand, onSkip func(*autoCand, float64)) (best, fallback *autoCand) {
	// A release whose throughput was measured is worth more than one whose
	// number came from a seeder count, so measured candidates win outright:
	// an unprobed row that only looks fast on paper must not displace a
	// release the swarm demonstrably fed.
	var measured *autoCand
	for _, c := range cands {
		if c.gated {
			continue
		}
		c.evaluate()
		if fallback == nil || c.score > fallback.score {
			fallback = c
		}
		headroom := c.have / c.need
		if headroom < gateHeadroom {
			if onSkip != nil {
				onSkip(c, headroom)
			}
			continue
		}
		if c.probed {
			if measured == nil || c.score > measured.score {
				measured = c
			}
			continue
		}
		if best == nil || c.score > best.score {
			best = c
		}
	}
	if measured != nil {
		return measured, fallback
	}
	return best, fallback
}

// chooseFallback ranks releases when nothing clears the gate. The question is no
// longer "which is best" but "which is least likely to sit there buffering", so
// headroom leads and unreachable 4K releases are discounted.
func chooseFallback(cands []*autoCand) *autoCand {
	var best *autoCand
	bestScore := -1.0
	for _, c := range cands {
		if c.gated {
			continue
		}
		ratio := c.have / c.need
		// A 4K needs a lot of room to be worth its bitrate; below 1.5x it is
		// the classic 60-second spinner.
		if c.res >= 2160 && ratio < 1.5 {
			ratio *= 0.5
		}
		// Evidence beats a guess: a measured release that is merely tight is a
		// better bet than an unmeasured one that looks comfortable.
		if c.measured {
			ratio *= 1.3
		}
		if ratio > bestScore {
			best, bestScore = c, ratio
		}
	}
	return best
}

// prescore is the cheap ordering used to pick whom to probe: bitrate-gated
// quality only, no network.
func prescore(o *autoCand) float64 {
	headroom := o.have / o.need
	rel := clamp01((headroom - 1.2) / 1.1)
	return 0.55*(0.55*resScore(o.res)+0.45*o.tier.score()) + 0.30*rel
}

// probeCand fills in what the container says. A cached probe is reused for up
// to a day; its measured throughput only for an hour, since that is the part
// that goes stale.
func (s *Server) probeCand(ctx context.Context, c *autoCand) {
	if p, ok := s.store.GetProbe(c.InfoHash); ok {
		s.applyProbe(c, p)
		return
	}
	ti, ok := s.probeAndSave(ctx, c.Result)
	if !ok {
		return
	}
	c.tracks = ti
	c.probed = true
	if h := ti.heightLabel(); h > 0 {
		c.res = h
	}
	if ti.Mbps > 0 {
		c.have = ti.Mbps
		c.measured = true
	}
}

// probeAndSave probes one release and records what it found. Saved probes are
// what make a repeat click cheap.
func (s *Server) probeAndSave(ctx context.Context, res Result) (TrackInfo, bool) {
	ti, ok := s.probeRelease(ctx, res)
	if !ok {
		return ti, false
	}
	idx := -1
	if _, _, files, ok := s.store.GetMedia(res.InfoHash); ok {
		if i, ok := bestCachedFile(files); ok {
			idx = i
		}
	}
	s.store.SaveProbe(res.InfoHash, idx, ti)
	return ti, true
}

// warmProbe fills the probe cache in the background at list time. Warming the
// torrent already opens its metadata, so this is the cheap moment to learn what
// the release actually contains — by the time a row is clicked, auto knows.
func (s *Server) warmProbe(res Result) {
	if _, ok := s.store.GetProbe(res.InfoHash); ok {
		return
	}
	// Two list requests arriving together must not open the same torrent twice.
	s.probeMu.Lock()
	if s.probeBusy[res.InfoHash] {
		s.probeMu.Unlock()
		return
	}
	s.probeBusy[res.InfoHash] = true
	s.probeMu.Unlock()
	defer func() {
		s.probeMu.Lock()
		delete(s.probeBusy, res.InfoHash)
		s.probeMu.Unlock()
	}()
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	ti, ok := s.probeAndSave(ctx, res)
	if !ok {
		return
	}
	s.log.Printf("probe %s: %dx%d video=%d audio=%d subs=%d %.1f Mbps",
		res.InfoHash[:8], ti.Width, ti.Height, ti.Video, ti.Audio, ti.Subs, ti.Mbps)
}

// autoCands turns indexer rows into rankable candidates: cam/3D and demoted
// releases are dropped here, before anything is measured.
func (s *Server) autoCands(results []Result, typ, rawID string, season int) []*autoCand {
	stalled := s.store.StalledHashes(rawID, stallWindow)
	cands := make([]*autoCand, 0, len(results))
	skippedCam, skippedStall := 0, 0
	for _, r := range results {
		tier := classifyTier(r.Title)
		if tier == tierCam {
			skippedCam++
			continue
		}
		if stalled[r.InfoHash] {
			skippedStall++
			continue
		}
		c := &autoCand{
			Result: r,
			tier:   tier,
			res:    resolutionFromName(r.Title),
			need:   neededMbps(r, typ, season),
			have:   estimatedMbps(r.Seeders),
		}
		if c.need <= 0 {
			c.need = 8
		}
		cands = append(cands, c)
	}
	if len(cands) == 0 && (skippedCam > 0 || skippedStall > 0) {
		s.log.Printf("auto %s: no candidates (cam=%d stalled=%d)", rawID, skippedCam, skippedStall)
	}
	return cands
}

// decide ranks candidates, measuring the plausible ones, and returns the
// release to play. probeNow runs the probe batch itself; when false it leans on
// the probes the warm pass already started (resolveAhead's case).
func (s *Server) decide(ctx context.Context, cands []*autoCand, rawID string, probeNow bool) *autoCand {
	// Cheap pre-score on name and swarm to decide who is worth probing.
	sort.SliceStable(cands, func(i, j int) bool { return prescore(cands[i]) > prescore(cands[j]) })

	if probeNow {
		// Probing costs a metadata wait and a few megabytes of swarm traffic, so
		// spend it on releases that could actually win: one already below the
		// gate on its seeder estimate is not worth opening at all.
		probe := make([]*autoCand, 0, probeLimit)
		for _, c := range cands {
			if len(probe) >= probeLimit {
				break
			}
			if c.have/c.need >= 1.0 {
				probe = append(probe, c)
			}
		}
		if len(probe) == 0 {
			// Everything looks starved on paper. Probe a few anyway: the seeder
			// estimate is a guess and the measurement is not.
			n := 3
			if len(cands) < n {
				n = len(cands)
			}
			probe = cands[:n]
		}

		probeCtx, cancel := context.WithTimeout(ctx, 8*time.Second)
		defer cancel()
		var wg sync.WaitGroup
		for _, c := range probe {
			wg.Add(1)
			go func(c *autoCand) {
				defer wg.Done()
				s.probeCand(probeCtx, c)
			}(c)
		}
		wg.Wait()
	} else {
		// Pick up whatever the warm probes already cached.
		for _, c := range cands[:min(len(cands), probeLimit)] {
			if p, ok := s.store.GetProbe(c.InfoHash); ok {
				s.applyProbe(c, p)
			}
		}
	}

	// Rank: gate on throughput, then score.
	best, fallback := chooseAuto(cands, func(c *autoCand, headroom float64) {
		src := ""
		if !c.measured {
			src = " (estimated)"
		}
		s.log.Printf("auto %s: %s at %.1f/%.1f Mbps (%.2fx)%s below gate", rawID, c.Title, c.have, c.need, headroom, src)
	})
	if best == nil {
		// Nothing clears the gate. Take the release most likely to play at all
		// rather than the prettiest one: a 4K WEB-DL that might scrape 25 Mbps
		// is exactly what leaves the player buffering.
		if best = chooseFallback(cands); best == nil {
			best = fallback // every candidate demoted: better to try than to 404
		}
		if best != nil {
			s.log.Printf("auto %s: nothing passed the gate, falling back to %q (%.2fx)", rawID, best.Title, best.have/best.need)
		}
	}
	if best == nil {
		return nil
	}

	// The one release whose throughput really matters is the one about to be
	// streamed. If it won on an estimate, measure it now: that is the difference
	// between a quick start and the player buffering through a cold metadata
	// wait, and if it fails the gate on measurement the next candidate takes it.
	if !best.measured {
		verifyCtx, vcancel := context.WithTimeout(ctx, 8*time.Second)
		s.probeCand(verifyCtx, best)
		vcancel()
		switch {
		case !best.measured:
			// Its own header would not come down: the swarm is cold or dead.
			// Serving it means waiting on metadata that never lands, so hand
			// over a release that did answer instead.
			if alt := chooseFallback(cands); alt != nil && alt != best && alt.measured {
				s.log.Printf("auto %s: %q did not respond to a probe, using %q instead", rawID, best.Title, alt.Title)
				best = alt
			}
		case best.have/best.need < gateHeadroom:
			s.log.Printf("auto %s: %q measured %.1f/%.1f Mbps, below the gate", rawID, best.Title, best.have, best.need)
			best.gated = true
			if alt, _ := chooseAuto(cands, nil); alt != nil {
				s.log.Printf("auto %s: switching to %q", rawID, alt.Title)
				best = alt
			}
		}
	}
	return best
}

// applyProbe copies a cached probe onto a candidate.
func (s *Server) applyProbe(c *autoCand, p Probe) {
	c.tracks = p.TrackInfo
	c.probed = true
	if h := p.heightLabel(); h > 0 {
		c.res = h
	}
	if age := time.Since(time.Unix(p.ProbedAt, 0)); age < time.Hour && p.Mbps > 0 {
		c.have = p.Mbps
		c.measured = true
	}
}

// resolveAhead decides the pick while the stream list is on screen. The search
// has already run and the warm probes are in flight, so a click a few seconds
// later can be served without deciding anything — which is the difference
// between a two-second start and a cold search behind the player's spinner.
func (s *Server) resolveAhead(rawID, typ string, season int, results []Result) {
	if ih, _, at, ok := s.store.GetAutoPick(rawID); ok {
		if time.Since(time.Unix(at, 0)) < autoPickTTL && !s.store.StalledHashes(rawID, stallWindow)[ih] {
			return // a good pick exists; the click will reuse it
		}
	}
	done := make(chan struct{})
	s.resolveMu.Lock()
	if s.resolveBusy == nil {
		s.resolveBusy = map[string]chan struct{}{}
	}
	s.resolveBusy[rawID] = done
	s.resolveMu.Unlock()
	defer func() {
		s.resolveMu.Lock()
		delete(s.resolveBusy, rawID)
		s.resolveMu.Unlock()
		close(done)
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()

	// Give the warm probes a few seconds to land before ranking. Waiting the
	// full budget is worse than ranking early: by the time the user clicks, a
	// late decision is no decision at all.
	for i := 0; i < 8; i++ {
		if s.probeCoverage(results, 2) {
			break
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(time.Second):
		}
	}

	cands := s.autoCands(results, typ, rawID, season)
	if len(cands) == 0 {
		return
	}
	best := s.decide(ctx, cands, rawID, false)
	if best == nil {
		return
	}
	// A click may have decided first; never overwrite a pick that is in use.
	if ih, _, at, ok := s.store.GetAutoPick(rawID); ok {
		if time.Since(time.Unix(at, 0)) < autoPickTTL {
			_ = ih
			return
		}
	}
	s.store.SaveAutoPick(rawID, best.InfoHash, best.Label(), best.score)
	s.log.Printf("auto %s: pre-decided %s (%s)", rawID, best.Title, best.Label())
}

// probeCoverage reports whether at least want of the first few candidates have
// a cached probe yet.
func (s *Server) probeCoverage(results []Result, want int) bool {
	if len(results) == 0 {
		return true
	}
	n := 0
	for i, r := range results {
		if i >= probeLimit+1 {
			break
		}
		if _, ok := s.store.GetProbe(r.InfoHash); ok {
			n++
		}
	}
	return n >= want
}

// Label is what the auto row shows once it has played: the concrete thing that
// was chosen, so the pick is never a black box.
func (c *autoCand) Label() string {
	parts := []string{}
	res := c.res
	if res > 0 {
		parts = append(parts, fmt.Sprintf("%dp", res))
	}
	parts = append(parts, c.tier.label())
	bits := []string{strings.Join(parts, " ")}
	if c.probed && !c.tracks.Partial {
		if c.tracks.Audio > 0 {
			bits = append(bits, fmt.Sprintf("%d audio", c.tracks.Audio))
		}
		if c.tracks.Subs > 0 {
			bits = append(bits, fmt.Sprintf("%d subs", c.tracks.Subs))
		}
		if c.tracks.Container != "" && c.tracks.Container != "?" {
			bits = append(bits, strings.ToUpper(c.tracks.Container))
		}
	}
	// A seeder estimate is marked as such: "40 Mbps" means measured, "~40 Mbps"
	// means we are guessing and the player may find out otherwise.
	switch {
	case c.have >= 100:
		bits = append(bits, "cached") // already on disk; no swarm involved
	case c.have > 0 && c.measured:
		bits = append(bits, fmt.Sprintf("%.0f Mbps", c.have))
	case c.have > 0:
		bits = append(bits, fmt.Sprintf("~%.0f Mbps", c.have))
	}
	return strings.Join(bits, " · ")
}

// ---- HTTP ----

// handleAuto resolves the pick at click time, not list time: that is the moment
// a slow swarm can be measured instead of guessed. It then serves the chosen
// release through the normal play path.
func (s *Server) handleAuto(w http.ResponseWriter, r *http.Request) {
	k, ok := s.authKey(w, r)
	if !ok {
		return
	}
	typ := r.PathValue("type")
	rawID := strings.TrimSuffix(r.PathValue("id"), ".json")
	baseID, season, ep := parseMediaID(rawID)
	if typ != "movie" && typ != "series" {
		httpError(w, 400, "bad type")
		return
	}

	ctx, cancel := context.WithTimeout(r.Context(), 40*time.Second)
	defer cancel()
	pick, err := s.pickAuto(ctx, typ, rawID, baseID, season, ep)
	if err != nil {
		s.log.Printf("auto %s: %v", rawID, err)
		httpError(w, 404, "no playable release")
		return
	}
	s.log.Printf("auto %s: chose %s (%s)", rawID, pick.Title, pick.Label)
	// An unmeasured pick has not proven it can deliver bytes — often because a
	// probe could not even fetch its metadata. Do not make the player wait the
	// full 45s timeout on the strength of a guess: ten seconds, then let the
	// client show an error and the stall be recorded.
	wait := time.Duration(0)
	if !pick.Measured {
		wait = 10 * time.Second
	}
	s.serveTorrent(w, r, k, pick.InfoHash, pick.Title, pick.Trackers, -1, season, ep, rawID, wait)
}

// autoRow is the only list entry. It is a plain HTTP URL, so it works in any
// player, and it resolves its release when the player asks for it.
func (s *Server) autoRow(base string, k Key, typ, rawID string) Stream {
	detail := "picks for this swarm at play time"
	if ih, label, at, ok := s.store.GetAutoPick(rawID); ok && time.Since(time.Unix(at, 0)) < 24*time.Hour {
		_ = ih
		detail = "last: " + label
	}
	return Stream{
		Name:  "Auto",
		Title: detail,
		URL:   fmt.Sprintf("%s/k/%s/auto/%s/%s", base, k.Token, typ, url.PathEscape(rawID)),
		BehaviorHints: map[string]any{
			// A fixed group, so switching episodes stays on auto rather than
			// pinning whichever release the first episode happened to use.
			"bingeGroup":  "bitlab-auto",
			"notWebReady": false,
		},
	}
}
