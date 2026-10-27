package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"mime"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"
	"github.com/anacrolix/torrent/storage"
)

type activity struct {
	readers       int
	lastAccess    time.Time
	label         string
	size          int64
	rate          float64
	prevBytes     int64
	prevAt        time.Time
	warnedNoPeers bool
}

// Engine owns the BitTorrent client and turns it into an HTTP byte source.
type Engine struct {
	cl            *torrent.Client
	dataDir       string
	trackers      []string
	store         *Store
	maxCacheBytes int64
	reserveBytes  int64
	prebufferMB   int64
	idleTimeout   time.Duration
	graceFor      time.Duration
	retainFor     time.Duration
	maxTorrents   int

	mu           sync.Mutex
	addLocks     map[string]*sync.Mutex
	act          map[string]*activity
	warmSem      chan struct{}
	log          *log.Logger
	closed       bool
	stats        CacheStats
	evictedBytes int64
	evictedCount int64

	// storageCloser is nil unless the platform supplies its own storage; the
	// torrent client closes the default one itself.
	storageCloser io.Closer
}

// baseClientConfig builds the torrent client configuration from the static
// config. Storage and the final data dir are layered on by NewEngine.
func baseClientConfig(cfg Config) *torrent.ClientConfig {
	cc := torrent.NewDefaultClientConfig()
	cc.DataDir = cfg.DataDir
	cc.Seed = false
	cc.NoUpload = false
	cc.DisableAggressiveUpload = false
	cc.ListenPort = cfg.TorrentPort
	cc.EstablishedConnsPerTorrent = 160
	cc.HalfOpenConnsPerTorrent = 100
	cc.DisableWebtorrent = false
	cc.DisablePEX = false
	allowPrivate := os.Getenv("BITLAB_ALLOW_PRIVATE_INDEXERS") == "1"
	// Trackers and metainfo sources are attacker-influenced URLs; they must not
	// become a path to the pod network or cloud metadata.
	cc.TrackerDialContext = guardedDialContext(allowPrivate)

	cc.MetainfoSourcesClient = &http.Client{
		Timeout:   30 * time.Second,
		Transport: guardedTransport(allowPrivate),
	}
	return cc
}

func NewEngine(cfg Config, store *Store, lg *log.Logger) (*Engine, error) {
	cc := baseClientConfig(cfg)
	// Media lives in its own subdirectory. The engine state (bitlab.db and,
	// without an env password, ADMIN_PASSWORD) stays in DataDir, so a torrent
	// file named "bitlab.db" can never resolve to it.
	mediaDir := filepath.Join(cfg.DataDir, "media")
	if err := os.MkdirAll(mediaDir, 0o700); err != nil {
		return nil, err
	}
	_ = os.Chmod(mediaDir, 0o700)
	if err := migrateCacheRoot(cfg.DataDir, mediaDir); err != nil {
		return nil, err
	}
	cc.DataDir = mediaDir
	// One directory per infohash: two releases with the same name cannot
	// overwrite each other's files.
	storageImpl, storageCloser := defaultStorage(mediaDir)
	if storageImpl == nil {
		storageImpl = storage.NewFileByInfoHash(mediaDir)
	}
	if storageImpl != nil {
		cc.DefaultStorage = storageImpl
	}
	cl, err := torrent.NewClient(cc)
	if err != nil {
		if storageCloser != nil {
			storageCloser.Close()
		}
		return nil, fmt.Errorf("torrent client: %w", err)
	}
	e := &Engine{
		cl:            cl,
		storageCloser: storageCloser,
		dataDir:       mediaDir,
		trackers:      cfg.Trackers,
		store:         store,
		maxCacheBytes: cfg.CacheMaxBytes,
		reserveBytes:  cfg.CacheReserveBytes,
		prebufferMB:   cfg.PrebufferMB,
		idleTimeout:   cfg.IdleTimeout,
		graceFor:      cfg.CacheGrace,
		retainFor:     cfg.CacheRetain,
		maxTorrents:   cfg.MaxTorrents,
		addLocks:      map[string]*sync.Mutex{},
		act:           map[string]*activity{},
		warmSem:       make(chan struct{}, 4),
		log:           lg,
	}
	go e.janitor()
	return e, nil
}

