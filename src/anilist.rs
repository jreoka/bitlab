use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AnilistResponse {
    #[serde(default)]
    data: AnilistData,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistData {
    #[serde(rename = "Page", default)]
    page: AnilistPage,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistPage {
    #[serde(default)]
    media: Vec<AnilistMedia>,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistMedia {
    #[serde(default)]
    title: AnilistTitle,
    #[serde(default)]
    synonyms: Vec<String>,
}
#[derive(Debug, Deserialize, Default)]
struct AnilistTitle {
    #[serde(default)]
    romaji: String,
    #[serde(default)]
    english: String,
    #[serde(default)]
    native: String,
}

#[derive(Debug, Deserialize)]
struct KitsuResponse {
    #[serde(default)]
    data: Vec<KitsuItem>,
}
#[derive(Debug, Deserialize, Default)]
struct KitsuItem {
    #[serde(default)]
    attributes: KitsuAttrs,
}
#[derive(Debug, Deserialize, Default)]
struct KitsuAttrs {
    #[serde(default, rename = "canonicalTitle")]
    canonical: String,
    #[serde(default)]
    titles: KitsuTitles,
    #[serde(default, rename = "abbreviatedTitles")]
    abbrev: Vec<String>,
}
#[derive(Debug, Deserialize, Default)]
struct KitsuTitles {
    #[serde(default)]
    en: String,
    #[serde(default)]
    en_jp: String,
}

const ANILIST_QUERY: &str = r#"query ($s: String) { Page(perPage: 1) { media(search: $s, type: ANIME) { title { romaji english native } synonyms } } }"#;

pub async fn anime_aliases(app: &crate::server::App, title: &str) -> Vec<String> {
    let title = title.trim();
    if title.len() < 2 {
        return vec![];
    }
    let ck = format!("anilist:{}", title.to_lowercase());
    if let Some(v) = app.store.get_cached_meta(&ck) {
        if let Ok(out) = serde_json::from_str::<Vec<String>>(&v) {
            if !out.is_empty() {
                return out;
            }
        }
    }
    let ctx = std::time::Duration::from_secs(8);
    let candidates = fetch_anilist_candidates(&app.http, title, ctx).await;
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

async fn fetch_anilist_candidates(http: &reqwest::Client, title: &str, _timeout: std::time::Duration) -> Vec<String> {
    for q in romanization_variants(title) {
        if q.trim().is_empty() {
            continue;
        }
        if let Some(c) = query_anilist(http, &q).await {
            if !c.is_empty() {
                return c;
            }
        }
    }
    // also try original
    query_anilist(http, title).await.unwrap_or_default()
}

async fn query_anilist(http: &reqwest::Client, query: &str) -> Option<Vec<String>> {
    let payload = serde_json::json!({"query": ANILIST_QUERY, "variables": {"s": query}});
    let resp = http.post("https://graphql.anilist.co").json(&payload).header("Accept", "application/json").send().await.ok()?;
    if resp.status() != 200 {
        return None;
    }
    let ar: AnilistResponse = resp.json().await.ok()?;
    let m = ar.data.page.media.into_iter().next()?;
    let mut out = vec![m.title.romaji, m.title.english, m.title.native];
    out.extend(m.synonyms);
    Some(out.into_iter().filter(|s| !s.trim().is_empty()).collect())
}

async fn fetch_kitsu_candidates(http: &reqwest::Client, title: &str) -> Vec<String> {
    let u = format!("https://kitsu.io/api/edge/anime?filter[text]={}", url::form_urlencoded::byte_serialize(title.as_bytes()).collect::<String>());
    let resp = match http.get(&u).header("Accept", "application/vnd.api+json").header("User-Agent", "bitlab/1.0").send().await { Ok(r) => r, Err(_) => return vec![] };
    if resp.status() != 200 {
        return vec![];
    }
    let kr: KitsuResponse = resp.json().await.unwrap_or(KitsuResponse { data: vec![] });
    for item in kr.data {
        let a = item.attributes;
        let mut c = vec![a.canonical, a.titles.en, a.titles.en_jp];
        c.extend(a.abbrev);
        let c: Vec<String> = c.into_iter().filter(|s| !s.trim().is_empty()).collect();
        if !c.is_empty() {
            return c;
        }
    }
    vec![]
}

fn romanization_variants(title: &str) -> Vec<String> {
    let mut out = vec![title.to_string()];
    let mut v = title.to_string();
    let reps = [
        ("jyou", "jou"), ("jyo", "jo"), ("jya", "ja"), ("jyu", "ju"),
        ("zyou", "jou"), ("zyo", "jo"), ("zya", "ja"), ("zyu", "ju"), ("zi", "ji"),
        ("shyou", "shou"), ("syou", "shou"), ("syo", "sho"), ("sya", "sha"), ("syu", "shu"), ("si", "shi"),
        ("chyou", "chou"), ("tyou", "chou"), ("tyo", "cho"), ("tya", "cha"), ("tyu", "chu"),
        ("cyou", "chou"), ("cyo", "cho"), ("cya", "cha"), ("cyu", "chu"),
        ("ti", "chi"), ("tu", "tsu"), ("hu", "fu"),
    ];
    for (f, t) in reps {
        // case-insensitive replace preserving case of first letter
        let re = regex::Regex::new(&format!("(?i){}", regex::escape(f))).unwrap();
        let nv = re.replace_all(&v, |caps: &regex::Captures| {
            let m = &caps[0];
            if m.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                let mut tc = t.to_string();
                if let Some(first) = tc.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                tc
            } else {
                t.to_string()
            }
        }).to_string();
        v = nv;
    }
    if !v.eq_ignore_ascii_case(title) {
        out.push(v.clone());
    }
    let lower = title.to_lowercase();
    if lower.contains("oo") {
        let re = regex::Regex::new(r"(?i)oo").unwrap();
        out.push(replace_preserving(title, &re, "ou"));
        if !v.eq_ignore_ascii_case(title) {
            out.push(replace_preserving(&v, &re, "ou"));
        }
    } else if lower.contains("ou") {
        let re = regex::Regex::new(r"(?i)ou").unwrap();
        out.push(replace_preserving(title, &re, "oo"));
        if !v.eq_ignore_ascii_case(title) {
            out.push(replace_preserving(&v, &re, "oo"));
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
            t.get_mut(..1).map(|f| f.make_ascii_uppercase());
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

fn pick_aliases(original: &str, candidates: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    seen.insert(original.trim().to_lowercase());
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
        seen.insert(k);
        out.push(c);
        if out.len() == 3 {
            break;
        }
    }
    out
}

fn is_latin(s: &str) -> bool {
    let mut letters = 0;
    for r in s.chars() {
        if r.is_ascii_alphabetic() || (r as u32 >= 0xC0 && r.is_alphabetic()) {
            letters += 1;
        } else if r.is_ascii_alphanumeric() || r.is_whitespace() || r.is_ascii_punctuation() || "‐‑‒–—―".contains(r) || (r as u32 >= 0x2000 && r as u32 <= 0x206F) {
            continue;
        } else if r.is_alphabetic() {
            // non-latin alphabet
            return false;
        } else {
            // symbols ok
        }
    }
    letters >= 2
}
