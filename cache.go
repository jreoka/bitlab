package main

import (
	"io/fs"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/anacrolix/torrent/metainfo"
)

// The cache is every top-level entry under the data directory: a multi-file
// torrent's directory, or a single-file torrent's file (usually still .part).
// The engine keeps no inventory of its own — the filesystem is the truth — so
// a restart cannot desynchronise the budget from what is actually on disk.

// cacheEntry is one top-level item in the data directory.
type cacheEntry struct {
	Name    string
	Path    string
	Bytes   int64 // allocated blocks, not apparent size
	Mod     time.Time
	Dir     bool
	Partial bool // contains at least one .part file
}

// cacheUnit is a cacheEntry plus what the engine knows about its torrent.
type cacheUnit struct {
	cacheEntry
	Infohash string
	Loaded   bool // still handled by the torrent client
	Readers  int
	LastUse  time.Time
}

type cacheDecision struct {
	Unit   cacheUnit
	Reason string // retention, budget or reserve
}

// CacheStats is the cache panel's data: what is on disk, what the rules allow
// and what has been reclaimed since the process started.
type CacheStats struct {
	Used         int64 `json:"used_bytes"`
	Budget       int64 `json:"budget_bytes"`
	Reserve      int64 `json:"reserve_bytes"`
	Free         int64 `json:"free_bytes"`
	Entries      int   `json:"entries"`
	PartialBytes int64 `json:"partial_bytes"`
	OrphanBytes  int64 `json:"orphan_bytes"`
	OrphanCount  int   `json:"orphan_entries"`
	Loaded       int   `json:"loaded_entries"`
	EvictedBytes int64 `json:"evicted_bytes"`
	EvictedCount int64 `json:"evicted_entries"`
	LastSweep    int64 `json:"last_sweep_unix"`
	RetainMin    int   `json:"retain_minutes"`
	GraceMin     int   `json:"grace_minutes"`
}

// cacheSkip reports whether a top-level name is engine state rather than
// media, so neither the budget nor the sweeper touches it.
func cacheSkip(name string) bool {
	switch name {
	case "bitlab.db", "bitlab.db-wal", "bitlab.db-shm", "bitlab.db-journal", "ADMIN_PASSWORD", "lost+found":
		return true
	}
	return name == "" || strings.HasPrefix(name, ".")
}

