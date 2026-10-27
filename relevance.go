package main

import (
	"fmt"
	"regexp"
	"strings"
)

// Indexers answer a search with whatever their fuzzy matcher liked, and a movie
// with a common title can come back as an episode of an unrelated show: asking
// for "The Uprising" (2026) once returned a 23-minute anime episode. The scores
// cannot fix that, because a clean 1080p episode outranks a cam rip. So the
// candidates are gated before scoring, on three things: is this an episode at
// all, does the name actually carry the title, and does the year agree.

var (
	reEpMarker = []*regexp.Regexp{
		regexp.MustCompile(`(?i)\bS\d{1,2}\s*E\d{1,3}\b`),
		regexp.MustCompile(`(?i)\b\d{1,2}x\d{2}\b`),
		regexp.MustCompile(`(?i)\bepisode[.\s_-]*\d{1,4}\b`),
		regexp.MustCompile(`(?i)\bseason[.\s_-]*\d{1,2}\b`),
		regexp.MustCompile(`(?i)\bcomplete[.\s_-]+(series|season)\b`),
		// "Show - 04" is how anime is usually numbered.
		regexp.MustCompile(`\s-\s\d{2,4}(\s|$)`),
	}
	reYearInName = regexp.MustCompile(`\b(19|20)\d{2}\b`)
	reNonAlnum   = regexp.MustCompile(`[^a-z0-9]+`)
	// A promo clip is not the film, and anime trackers file these beside it:
	// "[Hanasaku-Emblem] … Memory Snow PV (720p Hi444PP).mkv".
	rePromo = regexp.MustCompile(`(?i)(?:^|[\s._\-\[\(])(pv|ncop|nced|creditless)(?:[\s._\-\]\)]|$)`)
	// A release that announces itself as a film: "… The Movie - …",
	// "… Movie: …", the romaji "gekijouban". An episode or a season pack
	// never calls itself that, whatever franchise it belongs to.
	reMovie = regexp.MustCompile(`(?i)\bmovie\b|\bgekijo+ban\b|\bfeature[\s._-]+film\b`)
	// The narrower announcement, for contexts that do not know the show's
	// name: bare "movie" is also a file name ("movie.mkv") and a word in
	// unrelated titles, but "the movie" and "gekijouban" only ever label
	// a film.
	reAnnouncedFilm = regexp.MustCompile(`(?i)\bthe[\s._-]+movie\b|\bgekijo+ban\b`)
	// An archive or a subtitle track holds no video at all.
	reNonVideo = regexp.MustCompile(`(?i)\.(rar|zip|7z|ass|srt|ssa|sub|idx|iso|mka|mp3|flac|jpg|jpeg|png)$`)
)

// looksLikeNonFeature reports a release that can never play as the film: a
// promo clip, an archive or a subtitle upload. Only whole tokens and a trailing
// extension count, so a release that merely mentions FLAC or PV inside its name
// ("[Dual Audio] … FLAC") stays.
func looksLikeNonFeature(name string) bool {
	return rePromo.MatchString(name) || reNonVideo.MatchString(name)
}

// looksLikeMovie reports a release that announces itself as a film rather
// than an episode or a pack of episodes.
func looksLikeMovie(name string) bool {
	return reMovie.MatchString(name)
}

// announcesFilm is looksLikeMovie's narrower form: only the phrases that
// unambiguously label a film, safe to apply to a name whose show is unknown.
func announcesFilm(name string) bool {
	return reAnnouncedFilm.MatchString(name)
}

// normName flattens a release name to lowercase letters and digits, so
// "The.Uprising.2026.2160p.WEB-DL" and "The Uprising (2026)" compare.
func normName(s string) string {
	return reNonAlnum.ReplaceAllString(strings.ToLower(s), "")
}

// looksLikeEpisode reports whether a name carries an episode marker of any of
// the conventions indexers use.
func looksLikeEpisode(name string) bool {
	for _, re := range reEpMarker {
		if re.MatchString(name) {
			return true
		}
	}
	return false
}

// significantWords drops articles and short filler, which never distinguish one
// title from another.
func significantWords(title string) []string {
	var out []string
	for _, w := range reNonAlnum.Split(strings.ToLower(title), -1) {
		if len(w) >= 3 && w != "the" && w != "and" && w != "of" {
			out = append(out, w)
		}
	}
	return out
}

