package main

import "testing"

// The bug this exists for: "The Uprising" (2026) played a 23-minute anime
// episode, because a clean episode outscored everything else in the ranking.
func TestFilterRelevantRejectsEpisodesForMovies(t *testing.T) {
	in := []Result{
		{Title: "[Anime Time] The Uprising - 05 [1080p].mkv"},
		{Title: "[Group] The Uprising S01E05 1080p WEB"},
		{Title: "The.Uprising.2026.2160p.WEB-DL.HDR.DDP5.1"},
		{Title: "The Uprising (2026) 1080p BluRay x265"},
	}
	got, dropped := filterRelevant(in, "movie", "The Uprising", nil, 2026, 0)
	if dropped != 2 {
		t.Fatalf("dropped = %d, want 2", dropped)
	}
	if len(got) != 2 {
		t.Fatalf("kept = %d, want 2: %+v", len(got), got)
	}
	for _, r := range got {
		if looksLikeEpisode(r.Title) {
			t.Fatalf("kept an episode: %q", r.Title)
		}
	}
}

func TestFilterRelevantRejectsACloserDifferentTitle(t *testing.T) {
	in := []Result{
		{Title: "Pacific.Rim.Uprising.2018.1080p.BluRay"},
		{Title: "The.Uprising.2026.1080p.WEB-DL"},
	}
	got, dropped := filterRelevant(in, "movie", "The Uprising", nil, 2026, 0)
	if dropped != 1 || len(got) != 1 || got[0].Title != "The.Uprising.2026.1080p.WEB-DL" {
		t.Fatalf("got %+v (dropped %d)", got, dropped)
	}
}

func TestFilterRelevantYearGate(t *testing.T) {
	in := []Result{
		{Title: "Uprising.2014.1080p.BluRay"}, // wrong film, right words
		{Title: "Uprising.2025.1080p.WEB-DL"}, // a year either side is fine
		{Title: "Uprising.1080p.WEB-DL"},      // no year at all: allowed
	}
	got, dropped := filterRelevant(in, "movie", "Uprising", nil, 2026, 0)
	if dropped != 1 {
		t.Fatalf("dropped = %d, want 1 (%+v)", dropped, got)
	}
}

func TestTitleMatches(t *testing.T) {
	cases := []struct {
		name, title string
		want        bool
	}{
		{"The.Uprising.2026.2160p", "The Uprising", true},
		{"Uprising.2026.1080p", "The Uprising", true},        // article dropped by the group
		{"Pacific.Rim.Uprising.2018", "The Uprising", false}, // the trap
		{"The.Uprising.2026", "Pacific Rim Uprising", false},
		{"Severance.S02E01.1080p", "Severance", true},
		{"", "Severance", false},
	}
	for _, c := range cases {
		if got := titleMatches(c.name, c.title); got != c.want {
			t.Errorf("titleMatches(%q, %q) = %v, want %v", c.name, c.title, got, c.want)
		}
	}
}

func TestLooksLikeEpisode(t *testing.T) {
	yes := []string{
		"Show.S01E04.1080p",
		"Show - 04 [720p]",
		"Show 3x04",
		"Show Episode 12",
		"Show Season 2 Complete",
	}
	no := []string{
		"The.Uprising.2026.2160p.WEB-DL",
		"Blade.Runner.2049.1080p",
		"1917.2019.1080p",
	}
	for _, s := range yes {
		if !looksLikeEpisode(s) {
			t.Errorf("looksLikeEpisode(%q) = false, want true", s)
		}
	}
	for _, s := range no {
		if looksLikeEpisode(s) {
			t.Errorf("looksLikeEpisode(%q) = true, want false", s)
		}
	}
}

