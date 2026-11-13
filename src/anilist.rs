use crate::server::{de_null_default, read_body_limited, MAX_HTTP_BODY};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct AnilistResponse {
    #[serde(default, deserialize_with = "de_null_default")]
    data: AnilistData,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistData {
    #[serde(rename = "Page", default, deserialize_with = "de_null_default")]
    page: AnilistPage,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistPage {
    #[serde(default, deserialize_with = "de_null_default")]
    media: Vec<AnilistMedia>,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistMedia {
    #[serde(default, deserialize_with = "de_null_default")]
    title: AnilistTitle,
    #[serde(default, deserialize_with = "de_null_default")]
    synonyms: Vec<String>,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistTitle {
    #[serde(default, deserialize_with = "de_null_default")]
    romaji: String,
    #[serde(default, deserialize_with = "de_null_default")]
    english: String,
    #[serde(default, deserialize_with = "de_null_default")]
    native: String,
}

#[derive(Debug, Deserialize)]
struct KitsuResponse {
    #[serde(default, deserialize_with = "de_null_default")]
    data: Vec<KitsuItem>,
}
#[derive(Debug, Deserialize, Default)]
struct KitsuItem {
    #[serde(default, deserialize_with = "de_null_default")]
    attributes: KitsuAttrs,
}
#[derive(Debug, Deserialize, Default)]
struct KitsuAttrs {
    #[serde(default, rename = "canonicalTitle", deserialize_with = "de_null_default")]
    canonical: String,
    #[serde(default, deserialize_with = "de_null_default")]
    titles: KitsuTitles,
    #[serde(default, rename = "abbreviatedTitles", deserialize_with = "de_null_default")]
    abbrev: Vec<String>,
}
#[derive(Debug, Deserialize, Default)]
struct KitsuTitles {
    #[serde(default, deserialize_with = "de_null_default")]
    en: String,
    #[serde(default, deserialize_with = "de_null_default")]
    en_jp: String,
}

static ROMAN_REPS: Lazy<Vec<(regex::Regex, &'static str)>> = Lazy::new(|| {
    [
        ("jyou", "jou"), ("jyo", "jo"), ("jya", "ja"), ("jyu", "ju"),
        ("zyou", "jou"), ("zyo", "jo"), ("zya", "ja"), ("zyu", "ju"), ("zi", "ji"),
        ("shyou", "shou"), ("syou", "shou"), ("syo", "sho"), ("sya", "sha"), ("syu", "shu"), ("si", "shi"),
        ("chyou", "chou"), ("tyou", "chou"), ("tyo", "cho"), ("tya", "cha"), ("tyu", "chu"),
        ("cyou", "chou"), ("cyo", "cho"), ("cya", "cha"), ("cyu", "chu"),
        ("ti", "chi"), ("tu", "tsu"), ("hu", "fu"),
    ]
    .into_iter()
    .map(|(f, t)| (regex::Regex::new(&format!("(?i){}", regex::escape(f))).unwrap(), t))
    .collect()
});
static RE_OO: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(?i)oo").unwrap());
static RE_OU: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(?i)ou").unwrap());

const ANILIST_QUERY: &str = r#"query ($s: String) { Page(perPage: 1) { media(search: $s, type: ANIME) { title { romaji english native } synonyms } } }"#;

pub async fn anime_aliases(app: &crate::server::App, title: &str) -> Vec<String> {
    let title = title.trim();
    if title.len() < 2 {
        return vec![];
    }
    let ck = format!("anilist:v2:{}", title.to_lowercase());
    if let Some(v) = app.store.get_cached_meta(&ck) {
        if let Ok(out) = serde_json::from_str::<Vec<String>>(&v) {
            return out;
        }
    }
    let candidates = fetch_anilist_candidates(&app.http, title).await;
    let candidates = if candidates.is_empty() {
        fetch_kitsu_candidates(&app.http, title).await
    } else {
        candidates
    };
    let aliases = pick_aliases(title, &expand_macron_candidates(&candidates));
    if !aliases.is_empty() {
        let b = serde_json::to_string(&aliases).unwrap_or_else(|_| "[]".to_string());
        let _ = app.store.cache_meta(&ck, &b, 14 * 24 * 3600);
    } else {
        let _ = app.store.cache_meta(&ck, "[]", 600);
    }
    aliases
}

async fn fetch_anilist_candidates(http: &reqwest::Client, title: &str) -> Vec<String> {
    for q in romanization_variants(title) {
        if q.trim().is_empty() {
            continue;
        }
        if let Some(c) = query_anilist(http, &q, title).await {
            if !c.is_empty() {
                return c;
            }
        }
    }
    vec![]
}

async fn query_anilist(http: &reqwest::Client, query: &str, title: &str) -> Option<Vec<String>> {
    let payload = serde_json::json!({"query": ANILIST_QUERY, "variables": {"s": query}});
    let body = tokio::time::timeout(Duration::from_secs(4), async {
        let resp = http
            .post("https://graphql.anilist.co")
            .json(&payload)
            .header("Accept", "application/json")
            .send()
            .await
            .ok()?;
        if resp.status() != 200 {
            return None;
        }
        read_body_limited(resp, MAX_HTTP_BODY).await.ok()
    })
    .await
    .ok()??;
    let ar: AnilistResponse = serde_json::from_slice(&body).ok()?;
    let m = ar.data.page.media.into_iter().next()?;
    let mut out = vec![m.title.romaji, m.title.english, m.title.native];
    out.extend(m.synonyms);
    let out: Vec<String> = out.into_iter().filter(|s| !s.trim().is_empty()).collect();
    if any_variant_matches(&out, title) || any_variant_matches(&out, query) {
        Some(out)
    } else {
        None
    }
}

async fn fetch_kitsu_candidates(http: &reqwest::Client, title: &str) -> Vec<String> {
    let u = format!("https://kitsu.io/api/edge/anime?filter[text]={}", url::form_urlencoded::byte_serialize(title.as_bytes()).collect::<String>());
    let body = match tokio::time::timeout(Duration::from_secs(4), async {
        let resp = http
            .get(&u)
            .header("Accept", "application/vnd.api+json")
            .header("User-Agent", "bitlab/1.0")
            .send()
            .await
            .ok()?;
        if resp.status() != 200 {
            return None;
        }
        read_body_limited(resp, MAX_HTTP_BODY).await.ok()
    })
    .await
    {
        Ok(Some(b)) => b,
        _ => return vec![],
    };
    let kr: KitsuResponse = serde_json::from_slice(&body).unwrap_or(KitsuResponse { data: vec![] });
    for item in kr.data {
        let a = item.attributes;
        let mut c = vec![a.canonical, a.titles.en, a.titles.en_jp];
        c.extend(a.abbrev);
        let c: Vec<String> = c.into_iter().filter(|s| !s.trim().is_empty()).collect();
        if any_variant_matches(&c, title) {
            return c;
        }
    }
    vec![]
}

fn romanization_variants(title: &str) -> Vec<String> {
    let mut out = vec![title.to_string()];
    let mut v = title.to_string();
    for (re, t) in ROMAN_REPS.iter() {
        // case-insensitive replace preserving case of first letter
        v = replace_preserving(&v, re, t);
    }
    if !v.eq_ignore_ascii_case(title) {
        out.push(v.clone());
    }
    let lower = title.to_lowercase();
    if lower.contains("oo") {
        out.push(replace_preserving(title, &RE_OO, "ou"));
        if !v.eq_ignore_ascii_case(title) {
            out.push(replace_preserving(&v, &RE_OO, "ou"));
        }
    } else if lower.contains("ou") {
        out.push(replace_preserving(title, &RE_OU, "oo"));
        if !v.eq_ignore_ascii_case(title) {
            out.push(replace_preserving(&v, &RE_OU, "oo"));
        }
    }
    let unfolded = fold_macrons(title);
    if !unfolded.eq_ignore_ascii_case(title) {
        out.push(unfolded);
    }
    crate::relevance::dedupe_strings(out)
}

fn replace_preserving(s: &str, re: &regex::Regex, to: &str) -> String {
    re.replace_all(s, |caps: &regex::Captures| {
        let m = &caps[0];
        if m.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            let mut t = to.to_string();
            if let Some(f) = t.get_mut(..1) {
                f.make_ascii_uppercase();
            }
            t
        } else {
            to.to_string()
        }
    }).to_string()
}

fn fold_macrons(s: &str) -> String {
    s.replace('ō', "ou").replace('Ō', "Ou").replace('ū', "uu").replace('Ū', "Uu").replace('ā', "aa").replace('Ā', "Aa").replace('ī', "ii").replace('Ī', "Ii").replace('ē', "ee").replace('Ē', "Ee")
}

fn fold_macrons_single(s: &str) -> String {
    s.replace('ō', "o").replace('Ō', "O").replace('ū', "u").replace('Ū', "U").replace('ā', "a").replace('Ā', "A").replace('ī', "i").replace('Ī', "I").replace('ē', "e").replace('Ē', "E")
}

fn expand_macron_candidates(cands: &[String]) -> Vec<String> {
    let mut out = vec![];
    for c in cands {
        if c.is_empty() {
            continue;
        }
        out.push(c.clone());
        let f1 = fold_macrons(c);
        if !f1.eq_ignore_ascii_case(c) {
            out.push(f1);
        }
        let f2 = fold_macrons_single(c);
        if !f2.eq_ignore_ascii_case(c) && !f2.eq_ignore_ascii_case(&fold_macrons(c)) {
            out.push(f2);
        }
    }
    out
}

fn clean_comparable(s: &str) -> String {
    let s = s.replace('×', "x");
    crate::relevance::norm_name(&crate::relevance::norm_romaji(&s))
}

fn titles_match(cand: &str, query: &str) -> bool {
    let c = clean_comparable(cand);
    let q = clean_comparable(query);
    if c.is_empty() || q.is_empty() {
        return false;
    }
    if c == q {
        return true;
    }
    for (with_sub, other) in [(cand, query), (query, cand)] {
        for sep in [":", " - "] {
            if let Some((main_part, _)) = with_sub.split_once(sep) {
                let main_part = main_part.trim();
                if !main_part.is_empty() && clean_comparable(main_part) == clean_comparable(other) {
                    return true;
                }
            }
        }
    }
    false
}

fn media_matches_title(candidates: &[String], query: &str) -> bool {
    for c in candidates {
        if c.trim().is_empty() {
            continue;
        }
        if titles_match(c, query) {
            return true;
        }
    }
    false
}

fn any_variant_matches(candidates: &[String], title: &str) -> bool {
    if media_matches_title(candidates, title) {
        return true;
    }
    for v in romanization_variants(title) {
        if media_matches_title(candidates, &v) {
            return true;
        }
    }
    false
}

fn pick_aliases(original: &str, candidates: &[String]) -> Vec<String> {
    if !any_variant_matches(candidates, original) {
        return vec![];
    }
    let mut seen = std::collections::HashSet::new();
    seen.insert(original.trim().to_lowercase());
    let orig_clean = clean_comparable(original);
    let mut out = vec![];
    for c in candidates {
        let c = c.split_whitespace().collect::<Vec<_>>().join(" ");
        if c.len() < 3 || c.len() > 70 {
            continue;
        }
        if !is_latin(&c) {
            continue;
        }
        let k = c.to_lowercase();
        if seen.contains(&k) {
            continue;
        }
        if !orig_clean.is_empty() && clean_comparable(&c) == orig_clean {
            continue;
        }
        seen.insert(k);
        out.push(c);
        if out.len() == 3 {
            break;
        }
    }
    out
}

fn is_latin_char(r: char) -> bool {
    matches!(r as u32, 0x41..=0x5A | 0x61..=0x7A | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x24F | 0x1E00..=0x1EFF)
}

fn is_latin(s: &str) -> bool {
    let mut letters = 0;
    for r in s.chars() {
        if is_latin_char(r) {
            letters += 1;
        } else if r.is_ascii_alphanumeric() || r.is_whitespace() || r.is_ascii_punctuation() || "‐‑‒–—―".contains(r) || (0x2000..=0x206F).contains(&(r as u32)) {
            continue;
        } else if r.is_alphabetic() {
            // non-latin alphabet (Kanji, Kana, Hangul, Cyrillic, etc.)
            return false;
        }
    }
    letters >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_titles_match_real_anime() {
        assert!(titles_match("Attack on Titan", "Attack on Titan"));
        assert!(titles_match("Shingeki no Kyojin", "Shingeki no Kyojin"));
        assert!(titles_match("Demon Slayer: Kimetsu no Yaiba", "Demon Slayer"));
        assert!(titles_match("Demon Slayer", "Demon Slayer: Kimetsu no Yaiba"));
        assert!(titles_match("SPY×FAMILY", "Spy x Family"));
        assert!(titles_match("Sousou no Frieren", "Sōsō no Frieren"));
        assert!(titles_match("BOCCHI THE ROCK!", "Bocchi the Rock!"));
    }

    #[test]
    fn test_titles_match_rejects_unrelated() {
        assert!(!titles_match("Black Clover", "The Gilded Age"));
        assert!(!titles_match("Appare-Ranman!", "The Sopranos"));
        assert!(!titles_match("Ali Baba to 40-hiki no Touzoku", "Breaking Bad"));
        assert!(!titles_match("B: The Beginning Succession", "Succession"));
        assert!(!titles_match("Yuri Kuma Arashi", "Severance"));
        assert!(!titles_match("Bear Bear Bear Kuma!", "The Bear"));
    }

    #[test]
    fn test_pick_aliases_unrelated_returns_empty() {
        let candidates = vec![
            "Black Clover".to_string(),
            "ブラッククローバー".to_string(),
        ];
        let aliases = pick_aliases("The Gilded Age", &candidates);
        assert!(aliases.is_empty(), "Unrelated candidates must be rejected");
    }

    #[test]
    fn test_is_latin() {
        assert!(is_latin("Shingeki no Kyojin"));
        assert!(is_latin("Sousou no Frieren"));
        assert!(is_latin("Sōsō no Frieren"));
        assert!(is_latin("Amélie"));
        assert!(!is_latin("進撃の巨人"));
        assert!(!is_latin("ブラッククローバー"));
        assert!(!is_latin("Наруто"));
        assert!(!is_latin("बौद्ध"));
    }

    #[test]
    fn test_pick_aliases_real_anime() {
        let candidates = vec![
            "Shingeki no Kyojin".to_string(),
            "Attack on Titan".to_string(),
            "進撃の巨人".to_string(),
        ];
        let aliases = pick_aliases("Attack on Titan", &candidates);
        assert_eq!(aliases, vec!["Shingeki no Kyojin"]);
    }

    #[test]
    fn test_null_tolerance_in_payloads() {
        let j = r#"{"data":{"Page":{"media":[{"title":{"romaji":"Frieren","english":null,"native":null},"synonyms":null}]}}}"#;
        let ar: AnilistResponse = serde_json::from_str(j).unwrap();
        assert_eq!(ar.data.page.media[0].title.romaji, "Frieren");
        assert!(ar.data.page.media[0].synonyms.is_empty());

        let kj = r#"{"data":[{"attributes":{"canonicalTitle":"Frieren","titles":{"en":null,"en_jp":"Sousou no Frieren"},"abbreviatedTitles":null}}]}"#;
        let kr: KitsuResponse = serde_json::from_str(kj).unwrap();
        assert_eq!(kr.data[0].attributes.titles.en_jp, "Sousou no Frieren");
    }
}
