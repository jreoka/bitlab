use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

static RE_EP_MARKER_SXXEXX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bS\d{1,2}\s*E\d{1,3}\b").unwrap());
static RE_EP_MARKER_X: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b\d{1,2}x\d{2}\b").unwrap());
static RE_EP_MARKER_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bepisode[.\s_-]*\d{1,4}\b").unwrap());
static RE_EP_MARKER_SEASON: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bseason[.\s_-]*\d{1,2}\b").unwrap());
static RE_EP_MARKER_COMPLETE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bcomplete[.\s_-]+(series|season)\b").unwrap());
static RE_EP_MARKER_DASH: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s-\s\d{2,4}(\s|$)").unwrap());
static RE_YEAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(19|20)\d{2}\b").unwrap());
static RE_NON_ALNUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
static RE_PROMO: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:^|[\s._\-\[\(])(pv|ncop|nced|creditless)(?:[\s._\-\]\)]|$)").unwrap());
static RE_MOVIE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bmovie\b|\bgekijo+ban\b|\bfeature[\s._-]+film\b").unwrap());
static RE_ANNOUNCED_FILM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bthe[\s._-]+movie\b|\bgekijo+ban\b").unwrap());
static RE_NON_VIDEO: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(rar|zip|7z|ass|srt|ssa|sub|idx|iso|mka|mp3|flac|jpg|jpeg|png)$").unwrap());

pub static RE_SXXEXX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bs(\d{1,3})[\s._-]?e(\d{1,4})(?:[^0-9]|$)").unwrap());
static RE_DASH_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:^|[\s\[])[-–]\s*0*(\d{1,4})(?:v\d)?\b").unwrap());
static RE_EP_ONLY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bep(?:isode)?[\s._-]*0*(\d{1,3})(?:[^0-9]|$)").unwrap());
static RE_SEASON: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bs(\d{1,2})\b").unwrap());
static RE_RECUT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)director'?s[\s._-]*cut|\bshin[\s._-]*henshuu-?ban\b|新編集版").unwrap());

static RE_SEASON_RANGE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bS(\d{1,2})\s*[-–—]\s*(?:S)?(\d{1,2})\b").unwrap());
static RE_SEASON_ORDINAL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)[\s._-]+season\b").unwrap());
static RE_SEASON_LONG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bseason[\s._-]*(\d{1,2})\b").unwrap());
static RE_SEASON_SHORT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bS(\d{1,2})(?:[\s._-]*E\d{1,3})?\b").unwrap());

static RE_TITLE_SEP: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s*:\s*|\s+-+\s*|\s*-+\s+").unwrap());
static RE_GENERIC_TAIL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(part|chapter|volume|vol|season|episode|movie|the movie)\b").unwrap());
static RE_NON_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^A-Za-z0-9]+").unwrap());
static RE_BRACKETED: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[[^\]]*\]|\([^)]*\)").unwrap());
static RE_VIDEO_EXT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(mkv|mp4|avi|mov|m4v|webm|ts|wmv|mpg|mpeg|flv)$").unwrap());
static RE_TRAILING_GROUP: Lazy<Regex> = Lazy::new(|| Regex::new(r"-[A-Za-z0-9]+$").unwrap());
static RE_DASH_NUM_MARKER: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s-\s\d{2,4}(?:\s|$)").unwrap());
static RE_X_EP_MARKER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b\d{1,2}x\d{2}\b").unwrap());

