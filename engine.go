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
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"
)

var errInfoTimeout = errors.New("timed out waiting for torrent metadata (no peers?)")

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
	prebufferMB   int64
	idleTimeout   time.Duration

	mu       sync.Mutex
	addLocks map[string]*sync.Mutex
	act      map[string]*activity
	warmSem  chan struct{}
	log      *log.Logger
	closed   bool

	// storageCloser is nil unless the platform supplies its own storage; the
	// torrent client closes the default one itself.
	storageCloser io.Closer
}

func NewEngine(cfg Config, store *Store, lg *log.Logger) (*Engine, error) {
	cc := torrent.NewDefaultClientConfig()
	cc.DataDir = cfg.DataDir
	cc.Seed = false
	cc.NoUpload = false
	cc.DisableAggressiveUpload = false
	cc.ListenPort = cfg.TorrentPort
	cc.EstablishedConnsPerTorrent = 80
	cc.HalfOpenConnsPerTorrent = 40
	cc.DisableWebtorrent = false
	cc.DisablePEX = false
	if err := os.MkdirAll(cfg.DataDir, 0o755); err != nil {
		return nil, err
	}
	// Platforms that need their own on-disk layout supply storage here; the
	// rest leave it nil and get the client's default file storage.
	storageImpl, storageCloser := defaultStorage(cfg.DataDir)
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
		dataDir:       cfg.DataDir,
		trackers:      cfg.Trackers,
		store:         store,
		maxCacheBytes: cfg.CacheMaxBytes,
		prebufferMB:   cfg.PrebufferMB,
		idleTimeout:   cfg.IdleTimeout,
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

// ensure returns a torrent handle, adding the magnet if it is not loaded yet.
func (e *Engine) ensure(ih, name string, extra []string) (*torrent.Torrent, error) {
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
	t, err := e.cl.AddMagnet(magnetURI(ih, name, trs))
	if err != nil {
		return nil, err
	}
	a := e.activity(ih)
	a.label = name
	return t, nil
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
func (e *Engine) Warm(ih, name string, extra []string, mb int64) {
	select {
	case e.warmSem <- struct{}{}:
	default:
		return
	}
	go func() {
		defer func() { <-e.warmSem }()
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
	n, _ := io.CopyN(io.Discard, r, mb<<20)
	if n > 0 {
		e.log.Printf("prebuffered %s of %s (%s)", humanBytes(n), f.DisplayPath(), t.InfoHash().HexString()[:8])
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
		numbered := 0
		for i, p := range paths {
			if !isVideo(p) {
				continue
			}
			base := filepath.Base(p)
			switch sig := episodeSignal(base, season, ep); {
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
		// Nothing in the release is numbered (a single episode plus an extra, or
		// a film): there is nothing to match, so largest is the best guess.
	}
	return chooseFile(paths, sizes, want)
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
	if len(ih) != 40 {
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

	var extra []string
	if v, ok := s.hints.Load(ih); ok {
		extra = v.([]string)
	}
	t, err := s.engine.ensure(ih, hint, append(extra, r.URL.Query()["tr"]...))
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
	idx, err := chooseFileForEpisode(paths, sizes, wantIdx, seasonWanted, epWanted)
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

// janitor drops idle torrents and keeps the disk cache under budget.
func (e *Engine) janitor() {
	tick := time.NewTicker(30 * time.Second)
	defer tick.Stop()
	evictTick := 0
	for range tick.C {
		e.mu.Lock()
		closed := e.closed
		e.mu.Unlock()
		if closed {
			return
		}
		now := time.Now()
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
				e.mu.Unlock()
			}
		}
		evictTick++
		if evictTick%4 == 0 {
			e.evict()
		}
	}
}

func (e *Engine) evict() {
	if e.maxCacheBytes <= 0 {
		return
	}
	size, err := dirSize(e.dataDir)
	if err != nil || size <= e.maxCacheBytes {
		return
	}
	type cand struct {
		ih   string
		when time.Time
		dir  string
		size int64
	}
	var cands []cand
	for _, t := range e.cl.Torrents() {
		info := t.Info()
		if info == nil || t.BytesCompleted() < t.Length() {
			continue
		}
		ih := t.InfoHash().HexString()
		a := e.activity(ih)
		e.mu.Lock()
		readers := a.readers
		when := a.lastAccess
		e.mu.Unlock()
		if readers > 0 {
			continue
		}
		cands = append(cands, cand{ih: ih, when: when, dir: torrentDirPath(e.dataDir, info), size: t.Length()})
	}
	sort.Slice(cands, func(i, j int) bool { return cands[i].when.Before(cands[j].when) })
	for _, c := range cands {
		if size <= e.maxCacheBytes {
			break
		}
		if t, ok := e.cl.Torrent(metainfo.NewHashFromHex(c.ih)); ok {
			t.Drop()
		}
		base := filepath.Clean(e.dataDir)
		if filepath.Dir(filepath.Clean(c.dir)) == base {
			if err := removeAll(c.dir); err != nil {
				e.log.Printf("evict %s: %v", c.ih[:8], err)
				continue
			}
		}
		e.mu.Lock()
		delete(e.act, c.ih)
		e.mu.Unlock()
		e.store.db.Exec(`DELETE FROM media_cache WHERE infohash=?`, c.ih)
		size -= c.size
		e.log.Printf("evicted %s (%s) to stay under cache budget", c.ih[:8], humanBytes(c.size))
	}
}

// dirSize reports allocated bytes, not apparent size: torrent files are
// sparse while streaming, so the block count is the honest number. How that
// number is obtained is platform-specific (see platform_*.go).
func dirSize(root string) (int64, error) {
	var total int64
	err := filepath.Walk(root, func(path string, info os.FileInfo, err error) error {
		if err != nil || info.IsDir() {
			return nil
		}
		total += allocatedSize(path, info)
		return nil
	})
	return total, err
}

// torrentDirPath is the directory the storage layer keeps a torrent's files
// in. Eviction has to agree with it, or RemoveAll quietly targets nothing.
func torrentDirPath(dataDir string, info *metainfo.Info) string {
	return filepath.Join(dataDir, sanitizePathComponent(info.Name))
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

func (e *Engine) CacheBytes() int64 {
	n, _ := dirSize(e.dataDir)
	return n
}

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