// The bug this exists for: Spirited Away is filed as 2003 in Cinemeta but its
// releases carry 2001 and most are titled in romaji. Without the alias and a
// wider year window the list came back with 1 row out of 40.
func TestFilterRelevantAnimeMovieAlias(t *testing.T) {
	in := []Result{
		{Title: "Sen.to.Chihiro.no.Kamikakushi.2001.1080p.BluRay.x265"},
		{Title: "Spirited.Away.2003.1080p.BluRay"},
		{Title: "Spirited.Away.1990.1080p.BluRay"}, // far from either year
		{Title: "The.Cat.Returns.2002.1080p"},      // another film entirely
	}
	got, dropped := filterRelevant(in, "movie", "Spirited Away", []string{"Sen to Chihiro no Kamikakushi"}, 2003, 0)
	if dropped != 2 {
		t.Fatalf("dropped = %d, want 2 (%+v)", dropped, got)
	}
	if len(got) != 2 {
		t.Fatalf("kept = %d, want 2: %+v", len(got), got)
	}
	for _, r := range got {
		if r.Title == "Spirited.Away.1990.1080p.BluRay" || r.Title == "The.Cat.Returns.2002.1080p" {
			t.Fatalf("kept a wrong film: %q", r.Title)
		}
	}
}

// Without aliases (a live-action film) the old strictness must remain.
func TestFilterRelevantMovieWithoutAliasesStaysStrict(t *testing.T) {
	in := []Result{
		{Title: "Sen.to.Chihiro.no.Kamikakushi.2001.1080p"},
		{Title: "Spirited.Away.2003.1080p"},
	}
	got, dropped := filterRelevant(in, "movie", "Spirited Away", nil, 2003, 0)
	if dropped != 1 || len(got) != 1 || got[0].Title != "Spirited.Away.2003.1080p" {
		t.Fatalf("got %+v (dropped %d)", got, dropped)
	}
}

// The bug this exists for: Re:Zero S01E04 listed (and played) "[Erai-raws] …
// 4th Season - 04" twice, because "Show - 04" means absolute episode 4 and the
// "4th Season" token was invisible to the episode parser.
func TestReleaseSeasonMarkers(t *testing.T) {
	cases := []struct {
		name string
		want int
		got  int
	}{
		{"[Erai-raws] Re Zero kara Hajimeru Isekai Seikatsu 4th Season - 04 [1080p]", 1, 4},
		{"[Erai-raws] Re Zero kara Hajimeru Isekai Seikatsu 4th Season - 04 [1080p]", 4, 4},
		{"Show 2nd Season Part 2 (39-50)", 2, 2},
		{"Show Season 03 (1080p)", 3, 3},
		{"Show.S04E04.1080p.WEB-DL", 4, 4},
		{"Show S01E04 1080p", 1, 1},
		{"Show (Season 01 + Season 02 + Director's Cut) [BD]", 1, 1},
		{"Show S01-03 + OVAs", 2, 0}, // a range containing the request is a pack
		{"Show S01-03 + OVAs", 4, 1}, // outside the range: it is not season four
		{"Show - 04 [1080p]", 1, 0},  // no season stated at all
		{"The.Uprising.2026.2160p.WEB-DL.DDP5.1", 1, 0},
	}
	for _, c := range cases {
		if got := releaseSeason(c.name, c.want); got != c.got {
			t.Errorf("releaseSeason(%q, want %d) = %d, want %d", c.name, c.want, got, c.got)
		}
	}
}

func TestFilterRelevantDropsAnotherSeasonsEpisode(t *testing.T) {
	in := []Result{
		{Title: "[Erai-raws] Re Zero kara Hajimeru Isekai Seikatsu 4th Season - 04 [1080p CR WEB-DL AVC AAC]"},
		{Title: "[Judas] Re.Zero kara Hajimeru Isekai Seikatsu (Season 2 Part 2) [1080p][HEVC]"},
		{Title: "Re.Zero.Starting.Life.in.Another.World.S01.1080p.BluRay.x264"},
		{Title: "[Anime Time] Re:Zero kara Hajimeru Isekai Seikatsu (Season 01 + Season 02) [BD]"},
	}
	alias := []string{"Re:Zero kara Hajimeru Isekai Seikatsu"}
	got, dropped := filterRelevant(in, "series", "Re: Zero - Starting Life in Another World", alias, 2016, 1)
	if dropped != 2 {
		t.Fatalf("dropped = %d, want 2 (%+v)", dropped, got)
	}
	for _, r := range got {
		if releaseSeason(r.Title, 1) > 1 {
			t.Fatalf("kept another season: %q", r.Title)
		}
	}
	// The same list for season 4 must still offer the season-four episode.
	got4, _ := filterRelevant(in, "series", "Re: Zero - Starting Life in Another World", alias, 2016, 4)
	found := false
	for _, r := range got4 {
		if releaseSeason(r.Title, 4) == 4 {
			found = true
		}
	}
	if !found {
		t.Fatalf("season 4 lost its own episode: %+v", got4)
	}
}