static QUALITY_PATTERNS: Lazy<Vec<(Regex, f64)>> = Lazy::new(|| {
    vec![
        (Regex::new(r"(?i)\b(2160p|4k|uhd)\b").unwrap(), 900.0),
        (Regex::new(r"(?i)\b1080p\b").unwrap(), 700.0),
        (Regex::new(r"(?i)\b720p\b").unwrap(), 400.0),
        (Regex::new(r"(?i)\b(480p|sd)\b").unwrap(), 100.0),
        (Regex::new(r"(?i)\b(bluray|blu-ray|bdrip|web-dl|webrip|remux)\b").unwrap(), 350.0),
        (Regex::new(r"(?i)\b(x265|hevc|h\.265)\b").unwrap(), 120.0),
        (Regex::new(r"(?i)\b(hdr|dolby.?vision)\b").unwrap(), 80.0),
        (Regex::new(r"(?i)\b(cam|hdcam|tsrip|telesync|\bts\b|screener)\b").unwrap(), -1200.0),
        (Regex::new(r"(?i)\b(x264|h\.264)\b").unwrap(), 40.0),
    ]
});

pub fn looks_like_non_feature(name: &str) -> bool {
    RE_PROMO.is_match(name) || RE_NON_VIDEO.is_match(name)
}

pub fn looks_like_movie(name: &str) -> bool {
    RE_MOVIE.is_match(name)
}

pub fn announces_film(name: &str) -> bool {
    RE_ANNOUNCED_FILM.is_match(name)
}

pub fn norm_name(s: &str) -> String {
    RE_NON_ALNUM.replace_all(&s.to_lowercase(), "").to_string()
}

pub fn norm_romaji(s: &str) -> String {
    let mut s = s.to_lowercase();
    let reps = [
        ("jyou", "jo"), ("jyo", "jo"), ("jya", "ja"), ("jyu", "ju"),
        ("zyou", "jo"), ("zyo", "jo"), ("zya", "ja"), ("zyu", "ju"), ("zi", "ji"),
        ("shyou", "sho"), ("syou", "sho"), ("syo", "sho"), ("sya", "sha"), ("syu", "shu"), ("si", "shi"),
        ("chyou", "cho"), ("tyou", "cho"), ("tyo", "cho"), ("tya", "cha"), ("tyu", "chu"),
        ("cyou", "cho"), ("cyo", "cho"), ("cya", "cha"), ("cyu", "chu"),
        ("ti", "chi"), ("tu", "tsu"), ("hu", "fu"),
        ("ou", "o"), ("oo", "o"), ("oh", "o"),
        ("uu", "u"), ("aa", "a"), ("ee", "e"), ("ii", "i"),
        ("ō", "o"), ("ū", "u"), ("ā", "a"), ("ī", "i"), ("ē", "e"),
    ];
    for (f, t) in reps {
        s = s.replace(f, t);
    }
    s
}

pub fn looks_like_episode(name: &str) -> bool {
    RE_EP_MARKER_SXXEXX.is_match(name)
        || RE_EP_MARKER_X.is_match(name)
        || RE_EP_MARKER_EP.is_match(name)
        || RE_EP_MARKER_SEASON.is_match(name)
        || RE_EP_MARKER_COMPLETE.is_match(name)
        || RE_EP_MARKER_DASH.is_match(name)
}

pub fn significant_words(title: &str) -> Vec<String> {
    let mut out = vec![];
    for w in RE_NON_ALNUM.split(&title.to_lowercase()) {
        if w.len() >= 3 && w != "the" && w != "and" && w != "of" {
            out.push(w.to_string());
        }
    }
    out
}

fn match_normalized(name: &str, title: &str) -> bool {
    let want = norm_name(title);
    if want.is_empty() {
        return true;
    }
    let have = norm_name(name);
    if have.contains(&want) {
        return true;
    }
    let words = significant_words(title);
    if words.is_empty() {
        return true;
    }
    for w in &words {
        if !have.contains(w) {
            return false;
        }
    }
    let first: Vec<String> = RE_NON_ALNUM.split(&name.to_lowercase()).filter(|w| !w.is_empty()).map(|s| s.to_string()).collect();
    if first.is_empty() {
        return false;
    }
    first[0] == words[0]
}

pub fn title_matches(name: &str, title: &str) -> bool {
    if match_normalized(name, title) {
        return true;
    }
    match_normalized(&norm_romaji(name), &norm_romaji(title))
}