// titleMatches is deliberately conservative: either the whole normalised title
// appears, or every significant word does *and* the name starts with one of
// them. That second clause is what stops "Pacific Rim: Uprising" standing in for
// "The Uprising".
func titleMatches(name, title string) bool {
	want := normName(title)
	if want == "" {
		return true
	}
	have := normName(name)
	if strings.Contains(have, want) {
		return true
	}
	words := significantWords(title)
	if len(words) == 0 {
		return true
	}
	for _, w := range words {
		if !strings.Contains(have, w) {
			return false
		}
	}
	first := significantWords(name)
	if len(first) == 0 {
		return false
	}
	return first[0] == words[0]
}

// yearAgrees accepts a release when the requested year is unknown, when the
// name carries no year at all, or when any year in the name is within slack
// (festival runs and staggered regional releases move the date by a year;
// anime films commonly differ by two or three between Japan and the West).
// Any year may match, not just the first: a work's own title can be a
// year-shaped number ("1917", "Blade Runner 2049"), and that token must not
// veto a release whose real year follows it in the same name.
func yearAgrees(name string, year, slack int) bool {
	if year <= 0 {
		return true
	}
	found := false
	for _, m := range reYearInName.FindAllString(name, -1) {
		found = true
		var got int
		for _, c := range m {
			got = got*10 + int(c-'0')
		}
		diff := got - year
		if diff < 0 {
			diff = -diff
		}
		if diff <= slack {
			return true
		}
	}
	return !found
}

// filterRelevant drops candidates that cannot be the thing that was asked for.
// Movies are held to all three checks; series already have their episode numbers
// reconciled elsewhere, so they only lose the obvious cross-title matches.
//
// aliases are the other names an anime is released under. A release titled in
// romaji has to be allowed to stand for the English title the client asked for,
// and its year may be the Japanese release date — hence the wider slack when
// aliases are in play.
func filterRelevant(results []Result, typ, title string, aliases []string, year, season int) ([]Result, int) {
	if len(results) == 0 {
		return results, 0
	}
	titles := append([]string{title}, aliases...)
	slack := 1
	if len(aliases) > 0 {
		slack = 3
	}
	keep := make([]Result, 0, len(results))
	dropped := 0
	for _, r := range results {
		if looksLikeNonFeature(r.Title) {
			dropped++
			continue
		}
		matched := false
		for _, t := range titles {
			if titleMatches(r.Title, t) {
				matched = true
				break
			}
		}
		if !matched {
			dropped++
			continue
		}
		if typ == "movie" {
			if looksLikeEpisode(r.Title) {
				dropped++
				continue
			}
			if !yearAgrees(r.Title, year, slack) {
				dropped++
				continue
			}
		} else if season > 0 {
			// A film from the same franchise is not an episode of it. This
			// is the movie gate above, mirrored: asking for Demon Slayer
			// S01E01 put three Infinity Castle releases in the top rows
			// and the click played the film, because a movie title states
			// no episode number for the episode checks to reject — and
			// "states nothing" is also what a season pack looks like, so
			// the film was kept as a possible pack.
			if looksLikeMovie(r.Title) {
				dropped++
				continue
			}
			// A release that names its season must be for the season asked
			// for. Season one needs the check as much as later seasons do:
			// "… 4th Season - 04" was served as S01E04 for a year.
			if s := releaseSeason(r.Title, season); s > 0 && s != season {
				dropped++
				continue
			}
			// A second work in the same franchise files itself under the
			// show's name plus its own subtitle, then reuses the numbering
			// from one: anime arcs are released as their own series, so
			// "Kimetsu no Yaiba Yuukaku-hen S01" and "… Hashira Training
			// Arc S01E01" both claim exactly the season and episode asked
			// for, and every number check passes them. What gives them
			// away is the subtitle sitting *before* the number: an
			// episode's own title comes after its marker ("S01E01
			// Cruelty", "S02E05 Birdland"), a different work's title
			// comes before it. Demon Slayer S01E01's top row was the
			// Yuukaku-hen pack, and the click played that arc's first
			// episode — the follow-on from the Mugen Train film, not
			// S01E01.
			if extraTitleWordsBeforeMarker(r.Title, titles) >= 2 {
				dropped++
				continue
			}
			// A release nothing ties to the season asked for — no episode
			// number, no season marker, no range holding the season —
			// gets no benefit of the doubt on the two things that do
			// identify it: the year it carries and any second title.
			if !identifiedForSeason(r.Title, season) {
				// Season one of a 2019 show is not a 2025 release. Later
				// seasons air later, so the window widens with the season;
				// a release that names its episode or season is exempt,
				// because that marker outweighs a year.
				if !yearAgrees(r.Title, year, slack+(season-1)*3) {
					dropped++
					continue
				}
				// Words past the show's own name are another work's
				// title: "… Infinity Castle", "… Mugen Train". An
				// unmarked batch of the show itself has nothing left
				// over once its name and the release tags are removed.
				if extraTitleWords(r.Title, titles) >= 2 {
					dropped++
					continue
				}
			}
		}
		keep = append(keep, r)
	}
	return keep, dropped
}

