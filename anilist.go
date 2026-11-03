package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/url"
	"regexp"
	"strings"
	"time"
	"unicode"
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

type kitsuResponse struct {
	Data []struct {
		Attributes struct {
			CanonicalTitle string `json:"canonicalTitle"`
			Titles         struct {
				En   string `json:"en"`
				EnJp string `json:"en_jp"`
			} `json:"titles"`
			AbbreviatedTitles []string `json:"abbreviatedTitles"`
		} `json:"attributes"`
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

// animeAliases returns alternative titles for a show, or nil if AniList/Kitsu
// does not know it. Results are cached; failures are silent because this is a
// best-effort enrichment, not a dependency.
func (s *Server) animeAliases(ctx context.Context, title string) []string {
	title = strings.TrimSpace(title)
	if len(title) < 2 {
		return nil
	}
	ck := "anilist:" + strings.ToLower(title)
	if v, ok := s.store.GetCachedMeta(ck); ok {
		var out []string
		if json.Unmarshal([]byte(v), &out) == nil && len(out) > 0 {
			return out
		}
	}

	ctx, cancel := context.WithTimeout(ctx, 8*time.Second)
	defer cancel()

	candidates := s.fetchAniListCandidates(ctx, title)
	if len(candidates) == 0 {
		candidates = s.fetchKitsuCandidates(ctx, title)
	}

	aliases := pickAliases(title, expandMacronCandidates(candidates))
	if len(aliases) > 0 {
		b, _ := json.Marshal(aliases)
		s.store.CacheMeta(ck, string(b), 14*24*time.Hour)
	} else {
		// Cache negative lookup briefly (10 mins) instead of 14 days
		s.store.CacheMeta(ck, "[]", 10*time.Minute)
	}
	return aliases
}

func (s *Server) fetchAniListCandidates(ctx context.Context, title string) []string {
	queries := append([]string{title}, romanizationVariants(title)...)
	queries = dedupeStrings(queries)
	for _, q := range queries {
		if c := s.queryAniList(ctx, q); len(c) > 0 {
			return c
		}
	}
	return nil
}

func (s *Server) queryAniList(ctx context.Context, query string) []string {
	payload, _ := json.Marshal(map[string]any{"query": anilistQuery, "variables": map[string]string{"s": query}})
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
		return nil
	}
	m := ar.Data.Page.Media[0]
	return append([]string{m.Title.Romaji, m.Title.English}, m.Synonyms...)
}

func (s *Server) fetchKitsuCandidates(ctx context.Context, title string) []string {
	u := "https://kitsu.io/api/edge/anime?filter[text]=" + url.QueryEscape(title)
	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return nil
	}
	req.Header.Set("Accept", "application/vnd.api+json")
	req.Header.Set("User-Agent", "bitlab/1.0")
	resp, err := s.http.Do(req)
	if err != nil {
		return nil
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil
	}
	var kr kitsuResponse
	if err := json.NewDecoder(io.LimitReader(resp.Body, 4<<20)).Decode(&kr); err != nil {
		return nil
	}
	var candidates []string
	for _, item := range kr.Data {
		attrs := item.Attributes
		candidates = append(candidates, attrs.CanonicalTitle, attrs.Titles.En, attrs.Titles.EnJp)
		candidates = append(candidates, attrs.AbbreviatedTitles...)
		if len(candidates) > 0 {
			break
		}
	}
	return candidates
}

func romanizationVariants(title string) []string {
	var out []string
	v := title
	replacements := []struct{ from, to string }{
		{"jyou", "jou"}, {"jyo", "jo"}, {"jya", "ja"}, {"jyu", "ju"},
		{"zyou", "jou"}, {"zyo", "jo"}, {"zya", "ja"}, {"zyu", "ju"}, {"zi", "ji"},
		{"shyou", "shou"}, {"syou", "shou"}, {"syo", "sho"}, {"sya", "sha"}, {"syu", "shu"}, {"si", "shi"},
		{"chyou", "chou"}, {"tyou", "chou"}, {"tyo", "cho"}, {"tya", "cha"}, {"tyu", "chu"},
		{"cyou", "chou"}, {"cyo", "cho"}, {"cya", "cha"}, {"cyu", "chu"},
		{"ti", "chi"}, {"tu", "tsu"}, {"hu", "fu"},
	}
	for _, r := range replacements {
		re := regexp.MustCompile("(?i)" + r.from)
		if re.MatchString(v) {
			v = replacePreservingCase(v, re, r.to)
		}
	}
	if !strings.EqualFold(v, title) {
		out = append(out, v)
	}

	if strings.Contains(strings.ToLower(title), "oo") {
		re := regexp.MustCompile(`(?i)oo`)
		out = append(out, replacePreservingCase(title, re, "ou"))
		if !strings.EqualFold(v, title) {
			out = append(out, replacePreservingCase(v, re, "ou"))
		}
	} else if strings.Contains(strings.ToLower(title), "ou") {
		re := regexp.MustCompile(`(?i)ou`)
		out = append(out, replacePreservingCase(title, re, "oo"))
		if !strings.EqualFold(v, title) {
			out = append(out, replacePreservingCase(v, re, "oo"))
		}
	}
	if unfolded := foldMacrons(title); !strings.EqualFold(unfolded, title) {
		out = append(out, unfolded)
	}
	return dedupeStrings(out)
}

func replacePreservingCase(s string, re *regexp.Regexp, to string) string {
	return re.ReplaceAllStringFunc(s, func(match string) string {
		if len(match) == 0 || len(to) == 0 {
			return to
		}
		if match[0] >= 'A' && match[0] <= 'Z' {
			return strings.ToUpper(to[:1]) + to[1:]
		}
		return to
	})
}

func foldMacrons(s string) string {
	repl := strings.NewReplacer(
		"ō", "ou", "Ō", "Ou",
		"ū", "uu", "Ū", "Uu",
		"ā", "aa", "Ā", "Aa",
		"ī", "ii", "Ī", "Ii",
		"ē", "ee", "Ē", "Ee",
	)
	return repl.Replace(s)
}

func foldMacronsSingle(s string) string {
	repl := strings.NewReplacer(
		"ō", "o", "Ō", "O",
		"ū", "u", "Ū", "U",
		"ā", "a", "Ā", "A",
		"ī", "i", "Ī", "I",
		"ē", "e", "Ē", "E",
	)
	return repl.Replace(s)
}

func expandMacronCandidates(candidates []string) []string {
	var out []string
	for _, c := range candidates {
		if c == "" {
			continue
		}
		out = append(out, c)
		if f1 := foldMacrons(c); !strings.EqualFold(f1, c) {
			out = append(out, f1)
		}
		if f2 := foldMacronsSingle(c); !strings.EqualFold(f2, c) && !strings.EqualFold(f2, foldMacrons(c)) {
			out = append(out, f2)
		}
	}
	return out
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
		case (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || unicode.Is(unicode.Latin, r):
			letters++
		case (r >= '0' && r <= '9') || unicode.IsPunct(r) || unicode.IsSpace(r) || unicode.IsSymbol(r):
		default:
			return false
		}
	}
	return letters >= 2
}
