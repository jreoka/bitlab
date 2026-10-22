package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"time"
)

// AniList gives us the other names an anime is released under, which is the
// whole reason anime searches felt empty: "Re:ZERO -Starting Life in Another
// World-" is indexed as "Re ZERO Starting Life in Another World" or
// "Re:Zero kara Hajimeru Isekai Seikatsu" depending on the group.

type anilistResponse struct {
	Data struct {
		Page struct {
			Media []struct {
				Title struct {
					Romaji  string `json:"romaji"`
					English string `json:"english"`
					Native  string `json:"native"`
				} `json:"title"`
				Synonyms []string `json:"synonyms"`
			} `json:"media"`
		} `json:"Page"`
	} `json:"data"`
}

const anilistQuery = `query ($s: String) {
  Page(perPage: 1) {
    media(search: $s, type: ANIME) {
      title { romaji english native }
      synonyms
    }
  }
}`

// animeAliases returns alternative titles for a show, or nil if AniList does
// not know it. Results are cached; failures are silent because this is a
// best-effort enrichment, not a dependency.
func (s *Server) animeAliases(ctx context.Context, title string) []string {
	title = strings.TrimSpace(title)
	if len(title) < 2 {
		return nil
	}
	ck := "anilist:" + strings.ToLower(title)
	if v, ok := s.store.GetCachedMeta(ck); ok {
		var out []string
		if json.Unmarshal([]byte(v), &out) == nil {
			return out
		}
	}

	ctx, cancel := context.WithTimeout(ctx, 8*time.Second)
	defer cancel()

	payload, _ := json.Marshal(map[string]any{"query": anilistQuery, "variables": map[string]string{"s": title}})
	req, err := http.NewRequestWithContext(ctx, "POST", "https://graphql.anilist.co", bytes.NewReader(payload))
	if err != nil {
		return nil
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	resp, err := s.http.Do(req)
	if err != nil {
		return nil
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil
	}
	var ar anilistResponse
	if err := json.NewDecoder(io.LimitReader(resp.Body, 8<<20)).Decode(&ar); err != nil {
		return nil
	}
	if len(ar.Data.Page.Media) == 0 {
		s.store.CacheMeta(ck, "[]", 14*24*time.Hour)
		return nil
	}
	m := ar.Data.Page.Media[0]
	candidates := append([]string{m.Title.Romaji, m.Title.English}, m.Synonyms...)
	aliases := pickAliases(title, candidates)
	b, _ := json.Marshal(aliases)
	s.store.CacheMeta(ck, string(b), 14*24*time.Hour)
	return aliases
}

// pickAliases keeps names a release title could plausibly use: latin script,
// sane length, not the original, no duplicates.
func pickAliases(original string, candidates []string) []string {
	seen := map[string]bool{strings.ToLower(strings.TrimSpace(original)): true}
	var out []string
	for _, c := range candidates {
		c = strings.Join(strings.Fields(c), " ")
		if len(c) < 3 || len(c) > 70 {
			continue
		}
		if !isLatin(c) {
			continue
		}
		key := strings.ToLower(c)
		if seen[key] {
			continue
		}
		seen[key] = true
		out = append(out, c)
		if len(out) == 3 {
			break
		}
	}
	return out
}

func isLatin(s string) bool {
	letters := 0
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z':
			letters++
		case r >= '0' && r <= '9', r == ' ', r == ':', r == '-', r == '!', r == '?', r == '.', r == '\'', r == ',', r == '&', r == '(', r == ')', r == '+', r == '/':
		default:
			return false
		}
	}
	return letters >= 2
}