pub fn year_agrees(name: &str, year: i32, slack: i32) -> bool {
    if year <= 0 {
        return true;
    }
    let mut found = false;
    for m in RE_YEAR.find_iter(name) {
        found = true;
        let y: i32 = m.as_str().parse().unwrap_or(0);
        if (y - year).abs() <= slack {
            return true;
        }
    }
    !found
}

pub fn release_season(name: &str, want: i32) -> i32 {
    if let Some(m) = RE_SEASON_RANGE.captures(name) {
        let lo: i32 = m.get(1).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        let hi: i32 = m.get(2).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        let (mut lo, mut hi) = (lo, hi);
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        if want > 0 && want >= lo && want <= hi {
            return 0;
        }
        return lo;
    }
    for re in [&*RE_SEASON_ORDINAL, &*RE_SEASON_LONG, &*RE_SEASON_SHORT] {
        if let Some(m) = re.captures(name) {
            let s: i32 = m.get(1).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
            if s > 0 {
                return s;
            }
        }
    }
    0
}

pub fn identified_for_season(name: &str, season: i32) -> bool {
    if looks_like_episode(name) {
        return true;
    }
    if release_season(name, season) != 0 {
        return true;
    }
    if let Some(m) = RE_SEASON_RANGE.captures(name) {
        let lo: i32 = m.get(1).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        let hi: i32 = m.get(2).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        let (mut lo, mut hi) = (lo, hi);
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        return season >= lo && season <= hi;
    }
    false
}

static RELEASE_FURNITURE: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "bluray", "bdrip", "brrip", "webrip", "hdtv", "web", "dl", "rip", "xvid", "divx", "hevc", "avc",
        "remux", "proper", "repack", "internal", "limited", "extended", "uncut", "remastered", "dual",
        "audio", "multi", "subs", "subbed", "dubbed", "hardcoded", "complete", "series", "batch", "pack",
        "collection", "tv", "www", "com", "org", "net", "nf", "amzn", "dsnp", "hmax", "atvp", "cr",
    ]
    .into_iter()
    .collect()
});

pub fn hint_words(s: &str) -> Vec<String> {
    let mut out = vec![];
    for w in RE_NON_ALNUM.split(&s.to_lowercase()) {
        match w {
            "" | "the" | "and" | "of" | "to" | "in" | "a" | "an" => continue,
            _ => {}
        }
        if w.len() < 2 {
            continue;
        }
        out.push(w.to_string());
    }
    out
}

pub fn extra_title_words(name: &str, titles: &[String]) -> usize {
    let mut s = RE_BRACKETED.replace_all(name, " ").to_string();
    s = RE_VIDEO_EXT.replace_all(&s, "").to_string();
    s = RE_TRAILING_GROUP.replace_all(&s, "").to_string();
    let mut own: HashSet<String> = HashSet::new();
    for t in titles {
        for w in hint_words(t) {
            own.insert(w.clone());
            own.insert(norm_romaji(&w));
        }
    }
    let mut extra: HashSet<String> = HashSet::new();
    for w in hint_words(&s) {
        if own.contains(&w) || own.contains(&norm_romaji(&w)) || RELEASE_FURNITURE.contains(w.as_str()) || w.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        extra.insert(w);
    }
    extra.len()
}

pub fn extra_title_words_before_marker(name: &str, titles: &[String]) -> usize {
    let mut cut = name.len();
    let patterns: Vec<&Regex> = vec![
        &RE_SXXEXX, &RE_DASH_EP, &RE_EP_ONLY, &RE_X_EP_MARKER, &RE_SEASON_RANGE, &RE_SEASON_ORDINAL,
        &RE_SEASON_LONG, &RE_SEASON_SHORT, &RE_DASH_NUM_MARKER, &RE_YEAR,
    ];
    for re in patterns {
        if let Some(m) = re.find(name) {
            if m.start() < cut {
                cut = m.start();
            }
        }
    }
    if cut == name.len() {
        return 0;
    }
    extra_title_words(&name[..cut], titles)
}

