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
	got, dropped := filterRelevant(in, "movie", "The Uprising", 2026, 0)
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
	got, dropped := filterRelevant(in, "movie", "The Uprising", 2026, 0)
	if dropped != 1 || len(got) != 1 || got[0].Title != "The.Uprising.2026.1080p.WEB-DL" {
		t.Fatalf("got %+v (dropped %d)", got, dropped)
	}
}

func TestFilterRelevantYearGate(t *testing.T) {
	in := []Result{
		{Title: "Uprising.2014.1080p.BluRay"},          // wrong film, right words
		{Title: "Uprising.2025.1080p.WEB-DL"},          // a year either side is fine
		{Title: "Uprising.1080p.WEB-DL"},               // no year at all: allowed
	}
	got, dropped := filterRelevant(in, "movie", "Uprising", 2026, 0)
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
		{"Uprising.2026.1080p", "The Uprising", true},          // article dropped by the group
		{"Pacific.Rim.Uprising.2018", "The Uprising", false},   // the trap
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
