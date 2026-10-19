package main

import (
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
)

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
// name carries no year at all, or when the years are within one (festival runs
// and staggered regional releases move the date by a year).
func yearAgrees(name string, year int) bool {
	if year <= 0 {
		return true
	}
	m := reYearInName.FindString(name)
	if m == "" {
		return true
	}
	var got int
	for _, c := range m {
		got = got*10 + int(c-'0')
	}
	diff := got - year
	if diff < 0 {
		diff = -diff
	}
	return diff <= 1
}

// filterRelevant drops candidates that cannot be the thing that was asked for.
// Movies are held to all three checks; series already have their episode numbers
// reconciled elsewhere, so they only lose the obvious cross-title matches.
func filterRelevant(results []Result, typ, title string, year, season int) ([]Result, int) {
	if len(results) == 0 {
		return results, 0
	}
	keep := make([]Result, 0, len(results))
	dropped := 0
	for _, r := range results {
		if !titleMatches(r.Title, title) {
			dropped++
			continue
		}
		if typ == "movie" {
			if looksLikeEpisode(r.Title) {
				dropped++
				continue
			}
			if !yearAgrees(r.Title, year) {
				dropped++
				continue
			}
		} else if season > 1 {
			// A later season must not be served an episode of season one.
			if m := reSxxE.FindStringSubmatch(r.Title); m != nil && atoiSafe(m[1]) != season {
				dropped++
				continue
			}
		}
		keep = append(keep, r)
	}
	return keep, dropped
}

var reSxxE = regexp.MustCompile(`(?i)\bS(\d{1,2})\s*E(\d{1,3})\b`)

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