// A franchise name is a search dead end: releases are filed under the part.
func TestTitleForms(t *testing.T) {
	cases := []struct {
		title string
		want  []string
	}{
		{"Re:ZERO -Starting Life in Another World- Memory Snow",
			[]string{"Re:ZERO -Starting Life in Another World- Memory Snow", "Memory Snow"}},
		{"Re: Zero kara Hajimeru Isekai Seikatsu: Hyouketsu no Kizuna",
			[]string{"Re: Zero kara Hajimeru Isekai Seikatsu: Hyouketsu no Kizuna", "Hyouketsu no Kizuna"}},
		{"Spider-Man: Across the Spider-Verse",
			[]string{"Spider-Man: Across the Spider-Verse", "Across the Spider-Verse"}},
		{"Dune: Part Two", []string{"Dune: Part Two"}}, // "Part Two" names a part, not a work
		{"Spirited Away", []string{"Spirited Away"}},   // nothing to split
		{"Spider-Man", []string{"Spider-Man"}},         // a hyphen is not a separator
		{"The Lord of the Rings: The Two Towers",
			[]string{"The Lord of the Rings: The Two Towers", "The Two Towers"}},
	}
	for _, c := range cases {
		got := titleForms(c.title)
		if len(got) != len(c.want) {
			t.Errorf("titleForms(%q) = %q, want %q", c.title, got, c.want)
			continue
		}
		for i := range got {
			if got[i] != c.want[i] {
				t.Errorf("titleForms(%q)[%d] = %q, want %q", c.title, i, got[i], c.want[i])
			}
		}
	}
}

func TestCleanTitleDropsPunctuation(t *testing.T) {
	got := cleanTitle("Re:ZERO -Starting Life in Another World- Memory Snow")
	want := "Re ZERO Starting Life in Another World Memory Snow"
	if got != want {
		t.Fatalf("cleanTitle = %q, want %q", got, want)
	}
}

// Anime trackers list promos, subtitle tracks and rar uploads beside the film.
func TestFilterRelevantDropsNonFeatureUploads(t *testing.T) {
	in := []Result{
		{Title: "[Hanasaku-Emblem] ReZero kara Hajimeru Isekai Seikatsu - Memory Snow PV (720p Hi444PP).mkv"},
		{Title: "[KiruaSubs] Re Zero kara Hajimeru Isekai Seikatsu-Memory Snow OVA.rar"},
		{Title: "[SNuser] ReZero OVA 2 Hyouketsu no Kizuna subs.ass"},
		{Title: "[FLE] Re ZERO Starting Life in Another World - Memory Snow (2018) (BD 1080p HEVC FLAC) [Dual Audio] [287D999E].mkv"},
		{Title: "[SCY] Re:ZERO -Starting Life in Another World- Memory Snow (2018) v2 (BD 1080p x264 10-bit Hi10P FLAC 2.0) [D4B1A6F8].mkv"},
	}
	got, dropped := filterRelevant(in, "movie", "Re:ZERO -Starting Life in Another World- Memory Snow", []string{"Memory Snow"}, 2018, 0)
	if dropped != 3 || len(got) != 2 {
		t.Fatalf("dropped = %d, kept = %d, want 3 and 2: %+v", dropped, len(got), got)
	}
	for _, r := range got {
		if looksLikeNonFeature(r.Title) {
			t.Fatalf("kept a non-feature: %q", r.Title)
		}
	}
}