// scanCache aggregates the data directory into top-level entries. One walk
// produces both the size and the newest mtime of every entry, which is what
// the sweeper needs and saves it from stat-ing the tree repeatedly.
func scanCache(root string) ([]cacheEntry, error) {
	root = filepath.Clean(root)
	type agg struct {
		bytes   int64
		mod     time.Time
		dir     bool
		partial bool
	}
	m := map[string]*agg{}
	err := filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return nil // an unreadable subtree must not abort the whole sweep
		}
		rel, rerr := filepath.Rel(root, p)
		if rerr != nil || rel == "." {
			return nil
		}
		top := rel
		if i := strings.IndexRune(rel, filepath.Separator); i >= 0 {
			top = rel[:i]
		}
		if cacheSkip(top) {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		a := m[top]
		if a == nil {
			a = &agg{}
			m[top] = a
		}
		if d.IsDir() {
			if rel == top {
				a.dir = true
			}
			return nil
		}
		info, ierr := d.Info()
		if ierr != nil {
			return nil
		}
		a.bytes += allocatedSize(p, info)
		if mt := info.ModTime(); mt.After(a.mod) {
			a.mod = mt
		}
		if strings.HasSuffix(d.Name(), ".part") {
			a.partial = true
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	out := make([]cacheEntry, 0, len(m))
	for name, a := range m {
		out = append(out, cacheEntry{
			Name:    name,
			Path:    filepath.Join(root, name),
			Bytes:   a.bytes,
			Mod:     a.mod,
			Dir:     a.dir,
			Partial: a.partial,
		})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out, nil
}

// planCache decides what to delete, without touching the filesystem, so the
// policy can be tested on its own.
//
//   - retention: anything untouched for longer than retain goes, so the disk
//     reflects what has been watched lately rather than everything ever played;
//   - pressure: while the cache is over its budget, or the volume is inside its
//     free-space reserve, the least-recently-used entries go first;
//   - two things are never deleted: an entry with a live reader, and anything
//     touched inside the grace window (a play that just started, or the
//     prebuffer the stream list is filling right now).
//
// free < 0 means the free-space probe failed; the reserve check is skipped
// rather than treating an unknown volume as full.
func planCache(units []cacheUnit, used, budget, reserve, free int64, now time.Time, grace, retain time.Duration) []cacheDecision {
	if grace < 0 {
		grace = 0
	}
	protected := func(u cacheUnit) bool {
		return u.Readers > 0 || now.Sub(u.LastUse) < grace
	}
	ordered := make([]cacheUnit, len(units))
	copy(ordered, units)
	sort.Slice(ordered, func(i, j int) bool { return ordered[i].LastUse.Before(ordered[j].LastUse) })

	var plan []cacheDecision
	taken := map[string]bool{}
	var freed int64

	if retain > 0 {
		for _, u := range ordered {
			if protected(u) || now.Sub(u.LastUse) <= retain {
				continue
			}
			plan = append(plan, cacheDecision{Unit: u, Reason: "retention"})
			taken[u.Name] = true
			freed += u.Bytes
		}
	}
	used -= freed
	if free >= 0 {
		free += freed
	}

	for _, u := range ordered {
		if taken[u.Name] || protected(u) {
			continue
		}
		overBudget := budget > 0 && used > budget
		underReserve := reserve > 0 && free >= 0 && free < reserve
		if !overBudget && !underReserve {
			break
		}
		reason := "budget"
		if !overBudget && underReserve {
			reason = "reserve"
		}
		plan = append(plan, cacheDecision{Unit: u, Reason: reason})
		used -= u.Bytes
		if free >= 0 {
			free += u.Bytes
		}
	}
	return plan
}

// cacheOwners returns the infohashes the client is handling. Storage is one
// directory per infohash, so the entry name is the infohash itself. A torrent
// whose metadata has not arrived yet still owns its directory: media_cache
// remembers the infohash, which closes the window where a starting play looked
// exactly like an abandoned directory.
func (e *Engine) cacheOwners() (owned map[string]bool, pending map[string]bool) {
	owned = map[string]bool{}
	pending = map[string]bool{}
	for _, t := range e.cl.Torrents() {
		ih := t.InfoHash().HexString()
		if t.Info() != nil {
			owned[ih] = true
		} else if _, ok := e.store.MediaName(ih); ok {
			pending[ih] = true
		}
	}
	return owned, pending
}

// peekActivity reads a torrent's state without creating an activity record.
func (e *Engine) peekActivity(ih string) (readers int, last time.Time) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if a, ok := e.act[ih]; ok {
		return a.readers, a.lastAccess
	}
	return 0, time.Time{}
}

// sweepCache applies the policy: it scans what is on disk, works out what is
// disposable and deletes it.
func (e *Engine) sweepCache(now time.Time) {
	entries, err := scanCache(e.dataDir)
	if err != nil {
		e.log.Printf("cache: scan %s: %v", e.dataDir, err)
		return
	}
	owned, pending := e.cacheOwners()

	units := make([]cacheUnit, 0, len(entries))
	var used, partialBytes, orphanBytes int64
	var orphanCount, loaded int
	for _, en := range entries {
		used += en.Bytes
		if en.Partial {
			partialBytes += en.Bytes
		}
		u := cacheUnit{cacheEntry: en, LastUse: en.Mod}
		if owned[en.Name] {
			u.Loaded, u.Infohash = true, en.Name
			loaded++
			readers, last := e.peekActivity(en.Name)
			u.Readers = readers
			if last.After(u.LastUse) {
				u.LastUse = last
			}
		} else if pending[en.Name] {
			// Metadata is in flight; deleting now would race a play that is
			// about to serve these bytes.
			continue
		} else {
			orphanCount++
			orphanBytes += en.Bytes
		}
		units = append(units, u)
	}

	free, ferr := freeDiskSpace(e.dataDir)
	if ferr != nil {
		free = -1
	}
	plan := planCache(units, used, e.maxCacheBytes.Load(), e.reserveBytes, free, now, e.graceFor, e.retainFor)

	var freed int64
	var freedCount int64
	var removedOrphans, removedOrphanBytes, removedPartialBytes int64
	for _, d := range plan {
		if d.Unit.Loaded {
			if t, ok := e.cl.Torrent(metainfo.NewHashFromHex(d.Unit.Infohash)); ok {
				t.Drop()
			}
		}
		if err := removeAll(d.Unit.Path); err != nil {
			e.log.Printf("cache: remove %s: %v", d.Unit.Name, err)
			continue
		}
		if d.Unit.Infohash != "" {
			e.mu.Lock()
			delete(e.act, d.Unit.Infohash)
			// The other drop paths (reapIdle, evictOldest) release the
			// per-torrent add lock with the activity entry; without this
			// the lock map grows by one entry per evicted torrent forever.
			delete(e.addLocks, d.Unit.Infohash)
			e.mu.Unlock()
		} else {
			removedOrphans++
			removedOrphanBytes += d.Unit.Bytes
		}
		if d.Unit.Partial {
			removedPartialBytes += d.Unit.Bytes
		}
		e.store.ForgetMedia(d.Unit.Infohash, "")
		freed += d.Unit.Bytes
		freedCount++
		if d.Reason != "retention" {
			// Retention cleanup is a one-off housekeeping pass and would log
			// hundreds of lines on the first boot; pressure evictions are rare
			// and always worth a line.
			e.log.Printf("cache: evicted %s (%s, %s)", d.Unit.Name, humanBytes(d.Unit.Bytes), d.Reason)
		}
	}

	st := CacheStats{
		Used:         used - freed,
		Budget:       e.maxCacheBytes.Load(),
		Reserve:      e.reserveBytes,
		Free:         free,
		Entries:      len(entries) - int(freedCount),
		PartialBytes: partialBytes - removedPartialBytes,
		OrphanBytes:  orphanBytes - removedOrphanBytes,
		OrphanCount:  orphanCount - int(removedOrphans),
		Loaded:       loaded,
		LastSweep:    now.Unix(),
		RetainMin:    int(e.retainFor.Minutes()),
		GraceMin:     int(e.graceFor.Minutes()),
	}
	e.mu.Lock()
	st.EvictedBytes = e.evictedBytes + freed
	st.EvictedCount = e.evictedCount + freedCount
	e.evictedBytes = st.EvictedBytes
	e.evictedCount = st.EvictedCount
	e.stats = st
	e.mu.Unlock()

	if freedCount > 0 {
		e.log.Printf("cache: reclaimed %s in %d entries (used %s / budget %s, free %s)",
			humanBytes(freed), freedCount, humanBytes(st.Used), humanBytes(e.maxCacheBytes.Load()), humanBytes(free))
	}
}