func (e *Engine) Close() {
	e.cl.Close()
	if e.storageCloser != nil {
		if err := e.storageCloser.Close(); err != nil {
			e.log.Printf("close storage: %v", err)
		}
	}
}

// AddPeerListener registers an extra inbound peer listener with the torrent
// client (the shared-port mux). Ownership stays with the caller: the client
// does not close listeners added this way.
func (e *Engine) AddPeerListener(l torrent.Listener) {
	e.cl.AddListener(l)
}

// migrateCacheRoot moves pre-existing cache entries from the data root into
// the media root. Engine state and the new media directory are left alone.
func migrateCacheRoot(root, media string) error {
	entries, err := os.ReadDir(root)
	if err != nil {
		return err
	}
	for _, ent := range entries {
		name := ent.Name()
		if name == "media" || name == "ADMIN_PASSWORD" || cacheSkip(name) {
			continue
		}
		src := filepath.Join(root, name)
		dst := filepath.Join(media, name)
		if _, err := os.Stat(dst); err == nil {
			dst += "-migrated"
		}
		if err := os.Rename(src, dst); err != nil {
			return fmt.Errorf("migrate %s: %w", name, err)
		}
	}
	return nil
}

func (e *Engine) lockFor(ih string) *sync.Mutex {
	e.mu.Lock()
	defer e.mu.Unlock()
	l, ok := e.addLocks[ih]
	if !ok {
		l = &sync.Mutex{}
		e.addLocks[ih] = l
	}
	return l
}

func (e *Engine) activity(ih string) *activity {
	e.mu.Lock()
	defer e.mu.Unlock()
	a, ok := e.act[ih]
	if !ok {
		a = &activity{lastAccess: time.Now(), prevAt: time.Now()}
		e.act[ih] = a
	}
	return a
}

// touch records an access to a torrent's bytes. The cache sweeper treats the
// timestamp as the last-use time, so every path that may read must touch.
func (e *Engine) touch(ih, label string) {
	a := e.activity(ih)
	e.mu.Lock()
	if label != "" {
		a.label = label
	}
	a.lastAccess = time.Now()
	e.mu.Unlock()
}

// ensure returns a torrent handle, adding the magnet if it is not loaded yet.
func (e *Engine) ensure(ih, name string, extra []string) (*torrent.Torrent, error) {
	if !validInfohash(ih) {
		return nil, errors.New("invalid infohash")
	}
	e.touch(ih, name)
	h := metainfo.NewHashFromHex(ih)
	if t, ok := e.cl.Torrent(h); ok {
		return t, nil
	}
	l := e.lockFor(ih)
	l.Lock()
	defer l.Unlock()
	if t, ok := e.cl.Torrent(h); ok {
		return t, nil
	}
	trs := append([]string{}, e.trackers...)
	for _, t := range extra {
		if t != "" && !contains(trs, t) {
			trs = append(trs, t)
		}
	}
	if e.maxTorrents > 0 && len(e.cl.Torrents()) >= e.maxTorrents {
		// A valid key should not be able to pin thousands of torrents in memory.
		// Drop the least recently used torrent that nobody is reading.
		if !e.evictOldest() {
			return nil, errors.New("too many active torrents; try again shortly")
		}
	}
	t, err := e.cl.AddMagnet(magnetURI(ih, name, trs))
	if err != nil {
		return nil, err
	}
	return t, nil
}

