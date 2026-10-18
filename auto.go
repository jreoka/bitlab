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
	reDiscRel   = regexp.MustCompile(`(?i)\b(remux|bluray|blu-ray|bdremux|bd25|bd50|bdmv|uhdbd|bdrip)\b`)
	reWebDL     = regexp.MustCompile(`(?i)\b(web-?dl|webdl|dsnp|amzn|nf|hmax|atvp|pcok|itunes)\b`)
	reRipRel    = regexp.MustCompile(`(?i)\b(webrip|brrip|dvdrip|hdrip|dvd|xvid|divx)\b`)
	reTvRel     = regexp.MustCompile(`(?i)\b(hdtv|pdtv|dsr|sdtv|tvrip|hdtvrip)\b`)

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
	if c.probed {
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
}

// pickAuto assembles candidates, probes the plausible ones, and returns the
// release auto would play. Probes are the expensive part, so they are limited
// to the top few by cheap score and run in parallel under one deadline.
func (s *Server) pickAuto(ctx context.Context, typ, rawID, baseID string, season, ep int) (autoPick, error) {
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

	stalled := s.store.StalledHashes(rawID, 6*time.Hour)
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
	if len(cands) == 0 {
		return autoPick{}, fmt.Errorf("no candidates (cam=%d stalled=%d)", skippedCam, skippedStall)
	}

	// Cheap pre-score on name and swarm to decide who is worth probing.
	sort.SliceStable(cands, func(i, j int) bool { return prescore(cands[i]) > prescore(cands[j]) })

	// Probing costs a metadata wait and a few megabytes of swarm traffic, so
	// spend it on releases that could actually win: one already below the gate
	// on its seeder estimate is not worth opening at all.
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

	// Rank: gate on throughput, then score.
	best, fallback := chooseAuto(cands, func(c *autoCand, headroom float64) {
		src := ""
		if !c.measured {
			src = " (estimated)"
		}
		s.log.Printf("auto %s: %s at %.1f/%.1f Mbps (%.2fx)%s below gate", rawID, c.Title, c.have, c.need, headroom, src)
	})
	if best == nil {
		// Nothing clears the gate. Handing over the best-scoring release still
		// beats an error: it may stream slowly, and the watchdog will demote it
		// for the next attempt.
		s.log.Printf("auto %s: nothing passed the gate, falling back to %q", rawID, fallback.Title)
		best = fallback
	}

	// The one release whose throughput really matters is the one about to be
	// streamed. If it won on an estimate, measure it now: this is the difference
	// between a quick start and the player buffering through a cold metadata
	// wait, and if it fails the gate on measurement the next candidate takes it.
	if best != nil && !best.measured {
		verifyCtx, vcancel := context.WithTimeout(ctx, 8*time.Second)
		s.probeCand(verifyCtx, best)
		vcancel()
		if best.measured && best.have/best.need < gateHeadroom {
			s.log.Printf("auto %s: %q measured %.1f/%.1f Mbps, below the gate", rawID, best.Title, best.have, best.need)
			best.gated = true
			if alt, _ := chooseAuto(cands, nil); alt != nil {
				s.log.Printf("auto %s: switching to %q", rawID, alt.Title)
				best = alt
			}
		}
	}

	s.store.SaveAutoPick(rawID, best.InfoHash, best.Label(), best.score)
	return autoPick{
		InfoHash: best.InfoHash,
		Title:    best.Title,
		Trackers: best.Trackers,
		Mbps:     best.have,
		Label:    best.Label(),
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
		c.tracks = p.TrackInfo
		c.probed = true
		if h := p.heightLabel(); h > 0 {
			c.res = h
		}
		if age := time.Since(time.Unix(p.ProbedAt, 0)); age < time.Hour && p.Mbps > 0 {
			c.have = p.Mbps
			c.measured = true
		}
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
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	ti, ok := s.probeAndSave(ctx, res)
	if !ok {
		return
	}
	s.log.Printf("probe %s: %dx%d video=%d audio=%d subs=%d %.1f Mbps",
		res.InfoHash[:8], ti.Width, ti.Height, ti.Video, ti.Audio, ti.Subs, ti.Mbps)
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
	if c.probed {
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
	s.serveTorrent(w, r, k, pick.InfoHash, pick.Title, pick.Trackers, -1, season, ep, rawID)
}

// autoRow is the list entry. It is a plain HTTP URL, so it works in any player.
func (s *Server) autoRow(base string, k Key, typ, rawID, display string) Stream {
	detail := "picks for this swarm at play time"
	if ih, label, at, ok := s.store.GetAutoPick(rawID); ok && time.Since(time.Unix(at, 0)) < 24*time.Hour {
		_ = ih
		detail = "last: " + label
	}
	return Stream{
		Name:  display + " · Auto",
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
