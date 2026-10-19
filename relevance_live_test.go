package main

import (
	"context"
	"net/http"
	"os"
	"testing"
	"time"
)

// Hits the real indexers, so it is opt-in: BITLAB_LIVE=1 go test -run TestLive.
// It exists to prove the relevance gate works against what the indexers actually
// answer today, not against hand-written strings.
func TestLiveMovieSearchHasNoEpisodes(t *testing.T) {
	if os.Getenv("BITLAB_LIVE") != "1" {
		t.Skip("set BITLAB_LIVE=1 to query the indexers")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 40*time.Second)
	defer cancel()

	hc := &http.Client{Timeout: 30 * time.Second}
	res, err := knabenSearch(ctx, hc, Indexer{Kind: "knaben", URL: "https://api.knaben.org"}, "The Uprising")
	if err != nil {
		t.Fatalf("knaben: %v", err)
	}
	t.Logf("knaben returned %d results for the title alone", len(res))

	kept, dropped := filterRelevant(res, "movie", "The Uprising", 2026, 0)
	t.Logf("kept %d, dropped %d", len(kept), dropped)
	for _, r := range kept {
		if looksLikeEpisode(r.Title) {
			t.Errorf("kept an episode: %q", r.Title)
		}
		if !titleMatches(r.Title, "The Uprising") {
			t.Errorf("kept an unrelated title: %q", r.Title)
		}
	}
	if len(kept) == 0 {
		t.Log("nothing survived — correct if the only matches were episodes/other films")
	}
}