pub fn title_forms(title: &str) -> Vec<String> {
    let title = title.trim();
    let mut forms = vec![title.to_string()];
    let parts: Vec<&str> = RE_TITLE_SEP.split(title).collect();
    let mut last = "";
    for s in parts {
        let s = s.trim();
        if !s.is_empty() {
            last = s;
        }
    }
    // SAFETY: last borrows from title; handle lifetime by cloning
    let last_owned = last.to_string();
    if !last_owned.eq_ignore_ascii_case(title) && last_owned.len() >= 6 && last_owned.split_whitespace().count() >= 2 && !RE_GENERIC_TAIL.is_match(&last_owned) {
        forms.push(last_owned);
    }
    forms
}

pub fn clean_title(title: &str) -> String {
    RE_NON_WORD.replace_all(title, " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn with_year(q: &str, year: i32) -> String {
    if year > 0 {
        format!("{q} {year}")
    } else {
        q.to_string()
    }
}

pub fn dedupe_strings(input: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = vec![];
    for s in input {
        let t = s.trim().to_string();
        if t.is_empty() {
            continue;
        }
        let k = t.to_lowercase();
        if seen.contains(&k) {
            continue;
        }
        seen.insert(k);
        out.push(t);
    }
    out
}

pub fn episode_signal(title: &str, season: i32, ep: i32) -> i32 {
    if ep <= 0 {
        return 0;
    }
    if season > 0 {
        let s = release_season(title, season);
        if s > 0 && s != season {
            return -1;
        }
    }
    if RE_RECUT.is_match(title) {
        return -1;
    }
    if announces_film(title) {
        return -1;
    }
    if let Some(m) = RE_SXXEXX.captures(title) {
        let s: i32 = m.get(1).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        let e: i32 = m.get(2).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        if season > 0 && s != season {
            return -1;
        }
        if e == ep {
            return 1;
        }
        return -1;
    }
    if let Some(m) = RE_DASH_EP.captures(title) {
        let e: i32 = m.get(1).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        if e == ep {
            return 1;
        }
        return -1;
    }
    if let Some(m) = RE_EP_ONLY.captures(title) {
        let e: i32 = m.get(1).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        if e == ep {
            return 1;
        }
        return -1;
    }
    0
}

pub fn estimated_mbps(seeders: i32) -> f64 {
    if seeders <= 0 {
        return 0.0;
    }
    (2.0 + 3.0 * (seeders as f64).sqrt()).min(40.0)
}

// Score + filter need ResultRow; define a generic trait to avoid circular deps.
// We define them over a minimal struct here and convert in indexers.rs.
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub title: String,
    pub size: i64,
    pub seeders: i32,
}

pub fn score_candidate(c: &ScoredCandidate, typ: &str, season: i32, ep: i32) -> f64 {
    let mut score = ((c.seeders as f64) + 1.0).ln() * 220.0;
    for (re, bonus) in QUALITY_PATTERNS.iter() {
        if re.is_match(&c.title) {
            score += bonus;
        }
    }
    if c.size > 0 {
        let duration = if typ == "series" { 3000.0 } else { 7200.0 };
        let mut size = c.size as f64;
        if typ == "series" && RE_SEASON.is_match(&c.title) && !RE_SXXEXX.is_match(&c.title) {
            size /= 12.0;
        }
        let need_mbps = size * 8.0 / duration / 1e6;
        let have_mbps = estimated_mbps(c.seeders);
        if need_mbps > have_mbps {
            let pen = ((need_mbps - have_mbps) * 150.0).min(1200.0);
            score -= pen;
        }
    }
    if c.size > 0 {
        const GB: i64 = 6;
        let _ = GB;
        if c.size > 6 * 1024 * 1024 * 1024 && c.size < 60 * 1024 * 1024 * 1024 {
            score += 200.0;
        } else if c.size > 1024 * 1024 * 1024 {
            score += 100.0;
        } else if c.size < 100 * 1024 * 1024 {
            score -= 400.0;
        }
    }
    if typ == "series" && season > 0 {
        if RE_SEASON.is_match(&c.title) && !RE_SXXEXX.is_match(&c.title) {
            score -= 150.0;
        }
        let tag = format!("s{:02}e{:02}", season, ep);
        if c.title.to_lowercase().replace(' ', "").contains(&tag) {
            score += 400.0;
        }
    }
    score
}

