package main

import (
	"fmt"
	"testing"
)

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

// File names inside packs do not follow the release-title conventions: some
// groups write a bare number, three-digit seasons, or run the number into the
// next word. None of those should fall through to "largest file".
func TestFileSignalReadsPackFileNames(t *testing.T) {
	cases := []struct {
		path   string
		season int
		ep     int
		want   int
	}{
		{"Show/04.mkv", 1, 4, 1},
		{"Show/04.mkv", 1, 5, -1},
		{"Show/01 - Title.mkv", 1, 1, 1},
		{"Show/[Group] 04v2 [1080p].mkv", 1, 4, 1},
		{"Show/1080p.mkv", 1, 1, 0}, // a resolution, not an episode
		{"Show/2001.mkv", 1, 1, 0},  // four digits are a year, never an episode
		{"Show.S01E04.mkv", 1, 4, 1},
		{"Zoey 101 S01E11Disc Golf.avi", 1, 11, 1},    // no word boundary after 11
		{"Zoey 101 S003E26 Clip Show.avi", 3, 26, 1},  // three-digit season
		{"Zoey 101 S003E26 Clip Show.avi", 1, 26, -1}, // and it is not season one
		{"Show Ep. 04 [720p].mkv", 1, 4, 1},
		{"Show Episode 12.mkv", 1, 12, 1},
		{"NC/ED 01.mkv", 1, 1, 0}, // an extra that must not win
	}
	for _, c := range cases {
		if got := fileSignal(c.path, c.season, c.ep); got != c.want {
			t.Errorf("fileSignal(%q, %d, %d) = %d, want %d", c.path, c.season, c.ep, got, c.want)
		}
	}
}

// A season pack whose files are named "01.mkv" … "12.mkv" must serve the
// numbered episode, not the largest file.
func TestChooseFileForEpisodeReadsBareNumbers(t *testing.T) {
	var paths []string
	var sizes []int64
	for i := 1; i <= 12; i++ {
		paths = append(paths, fmt.Sprintf("Show/%02d.mkv", i))
		sizes = append(sizes, int64(1<<30)+int64(i)) // later files are bigger: least wins if it guesses
	}
	idx, err := chooseFileForEpisode(paths, sizes, -1, 1, 4)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if idx != 3 {
		t.Fatalf("expected 04.mkv (3), got %d", idx)
	}
}

// A batch with many opaque file names gives no way to identify the episode;
// the largest file is a coin flip between the show's episodes, so refuse.
func TestChooseFileForEpisodeRefusesAnOpaqueBatch(t *testing.T) {
	paths := []string{"The Pilot.mkv", "The Second One.mkv", "The Third.mkv", "The Fourth.mkv"}
	sizes := []int64{1 << 30, 2 << 30, 3 << 30, 4 << 30}
	if idx, err := chooseFileForEpisode(paths, sizes, -1, 1, 2); err != errNoEpisode {
		t.Fatalf("expected errNoEpisode for an unnumbered batch, got %d, %v", idx, err)
	}
}

// A release that holds several films must serve the one that was asked for, not
// the largest file: Memory Snow and The Frozen Bond ship in one OVA pack.
func TestChooseFileForTitlePicksTheFilm(t *testing.T) {
	paths := []string{
		"ReZero - Starting Life In Another World - The Frozen Bond - OVA BD.mkv",
		"ReZero - Starting Life In Another World - Memory Snow - OVA BD.mkv",
	}
	sizes := []int64{3 << 30, 2 << 30} // the wrong film is the bigger one

	idx, err := chooseFileForTitle(paths, sizes, -1, []string{"Re:ZERO -Starting Life in Another World- Memory Snow", "Memory Snow"})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if idx != 1 {
		t.Fatalf("expected Memory Snow (1), got %d", idx)
	}

	idx, err = chooseFileForTitle(paths, sizes, -1, []string{"The Frozen Bond"})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if idx != 0 {
		t.Fatalf("expected The Frozen Bond (0), got %d", idx)
	}
}

// Roman numerals are the only thing separating the parts of a trilogy.
func TestChooseFileForTitleReadsPartNumbers(t *testing.T) {
	paths := []string{
		"Back To The Future 1985 Remastered 1080p BluRay HEVC x265 5.1 BONE.mkv",
		"Back To The Future Part II 1989 Remastered 1080p BluRay HEVC x265 5.1 BONE.mkv",
		"Back To The Future Part III 1990 Remastered 1080p BluRay HEVC x265 5.1 BONE.mkv",
	}
	sizes := []int64{1469 << 20, 1448 << 20, 1446 << 20}
	idx, err := chooseFileForTitle(paths, sizes, -1, []string{"Back to the Future Part II"})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if idx != 1 {
		t.Fatalf("expected Part II (1), got %d", idx)
	}
}

// A single film plus featurettes still plays the film.
func TestChooseFileForTitleFallsBackToLargest(t *testing.T) {
	paths := []string{"Featurettes/Behind the Scenes.mkv", "The Invite (2026) (1080p BluRay x265 Ghost).mkv"}
	sizes := []int64{300 << 20, 6 << 30}
	idx, err := chooseFileForTitle(paths, sizes, -1, []string{"The Invite"})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if idx != 1 {
		t.Fatalf("expected the film (1), got %d", idx)
	}
}
