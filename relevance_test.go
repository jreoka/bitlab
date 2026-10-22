package main

import (
	"strings"
	"testing"
)

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

// The bug this exists for: Demon Slayer S01E01 (tt9335498:1:1, 2026-10-22)
// listed three Infinity Castle film releases in its top rows — one of them
// prebuffered first — and the click played the film. A movie title states no
// episode number, so every episode check read it as "unknown", and "unknown"
// is kept in case it is a season pack. The three rows below are the exact
// titles from the production log.
func TestFilterRelevantDropsFranchiseFilmsForSeries(t *testing.T) {
	in := []Result{
		// The row that was clicked and played.
		{Title: "Demon.Slayer.Kimetsu.No.Yaiba.Infinity.Castle.2025.1080p.WEB-DL.HCSubbed.AAC.H.264-Cassu.mkv"},
		{Title: "[Antrophiss] Kimetsu no Yaiba The Movie - Infinity Castle Part 1 1080p HEVC 10bit (WEB-DL)"},
		{Title: "[Raze] (Demon Slayer) Kimetsu no Yaiba - Infinity Castle (2025) (Web) x265 10bit 1080p 144fps.mkv"},
		// No "movie" in the name and a year inside the film window: only
		// the second title identifies this one as a different work.
		{Title: "[Group] Kimetsu no Yaiba - Mugen Train (2020) [1080p]"},
		{Title: "Kimetsu.no.Yaiba.Movie.Mugen.Train.2020.1080p.BluRay"},
		// What S01E01 is actually allowed to be.
		{Title: "Demon.Slayer.Kimetsu.no.Yaiba.S01E01.Cruelty.1080p.WEB-DL"},
		{Title: "[SubsPlease] Kimetsu no Yaiba - 01 (1080p) [ABCD1234].mkv"},
		{Title: "Kimetsu.no.Yaiba.S01.1080p.BluRay.x264"},
		{Title: "[Judas] Kimetsu no Yaiba (2019) [1080p][HEVC x265]"},
	}
	alias := []string{"Kimetsu no Yaiba"}
	got, dropped := filterRelevant(in, "series", "Demon Slayer: Kimetsu no Yaiba", alias, 2019, 1)
	if dropped != 5 || len(got) != 4 {
		t.Fatalf("dropped = %d, kept = %d, want 5 and 4: %+v", dropped, len(got), got)
	}
	for _, r := range got {
		if strings.Contains(strings.ToLower(r.Title), "infinity") || strings.Contains(strings.ToLower(r.Title), "mugen") {
			t.Fatalf("kept a film: %q", r.Title)
		}
	}
	// A markerless batch carrying a later season's own year is not the
	// film: season three aired in 2023, and the widening window must let
	// its year through (the film above still falls to its second title).
	got3, _ := filterRelevant([]Result{
		{Title: "[Judas] Kimetsu no Yaiba (2023) [1080p][HEVC x265]"},
		{Title: "[Raze] (Demon Slayer) Kimetsu no Yaiba - Infinity Castle (2025) (Web) x265 10bit 1080p.mkv"},
	}, "series", "Demon Slayer: Kimetsu no Yaiba", alias, 2019, 3)
	if len(got3) != 1 || !strings.Contains(got3[0].Title, "2023") {
		t.Fatalf("season 3 kept %+v, want only its own batch", got3)
	}
}

// The sequel bug, 2026-10-22 after the film gate above shipped: Demon
// Slayer S01E01's top row was not a film but the Yuukaku-hen (Entertainment
// District) arc pack, and the click played that arc's episode 1. Anime arcs
// are released as their own series with their own numbering, so the arc's
// "S01" / "S01E01" markers state exactly what was asked for and pass every
// number check — the film gate's "identified for this season" exemption
// then skipped the second-title check that would have caught them. What
// identifies them is the arc's name ahead of the number. The rows below are
// the exact kept titles from the production replay of tt9335498:1:1.
func TestFilterRelevantDropsArcPacksForSeries(t *testing.T) {
	in := []Result{
		// The row that was clicked and played: file 0 is
		// "[EMBER] Kimetsu no Yaiba - Yuukaku-hen - 01.mkv".
		{Title: "Kimetsu no Yaiba Yuukaku-hen S01 1080p WEBRip AAC x265-EMBER"},
		{Title: "Kimetsu no Yaiba Yuukaku-hen S01 1080p Dual Audio WEBRip AAC x265-EMBER"},
		{Title: "Demon.Slayer.Kimetsu.No.Yaiba.Entertainment.District.Arc.S01E01-05.1080p.HULU.WEB-DL.AAC2.0.HEVC-WKS"},
		{Title: "[ToonsHub] Demon Slayer- Kimetsu no Yaiba Hashira Training Arc S01E01 1080p B-Global WEB-DL AAC2.0 x264 (Multi-Subs)"},
		// The Mugen Train arc files itself the same way when it appears.
		{Title: "[Group] Demon Slayer Kimetsu no Yaiba Mugen Train Arc S01E01 1080p WEB-DL"},
		// What S01E01 is actually allowed to be, including the rows whose
		// extra words are the episode's own title, after the marker.
		{Title: "Demon Slayer Kimetsu no Yaiba S01E01 VOSTFR 1080p WEB H.264 AAC -Tsundere-Raws"},
		{Title: "[LostYears] Demon Slayer: Kimetsu no Yaiba - S01E01 (BD 1080p Hi10P AAC) [Dual Audio]"},
		{Title: "Kimetsu.no.Yaiba.S01.1080p.NF.WEB-DL.AAC2.0.x264.MULTI-Emmid"},
		{Title: "[Trix] Kimetsu no Yaiba S01-05 [Dual Audio] [Multi Subs] (BD 1080p AV1)"},
	}
	alias := []string{"Kimetsu no Yaiba"}
	got, dropped := filterRelevant(in, "series", "Demon Slayer: Kimetsu no Yaiba", alias, 2019, 1)
	if dropped != 5 || len(got) != 4 {
		t.Fatalf("dropped = %d, kept = %d, want 5 and 4: %+v", dropped, len(got), got)
	}
	for _, r := range got {
		lower := strings.ToLower(r.Title)
		for _, arc := range []string{"yuukaku", "entertainment", "hashira", "mugen"} {
			if strings.Contains(lower, arc) {
				t.Fatalf("kept another arc: %q", r.Title)
			}
		}
	}
	// An episode title after the marker is not a second work: MobLand
	// S02E05 is called "Birdland", and the row must survive for season 2.
	got2, _ := filterRelevant([]Result{
		{Title: "MobLand S02E05 Birdland 1080p AMZN WEB-DL DDP5.1 H.264-RAWR"},
	}, "series", "MobLand", nil, 2025, 2)
	if len(got2) != 1 {
		t.Fatalf("season 2 kept %+v, want its own episode", got2)
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