pub fn filter_relevant_generic(
    titles_in: &[(String, i64, i32)],
    typ: &str,
    title: &str,
    aliases: &[String],
    year: i32,
    season: i32,
) -> (Vec<usize>, usize) {
    let mut titles = vec![title.to_string()];
    titles.extend_from_slice(aliases);
    let slack = if aliases.is_empty() { 1 } else { 3 };
    let mut keep = vec![];
    let mut dropped = 0;
    for (idx, (name, _size, _seeds)) in titles_in.iter().enumerate() {
        if looks_like_non_feature(name) {
            dropped += 1;
            continue;
        }
        let mut matched = false;
        for t in &titles {
            if title_matches(name, t) {
                matched = true;
                break;
            }
        }
        if !matched {
            dropped += 1;
            continue;
        }
        if typ == "movie" {
            if looks_like_episode(name) {
                dropped += 1;
                continue;
            }
            if !year_agrees(name, year, slack) {
                dropped += 1;
                continue;
            }
        } else if season > 0 {
            if looks_like_movie(name) {
                dropped += 1;
                continue;
            }
            if release_season(name, season) != 0 && release_season(name, season) != season {
                dropped += 1;
                continue;
            }
            if extra_title_words_before_marker(name, &titles) >= 2 {
                dropped += 1;
                continue;
            }
            if !identified_for_season(name, season) {
                if !year_agrees(name, year, slack + (season - 1) * 3) {
                    dropped += 1;
                    continue;
                }
                if extra_title_words(name, &titles) >= 2 {
                    dropped += 1;
                    continue;
                }
            }
        }
        keep.push(idx);
    }
    (keep, dropped)
}

#[allow(dead_code)]
pub fn atoi_safe(s: &str) -> i32 {
    let mut n = 0;
    for c in s.chars() {
        if !c.is_ascii_digit() {
            return n;
        }
        n = n * 10 + (c as i32 - '0' as i32);
    }
    n
}

// fileSignal + chooseFile helpers (pure, used by engine)
use std::path::Path as FsPath;

pub fn is_video_path(p: &str) -> bool {
    let ext = FsPath::new(p).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    matches!(ext.as_str(), "mp4" | "mkv" | "avi" | "mov" | "m4v" | "webm" | "ts" | "wmv" | "mpg" | "mpeg" | "flv")
}

static RE_FILE_EP_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:\[[^\]]*\]\s*)*0*(\d{1,3})(?:v\d)?\s*(?:[.\-_ ]|$)").unwrap());

pub fn file_signal(path: &str, season: i32, ep: i32) -> i32 {
    if ep <= 0 {
        return 0;
    }
    let base = FsPath::new(path).file_name().and_then(|s| s.to_str()).unwrap_or(path);
    let sig = episode_signal(base, season, ep);
    if sig != 0 {
        return sig;
    }
    if let Some(m) = RE_FILE_EP_NUM.captures(base) {
        let n: i32 = m.get(1).map(|v| v.as_str().parse().unwrap_or(-1)).unwrap_or(-1);
        if n == ep {
            return 1;
        }
        return -1;
    }
    0
}