// Season markers come in three shapes ("S04", "Season 4", "4th Season") and
// anime release groups use the last one constantly: "[Erai-raws] Re Zero kara
// Hajimeru Isekai Seikatsu 4th Season - 04" is a season-four episode that the
// absolute-numbering rule ("Show - 04" is episode 4) happily served as S01E04.
var (
	reSeasonRange   = regexp.MustCompile(`(?i)\bS(\d{1,2})\s*[-–—]\s*(?:S)?(\d{1,2})\b`)
	reSeasonOrdinal = regexp.MustCompile(`(?i)\b(\d{1,2})(?:st|nd|rd|th)[\s._-]+season\b`)
	reSeasonLong    = regexp.MustCompile(`(?i)\bseason[\s._-]*(\d{1,2})\b`)
	reSeasonShort   = regexp.MustCompile(`(?i)\bS(\d{1,2})(?:[\s._-]*E\d{1,3})?\b`)
)

// releaseSeason reports the season a release name states. It returns 0 when the
// name says nothing about a season, or when it names a range that contains the
// wanted one ("S01-S03" is a usable batch for any of them).
func releaseSeason(name string, want int) int {
	if m := reSeasonRange.FindStringSubmatch(name); m != nil {
		lo, hi := atoiSafe(m[1]), atoiSafe(m[2])
		if lo > hi {
			lo, hi = hi, lo
		}
		if want > 0 && want >= lo && want <= hi {
			return 0
		}
		return lo
	}
	for _, re := range []*regexp.Regexp{reSeasonOrdinal, reSeasonLong, reSeasonShort} {
		if m := re.FindStringSubmatch(name); m != nil {
			if s := atoiSafe(m[1]); s > 0 {
				return s
			}
		}
	}
	return 0
}

// identifiedForSeason reports whether a release name states anything tying
// it to the season asked for: an episode marker of any convention, a season
// marker for that season, or a season range that contains it. A batch titled
// with nothing but the show's name is unidentified, however good it may be.
func identifiedForSeason(name string, season int) bool {
	if looksLikeEpisode(name) {
		return true
	}
	if releaseSeason(name, season) != 0 {
		return true
	}
	if m := reSeasonRange.FindStringSubmatch(name); m != nil {
		lo, hi := atoiSafe(m[1]), atoiSafe(m[2])
		if lo > hi {
			lo, hi = hi, lo
		}
		return season >= lo && season <= hi
	}
	return false
}

// Release furniture: tags that say how a release was made or packed, never
// what it is called. Anything left after these, the show's own names and the
// bracketed tags are removed is part of a title.
var releaseFurniture = map[string]bool{
	"bluray": true, "bdrip": true, "brrip": true, "webrip": true, "hdtv": true,
	"web": true, "dl": true, "rip": true, "xvid": true, "divx": true, "hevc": true,
	"avc": true, "remux": true, "proper": true, "repack": true, "internal": true,
	"limited": true, "extended": true, "uncut": true, "remastered": true,
	"dual": true, "audio": true, "multi": true, "subs": true, "subbed": true,
	"dubbed": true, "hardcoded": true, "complete": true, "series": true,
	"batch": true, "pack": true, "collection": true, "tv": true,
	"www": true, "com": true, "org": true, "net": true,
	"nf": true, "amzn": true, "dsnp": true, "hmax": true, "atvp": true, "cr": true,
}

var (
	// Bracketed and parenthesised segments are tags ([1080p], (2019),
	// [Group]), not title text.
	reBracketed = regexp.MustCompile(`\[[^\]]*\]|\([^)]*\)`)
	reVideoExt  = regexp.MustCompile(`(?i)\.(mkv|mp4|avi|mov|m4v|webm|ts|wmv|mpg|mpeg|flv)$`)
	// Scene and p2p names end in "-Group" once the extension is off.
	reTrailingGroup = regexp.MustCompile(`-[A-Za-z0-9]+$`)
)

// extraTitleWords counts the distinct title-like words in a release name
// once the show's own names (title and aliases), its bracketed tags, its
// extension, its trailing release group and the release furniture are
// removed. Two or more left over means the name carries a second title.
func extraTitleWords(name string, titles []string) int {
	s := reBracketed.ReplaceAllString(name, " ")
	s = reVideoExt.ReplaceAllString(s, "")
	s = reTrailingGroup.ReplaceAllString(s, "")
	own := map[string]bool{}
	for _, t := range titles {
		for _, w := range hintWords(t) {
			own[w] = true
		}
	}
	extra := map[string]bool{}
	for _, w := range hintWords(s) {
		if own[w] || releaseFurniture[w] || strings.ContainsAny(w, "0123456789") {
			continue
		}
		extra[w] = true
	}
	return len(extra)
}

