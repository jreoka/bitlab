package main

import "testing"

// A pack whose files are numbered, none of which is the episode asked for, is
// a release that does not contain that episode. Falling through to "largest
// file" is how a season-1 pack once played S01E17 for a request for S03E01.
func TestChooseFileForEpisodeRefusesAPackWithoutTheEpisode(t *testing.T) {
	paths := []string{"Show.S01E17.1080p.mkv", "Show.S01E18.1080p.mkv"}
	sizes := []int64{900 << 20, 4 << 30}
	idx, err := chooseFileForEpisode(paths, sizes, -1, 1, 1)
	if err == nil {
		t.Fatalf("expected a refusal, got file %d", idx)
	}
	if err != errNoEpisode {
		t.Fatalf("expected errNoEpisode, got %v", err)
	}
}

// A release with no numbering at all (a single episode plus a sample or an
// extra) has nothing to match, so the largest file is still the best guess.
func TestChooseFileForEpisodeFallsBackWhenNothingIsNumbered(t *testing.T) {
	paths := []string{"movie.mkv", "sample.mkv"}
	sizes := []int64{4 << 30, 40 << 20}
	idx, err := chooseFileForEpisode(paths, sizes, -1, 1, 1)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if idx != 0 {
		t.Fatalf("expected the largest file (0), got %d", idx)
	}
}

// The wanted episode is still found inside a pack.
func TestChooseFileForEpisodeFindsTheEpisode(t *testing.T) {
	paths := []string{"Show.S03E01.mkv", "Show.S03E02.mkv", "Show.S03E03.mkv"}
	sizes := []int64{1 << 30, 1 << 30, 1 << 30}
	idx, err := chooseFileForEpisode(paths, sizes, -1, 3, 2)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if idx != 1 {
		t.Fatalf("expected S03E02 (1), got %d", idx)
	}
}