pub fn choose_file(paths: &[String], sizes: &[i64], want: i32) -> anyhow::Result<usize> {
    if paths.is_empty() {
        anyhow::bail!("torrent has no files");
    }
    if want >= 0 && (want as usize) < paths.len() {
        return Ok(want as usize);
    }
    let mut best: Option<usize> = None;
    for (i, p) in paths.iter().enumerate() {
        let lp = p.to_lowercase();
        if !is_video_path(&lp) {
            continue;
        }
        if lp.contains("sample") && paths.len() > 1 {
            continue;
        }
        match best {
            None => best = Some(i),
            Some(b) if sizes[i] > sizes[b] => best = Some(i),
            _ => {}
        }
    }
    if let Some(b) = best {
        return Ok(b);
    }
    // fallback: largest any
    let mut b = 0usize;
    for i in 1..paths.len() {
        if sizes[i] > sizes[b] {
            b = i;
        }
    }
    if paths.is_empty() {
        anyhow::bail!("no playable file found");
    }
    Ok(b)
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct NoEpisode;

impl std::fmt::Display for NoEpisode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "release has no file for this episode")
    }
}
impl std::error::Error for NoEpisode {}

pub fn choose_file_for_episode(paths: &[String], sizes: &[i64], want: i32, season: i32, ep: i32) -> anyhow::Result<usize> {
    if want >= 0 && (want as usize) < paths.len() {
        return Ok(want as usize);
    }
    if ep > 0 {
        let mut numbered = 0;
        let mut videos = 0;
        for (i, p) in paths.iter().enumerate() {
            if !is_video_path(p) {
                continue;
            }
            videos += 1;
            match file_signal(p, season, ep) {
                s if s > 0 => return Ok(i),
                s if s < 0 => numbered += 1,
                _ => {}
            }
        }
        if numbered > 0 {
            anyhow::bail!("release has no file for this episode");
        }
        if videos > 3 {
            anyhow::bail!("release has no file for this episode");
        }
    }
    choose_file(paths, sizes, want)
}

pub fn choose_file_for_title(paths: &[String], sizes: &[i64], want: i32, hints: &[String]) -> anyhow::Result<usize> {
    if want >= 0 && (want as usize) < paths.len() {
        return Ok(want as usize);
    }
    let words = hint_words(&hints.join(" "));
    if words.is_empty() {
        return choose_file(paths, sizes, want);
    }
    let mut sets: Vec<Option<HashSet<String>>> = vec![None; paths.len()];
    for (i, p) in paths.iter().enumerate() {
        if !is_video_path(p) {
            continue;
        }
        let base = FsPath::new(p).file_name().and_then(|s| s.to_str()).unwrap_or(p).to_lowercase();
        let mut set = HashSet::new();
        for w in RE_NON_ALNUM.split(&base) {
            if !w.is_empty() {
                set.insert(w.to_string());
            }
        }
        sets[i] = Some(set);
    }
    let mut disc = vec![];
    for w in &words {
        let mut in_all = true;
        let mut any = false;
        for s in &sets {
            if let Some(set) = s {
                any = true;
                if !set.contains(w) {
                    in_all = false;
                    break;
                }
            }
        }
        if any && !in_all {
            disc.push(w.clone());
        }
    }
    let mut best: Option<usize> = None;
    let mut best_hits = 0;
    for (i, set) in sets.iter().enumerate() {
        let set = match set {
            Some(s) => s,
            None => continue,
        };
        let mut hits = 0;
        for w in &disc {
            if set.contains(w) {
                hits += 1;
            }
        }
        if hits > best_hits || (hits == best_hits && best.is_some() && hits > 0 && sizes[i] > sizes[best.unwrap()]) {
            best = Some(i);
            best_hits = hits;
        }
    }
    if let Some(b) = best {
        if best_hits > 0 {
            return Ok(b);
        }
    }
    choose_file(paths, sizes, want)
}

pub fn remap_episode(eps: &[(i32, i32)], season: i32, ep: i32) -> (i32, i32) {
    if eps.is_empty() {
        return (season, ep);
    }
    if season != eps[0].0 {
        return (season, ep);
    }
    let mut in_season = 0;
    for e in eps {
        if e.0 == season {
            in_season += 1;
        }
    }
    if ep <= in_season {
        return (season, ep);
    }
    if ep as usize <= eps.len() {
        return eps[(ep - 1) as usize];
    }
    (season, ep)
}