// The points at which a release stops naming the work and starts numbering
// it: an episode or season marker, the anime dash number, or a year. A
// second title lives before the first of these; an episode title lives
// after its own marker and is never counted here.
var (
	reDashNumMarker = regexp.MustCompile(`\s-\s\d{2,4}(?:\s|$)`)
	reXEpMarker     = regexp.MustCompile(`(?i)\b\d{1,2}x\d{2}\b`)
	reNumMarkers    = []*regexp.Regexp{
		reSxxExx, reDashEp, reEpOnly, reXEpMarker,
		reSeasonRange, reSeasonOrdinal, reSeasonLong, reSeasonShort,
		reDashNumMarker, reYearInName,
	}
)

// extraTitleWordsBeforeMarker counts the second-title words a release name
// carries ahead of its first numbering marker, with the same accounting as
// extraTitleWords. A name with no marker at all reports 0: the gates for
// unidentified releases already judge the whole of those.
func extraTitleWordsBeforeMarker(name string, titles []string) int {
	cut := len(name)
	for _, re := range reNumMarkers {
		if loc := re.FindStringIndex(name); loc != nil && loc[0] < cut {
			cut = loc[0]
		}
	}
	if cut == len(name) {
		return 0
	}
	return extraTitleWords(name[:cut], titles)
}

func atoiSafe(s string) int {
	n := 0
	for _, c := range s {
		if c < '0' || c > '9' {
			return n
		}
		n = n*10 + int(c-'0')
	}
	return n
}

// reTitleSep splits a franchise name from the work it is selling: a colon, or a
// dash with whitespace on one side. "Spider-Man" has neither and stays whole;
// "Re:ZERO -Starting Life in Another World- Memory Snow" splits into four.
var reTitleSep = regexp.MustCompile(`\s*:\s*|\s+-+\s*|\s*-+\s+`)

// reGenericTail rejects a trailing segment that names a part rather than a work,
// so "Dune: Part Two" never becomes a search for "Part Two".
var reGenericTail = regexp.MustCompile(`(?i)^(part|chapter|volume|vol|season|episode|movie|the movie)\b`)

// titleForms lists the names a film is filed under: the Cinemeta title, then the
// part after the franchise separator when that part stands on its own.
func titleForms(title string) []string {
	title = strings.TrimSpace(title)
	forms := []string{title}
	last := ""
	for _, s := range reTitleSep.Split(title, -1) {
		if s = strings.TrimSpace(s); s != "" {
			last = s
		}
	}
	if !strings.EqualFold(last, title) && len(last) >= 6 &&
		len(strings.Fields(last)) >= 2 && !reGenericTail.MatchString(last) {
		forms = append(forms, last)
	}
	return forms
}

// reNonWord flattens punctuation while keeping case, so a cleaned query still
// reads like a title.
var reNonWord = regexp.MustCompile(`[^A-Za-z0-9]+`)

// cleanTitle is the title with the punctuation indexers do not match on
// removed. "Re:ZERO -Starting Life in Another World- Memory Snow" returns zero
// hits from every indexer; the same words separated by spaces return dozens.
func cleanTitle(title string) string {
	return strings.Join(strings.Fields(reNonWord.ReplaceAllString(title, " ")), " ")
}

// withYear appends the year when there is one.
func withYear(q string, year int) string {
	if year > 0 {
		return fmt.Sprintf("%s %d", q, year)
	}
	return q
}

// dedupeStrings drops blanks and case-insensitive repeats, keeping order — the
// first queries are the ones small indexers are allowed to run.
func dedupeStrings(in []string) []string {
	seen := make(map[string]bool, len(in))
	out := in[:0]
	for _, s := range in {
		s = strings.TrimSpace(s)
		if s == "" || seen[strings.ToLower(s)] {
			continue
		}
		seen[strings.ToLower(s)] = true
		out = append(out, s)
	}
	return out
}

// hintWords are the words a release name would use for a title. Two-letter
// words are kept because "Part II" and "Part III" differ only there.
func hintWords(s string) []string {
	var out []string
	for _, w := range reNonAlnum.Split(strings.ToLower(s), -1) {
		switch w {
		case "", "the", "and", "of", "to", "in", "a", "an":
			continue
		}
		if len(w) < 2 {
			continue
		}
		out = append(out, w)
	}
	return out
}