// evictOldest drops the least recently used torrent with no active readers.
func (e *Engine) evictOldest() bool {
	e.mu.Lock()
	oldest := ""
	var when time.Time
	for ih, a := range e.act {
		if a.readers > 0 {
			continue
		}
		if oldest == "" || a.lastAccess.Before(when) {
			oldest, when = ih, a.lastAccess
		}
	}
	delete(e.act, oldest)
	delete(e.addLocks, oldest)
	e.mu.Unlock()
	if oldest == "" {
		return false
	}
	if t, ok := e.cl.Torrent(metainfo.NewHashFromHex(oldest)); ok {
		t.Drop()
	}
	return true
}

func contains(list []string, s string) bool {
	for _, v := range list {
		if v == s {
			return true
		}
	}
	return false
}

func waitInfo(ctx context.Context, t *torrent.Torrent) error {
	select {
	case <-t.GotInfo():
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// Warm pulls metadata in the background so the click that follows is instant.
// streamBusy reports whether any play reader is open right now. Stream-first
// policy: background warming (metadata fetch and prebuffering) never competes
// with an active stream for peers or bandwidth, so anything speculative
// checks this before pulling bytes.
func (e *Engine) streamBusy() bool {
	e.mu.Lock()
	defer e.mu.Unlock()
	for _, a := range e.act {
		if a.readers > 0 {
			return true
		}
	}
	return false
}

func (e *Engine) Warm(ih, name string, extra []string, mb int64) {
	if !validInfohash(ih) {
		return
	}
	if e.streamBusy() {
		e.log.Printf("warm %s skipped: a stream is active (stream-first)", ih[:8])
		return
	}
	select {
	case e.warmSem <- struct{}{}:
	default:
		return
	}
	go func() {
		defer func() { <-e.warmSem }()
		defer func() {
			if r := recover(); r != nil {
				e.log.Printf("warm %s panicked: %v", ih, r)
			}
		}()
		ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
		defer cancel()
		t, err := e.ensure(ih, name, extra)
		if err != nil {
			return
		}
		if err := waitInfo(ctx, t); err != nil {
			return
		}
		e.recordMedia(t)
		if e.streamBusy() {
			// A stream started while the metadata was on its way in;
			// do not follow through with the prebuffer.
			return
		}
		e.prebuffer(t, mb)
	}()
}

// prebuffer pulls the head of the most likely file so the click that follows
// starts from local bytes instead of waiting on the swarm.
func (e *Engine) prebuffer(t *torrent.Torrent, mb int64) {
	if mb <= 0 {
		return
	}
	files := t.Files()
	if len(files) == 0 {
		return
	}
	paths := make([]string, len(files))
	sizes := make([]int64, len(files))
	for i, f := range files {
		paths[i] = f.DisplayPath()
		sizes[i] = f.Length()
	}
	idx, err := chooseFile(paths, sizes, -1)
	if err != nil {
		return
	}
	f := files[idx]
	r := f.NewReader()
	defer r.Close()
	r.SetReadahead(8 << 20)
	r.SetResponsive()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	r.SetContext(ctx)
	// Pull in chunks and bail the moment a stream starts: a prebuffer that
	// blindly finishes its full pull would compete with the very playback
	// it was warming up for (stream-first).
	var n int64
	buf := make([]byte, 1<<20)
	target := mb << 20
	for n < target && ctx.Err() == nil {
		if e.streamBusy() {
			break
		}
		nr, rerr := r.Read(buf)
		n += int64(nr)
		if rerr != nil {
			break
		}
	}
	if n > 0 {
		e.log.Printf("prebuffered %s of %q (%s)", humanBytes(n), f.DisplayPath(), t.InfoHash().HexString()[:8])
	}
}

func (e *Engine) recordMedia(t *torrent.Torrent) {
	info := t.Info()
	if info == nil {
		return
	}
	var files []CachedFile
	for i, f := range t.Files() {
		if !isVideo(f.DisplayPath()) {
			continue
		}
		files = append(files, CachedFile{Idx: i, Path: f.DisplayPath(), Size: f.Length()})
	}
	e.store.SaveMedia(t.InfoHash().HexString(), info.Name, t.Length(), files)
}

func isVideo(p string) bool {
	ext := strings.ToLower(filepath.Ext(p))
	return videoExts[ext]
}

// chooseFile picks the file to serve: an explicit index if valid, else the
// largest video file, skipping samples. Pure so it can be tested directly.
// chooseFileForEpisode prefers the file whose name carries the requested
// episode, which is how a season pack gets played correctly.
func chooseFileForEpisode(paths []string, sizes []int64, want, season, ep int) (int, error) {
	if want >= 0 && want < len(paths) {
		return want, nil
	}
	if ep > 0 {
		numbered, videos := 0, 0
		for i, p := range paths {
			if !isVideo(p) {
				continue
			}
			videos++
			switch sig := fileSignal(p, season, ep); {
			case sig > 0:
				return i, nil
			case sig < 0:
				numbered++ // this file states an episode, just not this one
			}
		}
		// Files that name their episodes, none of which is the one asked for:
		// the release does not contain this episode and must not fall through to
		// "largest file" — that plays an arbitrary episode, which is how a
		// season-1 pack answered a request for season 3.
		if numbered > 0 {
			return -1, errNoEpisode
		}
		// A batch whose files say nothing about their episodes (named by title,
		// or numbered in a convention we cannot read): picking the largest is a
		// coin flip between the show's episodes, so refuse instead.
		if videos > 3 {
			return -1, errNoEpisode
		}
		// One episode plus an extra, or a film: nothing to match, largest wins.
	}
	return chooseFile(paths, sizes, want)
}

// reFileEpNum is a bare episode number at the head of a file name — "04.mkv",
// "01 - Title.mkv", "04v2 [1080p].mkv". Years (four digits) and resolutions
// (a letter after the digits) cannot match, and four-digit numbers never do.
var reFileEpNum = regexp.MustCompile(`^(?:\[[^\]]*\]\s*)*0*(\d{1,3})(?:v\d)?\s*(?:[.\-_ ]|$)`)

// fileSignal judges one file inside a torrent. A file name is not a release
// title — it may be a bare number, which episodeSignal deliberately does not
// accept on a title because "86" and "24" are show names.
func fileSignal(path string, season, ep int) int {
	if ep <= 0 {
		return 0
	}
	base := filepath.Base(path)
	if sig := episodeSignal(base, season, ep); sig != 0 {
		return sig
	}
	if m := reFileEpNum.FindStringSubmatch(base); m != nil {
		if n, _ := strconv.Atoi(m[1]); n == ep {
			return 1
		}
		return -1
	}
	return 0
}

// errNoEpisode means the release does not contain the requested episode.
var errNoEpisode = errors.New("release has no file for this episode")

func chooseFile(paths []string, sizes []int64, want int) (int, error) {
	if len(paths) == 0 {
		return 0, errors.New("torrent has no files")
	}
	if want >= 0 && want < len(paths) {
		return want, nil
	}
	best := -1
	for i, p := range paths {
		lp := strings.ToLower(p)
		if !isVideo(lp) {
			continue
		}
		if strings.Contains(lp, "sample") && len(paths) > 1 {
			continue
		}
		if best < 0 || sizes[i] > sizes[best] {
			best = i
		}
	}
	if best < 0 {
		for i := range paths {
			if best < 0 || sizes[i] > sizes[best] {
				best = i
			}
		}
	}
	if best < 0 {
		return 0, errors.New("no playable file found")
	}
	return best, nil
}

// handlePlay streams one file out of a torrent with full Range support.
func (s *Server) handlePlay(w http.ResponseWriter, r *http.Request) {
	key, ok := s.authKey(w, r)
	if !ok {
		return
	}
	ih := strings.ToLower(r.PathValue("infohash"))
	if !validInfohash(ih) {
		httpError(w, 400, "bad infohash")
		return
	}
	hint := r.URL.Query().Get("n")
	wantIdx := -1
	if f := r.URL.Query().Get("f"); f != "" {
		if n, err := strconv.Atoi(f); err == nil {
			wantIdx = n
		}
	}
	seasonWanted, epWanted := 0, 0
	if v := r.URL.Query().Get("s"); v != "" {
		seasonWanted, _ = strconv.Atoi(v)
	}
	if v := r.URL.Query().Get("e"); v != "" {
		epWanted, _ = strconv.Atoi(v)
	}
	if !s.validPlayToken(r, key.ID, ih, seasonWanted, epWanted) {
		httpError(w, 403, "play link expired or not issued for this key — reopen the stream list")
		return
	}

	var extra []string
	if v, ok := s.hints.Load(ih); ok {
		extra = v.([]string)
	}
	// Client-supplied ?tr= is deliberately ignored: it let any key holder make
	// the torrent client announce to arbitrary internal addresses. Trackers
	// come from the configured list and the indexer result that produced the
	// row, both filtered when stored.
	t, err := s.engine.ensure(ih, hint, extra)
	if err != nil {
		httpError(w, 500, "torrent: "+err.Error())
		return
	}

	// Metadata is usually already warm by the time a player asks.
	ctx := r.Context()
	infoCtx, cancel := context.WithTimeout(ctx, s.cfg.InfoTimeout)
	defer cancel()
	if err := waitInfo(infoCtx, t); err != nil {
		httpError(w, 504, "waiting for metadata: "+err.Error())
		return
	}
	s.engine.recordMedia(t)

	allFiles := t.Files()
	paths := make([]string, len(allFiles))
	sizes := make([]int64, len(allFiles))
	for i, ff := range allFiles {
		paths[i] = ff.DisplayPath()
		sizes[i] = ff.Length()
	}
	// A movie row carries the film's names instead of an episode: the release may
	// hold several films and the play handler has to pick this one.
	titles := r.URL.Query()["t"]
	var idx int
	if len(titles) > 0 && epWanted == 0 {
		idx, err = chooseFileForTitle(paths, sizes, -1, titles)
	} else {
		idx, err = chooseFileForEpisode(paths, sizes, wantIdx, seasonWanted, epWanted)
	}
	if err != nil {
		if errors.Is(err, errNoEpisode) {
			// Deterministic, not transient: this release can never serve this
			// episode, so say so instead of playing a different one.
			s.log.Printf("play %s S%02dE%02d: %v", ih[:8], seasonWanted, epWanted, err)
		}
		httpError(w, 404, err.Error())
		return
	}
	f := allFiles[idx]

	// A range past the end cannot be served. The release is fine; the player is
	// holding a byte offset from a different file (it switched sources or rows
	// with the player open), so say exactly that.
	if start, ok := rangeStart(r.Header.Get("Range")); ok && start >= f.Length() {
		s.log.Printf("play %s: range starts at %d but this file is %s — the player is holding an offset from another release; reopening the episode resets it",
			ih[:8], start, humanBytes(f.Length()))
	}

	act := s.engine.activity(ih)
	s.engine.mu.Lock()
	act.readers++
	act.lastAccess = time.Now()
	act.size = f.Length()
	s.engine.mu.Unlock()
	defer func() {
		s.engine.mu.Lock()
		act.readers--
		act.lastAccess = time.Now()
		s.engine.mu.Unlock()
	}()

	reader := f.NewReader()
	reader.SetReadahead(s.cfg.ReadaheadBytes)
	reader.SetResponsive()
	reader.SetContext(ctx)
	defer reader.Close()

	name := filepath.Base(f.DisplayPath())
	ct := mime.TypeByExtension(filepath.Ext(name))
	if ct == "" || ct == "application/octet-stream" {
		// Many releases ship files without an extension. Sniff the container so
		// players pick the right decoder instead of falling back to octet-stream.
		hdr := make([]byte, 16)
		if _, rerr := io.ReadFull(reader, hdr); rerr == nil {
			if ext, sniffed := sniffContainer(hdr); sniffed != "" {
				ct = sniffed
				if filepath.Ext(name) == "" {
					name += ext
				}
			}
		}
		reader.Seek(0, io.SeekStart)
	}
	if ct == "" {
		ct = "application/octet-stream"
	}
	// Anything that is not a video is refused rather than served inline: a
	// torrent can contain HTML or SVG, and this origin holds the admin session.
	if !strings.HasPrefix(ct, "video/") {
		httpError(w, 415, "this file is not a playable video")
		return
	}
	w.Header().Set("Content-Type", ct)
	w.Header().Set("Content-Disposition", fmt.Sprintf("inline; filename=%q", name))
	w.Header().Set("Accept-Ranges", "bytes")
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("X-Bitlab-File", strconv.Itoa(idx))

	cw := &countingWriter{ResponseWriter: w, n: &s.bytesOut}
	// ServeContent probes the size with Seek(0, End) before writing anything.
	// Answering that arithmetically avoids telling the torrent client to
	// prioritise the tail of the file, which delays the first byte.
	http.ServeContent(cw, r, name, time.Time{}, &sizedReader{r: reader, size: f.Length()})
	s.store.AddKeyBytes(key.ID, cw.written)
	s.log.Printf("play %s key=%s file=%d served=%s range=%q", ih[:8], key.ID, idx, humanBytes(cw.written), r.Header.Get("Range"))
}

// rangeStart reads the first byte offset out of a Range header.
func rangeStart(h string) (int64, bool) {
	h = strings.TrimSpace(h)
	if !strings.HasPrefix(h, "bytes=") {
		return 0, false
	}
	spec := strings.TrimSpace(strings.TrimPrefix(h, "bytes="))
	if i := strings.IndexByte(spec, ','); i >= 0 {
		spec = spec[:i] // a player asking for several ranges gets the first
	}
	dash := strings.IndexByte(spec, '-')
	if dash <= 0 {
		return 0, false
	}
	n, err := strconv.ParseInt(strings.TrimSpace(spec[:dash]), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

type countingWriter struct {
	http.ResponseWriter
	n       *int64
	written int64
}

func (c *countingWriter) Write(p []byte) (int, error) {
	n, err := c.ResponseWriter.Write(p)
	c.written += int64(n)
	return n, err
}

// janitor keeps the torrent client and the disk tidy: it drops torrents that
// nobody is reading and periodically applies the cache policy.
func (e *Engine) janitor() {
	tick := time.NewTicker(30 * time.Second)
	defer tick.Stop()
	sweeps := 0
	for range tick.C {
		e.mu.Lock()
		closed := e.closed
		e.mu.Unlock()
		if closed {
			return
		}
		now := time.Now()
		e.reapIdle(now)
		if sweeps == 0 || sweeps%4 == 0 {
			e.sweepCache(now)
		}
		sweeps++
	}
}

// reapIdle drops torrents with no readers after the idle timeout, and tracks
// their throughput for the console. Dropping a handle stops the swarm traffic
// but keeps the bytes on disk; whether they survive is the cache policy's
// call, not the janitor's.
func (e *Engine) reapIdle(now time.Time) {
	for _, t := range e.cl.Torrents() {
		ih := t.InfoHash().HexString()
		a := e.activity(ih)
		e.mu.Lock()
		if t.Info() != nil {
			a.size = t.Length()
		}
		if a.prevAt.Before(now) {
			dt := now.Sub(a.prevAt).Seconds()
			if dt > 0 {
				cur := t.BytesCompleted()
				a.rate = float64(cur-a.prevBytes) / dt
				a.prevBytes = cur
			}
		}
		a.prevAt = now
		idle := now.Sub(a.lastAccess)
		readers := a.readers
		e.mu.Unlock()

		if readers > 0 && a.size > 0 && t.Stats().ActivePeers == 0 && now.Sub(a.lastAccess) > 15*time.Second && !a.warnedNoPeers {
			a.warnedNoPeers = true
			e.log.Printf("no peers for %s after %s (trackers=%d)", ih[:8], time.Since(a.lastAccess).Round(time.Second), len(e.trackers))
		}
		if readers == 0 && idle > e.idleTimeout {
			e.log.Printf("drop idle torrent %s (%s idle)", ih[:8], idle.Round(time.Second))
			t.Drop()
			e.mu.Lock()
			delete(e.act, ih)
			delete(e.addLocks, ih)
			e.mu.Unlock()
		}
	}
}

type TorrentView struct {
	InfoHash    string  `json:"infohash"`
	Name        string  `json:"name"`
	Size        int64   `json:"size"`
	Completed   int64   `json:"completed"`
	Progress    float64 `json:"progress"`
	Peers       int     `json:"peers"`
	Seeders     int     `json:"seeders"`
	Rate        float64 `json:"rate"`
	Readers     int     `json:"readers"`
	IdleSeconds int64   `json:"idle_seconds"`
}

func (e *Engine) Views() []TorrentView {
	var out []TorrentView
	for _, t := range e.cl.Torrents() {
		ih := t.InfoHash().HexString()
		a := e.activity(ih)
		st := t.Stats()
		e.mu.Lock()
		v := TorrentView{
			InfoHash:    ih,
			Readers:     a.readers,
			Rate:        a.rate,
			IdleSeconds: int64(time.Since(a.lastAccess).Seconds()),
			Name:        a.label,
		}
		e.mu.Unlock()
		if info := t.Info(); info != nil {
			v.Name = info.Name
		}
		v.Size = t.Length()
		v.Completed = t.BytesCompleted()
		if v.Size > 0 {
			v.Progress = float64(v.Completed) / float64(v.Size)
		}
		v.Peers = st.ActivePeers
		v.Seeders = st.ConnectedSeeders
		out = append(out, v)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].IdleSeconds < out[j].IdleSeconds })
	return out
}

// CacheBytes is the media footprint of the data directory, in allocated
// blocks. The database files are not media and are not counted.
func (e *Engine) CacheBytes() int64 {
	entries, err := scanCache(e.dataDir)
	if err != nil {
		return 0
	}
	var n int64
	for _, en := range entries {
		n += en.Bytes
	}
	return n
}

// CacheStats is what the console shows: the last sweep's picture of the cache,
// with the live disk numbers refreshed on request.
func (e *Engine) CacheStats() CacheStats {
	var used, partial int64
	entries, err := scanCache(e.dataDir)
	if err == nil {
		for _, en := range entries {
			used += en.Bytes
			if en.Partial {
				partial += en.Bytes
			}
		}
	}
	e.mu.Lock()
	st := e.stats
	e.mu.Unlock()
	st.Used = used
	if st.LastSweep == 0 && err == nil {
		// Until the first sweep lands, show the scan's own picture rather than
		// an empty panel.
		st.Entries = len(entries)
		st.PartialBytes = partial
	}
	st.Budget = e.maxCacheBytes
	st.Reserve = e.reserveBytes
	if free, ferr := freeDiskSpace(e.dataDir); ferr == nil {
		st.Free = free
	} else {
		st.Free = -1
	}
	st.RetainMin = int(e.retainFor.Minutes())
	st.GraceMin = int(e.graceFor.Minutes())
	return st
}

// DiskFree is the volume's available bytes, or -1 when the platform cannot
// report it. The health endpoints use this instead of a full cache scan.
func (e *Engine) DiskFree() int64 {
	n, err := freeDiskSpace(e.dataDir)
	if err != nil {
		return -1
	}
	return n
}

// TorrentCount is how many handles the client currently holds.
func (e *Engine) TorrentCount() int { return len(e.cl.Torrents()) }

// sizedReader answers size queries locally so the first read goes to byte 0.
type sizedReader struct {
	r    io.ReadSeeker
	size int64
	pos  int64
}

func (s *sizedReader) Read(p []byte) (int, error) {
	if s.pos >= s.size {
		return 0, io.EOF
	}
	if int64(len(p)) > s.size-s.pos {
		p = p[:s.size-s.pos]
	}
	n, err := s.r.Read(p)
	s.pos += int64(n)
	return n, err
}

func (s *sizedReader) Seek(off int64, whence int) (int64, error) {
	var target int64
	switch whence {
	case io.SeekStart:
		target = off
	case io.SeekCurrent:
		target = s.pos + off
	case io.SeekEnd:
		target = s.size + off
	default:
		return 0, errors.New("bad whence")
	}
	if target < 0 {
		return 0, errors.New("negative seek")
	}
	if target > s.size {
		target = s.size
	}
	if target != s.pos {
		if _, err := s.r.Seek(target, io.SeekStart); err != nil {
			return 0, err
		}
		s.pos = target
	}
	return s.pos, nil
}

// sniffContainer identifies a video container from its first bytes.
func sniffContainer(b []byte) (ext, mimeType string) {
	if len(b) >= 8 && string(b[4:8]) == "ftyp" {
		brand := strings.TrimSpace(string(b[8:min(12, len(b))]))
		if strings.HasPrefix(brand, "qt") {
			return ".mov", "video/quicktime"
		}
		return ".mp4", "video/mp4"
	}
	if len(b) >= 4 && b[0] == 0x1a && b[1] == 0x45 && b[2] == 0xdf && b[3] == 0xa3 {
		return ".mkv", "video/x-matroska"
	}
	if len(b) >= 12 && string(b[0:4]) == "RIFF" && string(b[8:12]) == "AVI " {
		return ".avi", "video/x-msvideo"
	}
	if len(b) >= 4 && string(b[0:4]) == "OggS" {
		return ".ogv", "video/ogg"
	}
	if len(b) >= 4 && string(b[0:3]) == "FLV" {
		return ".flv", "video/x-flv"
	}
	if len(b) >= 1 && b[0] == 0x47 {
		return ".ts", "video/mp2t"
	}
	if len(b) >= 4 && b[0] == 0x00 && b[1] == 0x00 && b[2] == 0x01 && (b[3] == 0xba || b[3] == 0xb3) {
		return ".mpg", "video/mpeg"
	}
	return "", ""
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// chooseFileForTitle picks the film a movie torrent was asked for. A release can
// hold several films — an OVA pack, a trilogy — and "largest file" then plays
// whichever happens to be biggest, which is how Memory Snow played The Frozen
// Bond. Words every file carries (the franchise line) say nothing about which
// film a file is, so only the discriminating ones count.
func chooseFileForTitle(paths []string, sizes []int64, want int, hints []string) (int, error) {
	if want >= 0 && want < len(paths) {
		return want, nil
	}
	words := hintWords(strings.Join(hints, " "))
	if len(words) == 0 {
		return chooseFile(paths, sizes, want)
	}
	sets := make([]map[string]bool, len(paths))
	for i, p := range paths {
		if !isVideo(p) {
			continue
		}
		set := map[string]bool{}
		for _, w := range reNonAlnum.Split(strings.ToLower(filepath.Base(p)), -1) {
			if w != "" {
				set[w] = true
			}
		}
		sets[i] = set
	}
	disc := make([]string, 0, len(words))
	for _, w := range words {
		inAll, any := true, false
		for _, set := range sets {
			if set == nil {
				continue
			}
			any = true
			if !set[w] {
				inAll = false
				break
			}
		}
		if any && !inAll {
			disc = append(disc, w)
		}
	}
	best, bestHits := -1, 0
	for i, set := range sets {
		if set == nil {
			continue
		}
		hits := 0
		for _, w := range disc {
			if set[w] {
				hits++
			}
		}
		if hits > bestHits || (hits == bestHits && best >= 0 && hits > 0 && sizes[i] > sizes[best]) {
			best, bestHits = i, hits
		}
	}
	if best >= 0 && bestHits > 0 {
		return best, nil
	}
	return chooseFile(paths, sizes, want)
}
