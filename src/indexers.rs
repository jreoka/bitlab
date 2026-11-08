use regex::Regex;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Indexer {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

pub fn default_indexers() -> Vec<Indexer> {
    vec![
        Indexer { kind: "knaben".into(), name: "knaben".into(), url: "".into(), api_key: "".into(), enabled: true },
        Indexer { kind: "nyaa".into(), name: "nyaa".into(), url: "".into(), api_key: "".into(), enabled: true },
        Indexer { kind: "eztv".into(), name: "eztv".into(), url: "".into(), api_key: "".into(), enabled: true },
        Indexer { kind: "yts".into(), name: "yts".into(), url: "".into(), api_key: "".into(), enabled: true },
        Indexer { kind: "animetosho".into(), name: "animetosho".into(), url: "".into(), api_key: "".into(), enabled: true },
        Indexer { kind: "torrentio".into(), name: "torrentio".into(), url: "".into(), api_key: "".into(), enabled: true },
        Indexer { kind: "apibay".into(), name: "apibay".into(), url: "".into(), api_key: "".into(), enabled: true },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultRow {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub infohash: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub seeders: i32,
    #[serde(default)]
    pub indexer: String,
    #[serde(default)]
    pub magnet: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub trackers: Vec<String>,
}

static IH_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"(?i)\burn:btih:([a-z0-9]{32,40})\b").unwrap());

pub fn magnet_uri(ih: &str, name: &str, trackers: &[String]) -> String {
    let mut b = format!("magnet:?xt=urn:btih:{ih}");
    if !name.is_empty() {
        b.push_str(&format!("&dn={}", urlencoding(&name)));
    }
    for t in trackers {
        b.push_str(&format!("&tr={}", urlencoding(t)));
    }
    b
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub fn parse_size(s: &str) -> i64 {
    let s = s.trim();
    let fields: Vec<&str> = s.split_whitespace().collect();
    if fields.len() != 2 {
        return 0;
    }
    let num: f64 = fields[0].replace(',', "").parse().unwrap_or(0.0);
    if !num.is_finite() || num < 0.0 {
        return 0;
    }
    let mult = match fields[1].to_lowercase().as_str() {
        "b" => 1.0,
        "kib" | "kb" => 1024.0,
        "mib" | "mb" => (1024 * 1024) as f64,
        "gib" | "gb" => (1024 * 1024 * 1024) as f64,
        "tib" | "tb" => (1024_i64 * 1024 * 1024 * 1024) as f64,
        _ => return 0,
    };
    let bytes = num * mult;
    if !bytes.is_finite() || bytes < 0.0 || bytes > i64::MAX as f64 {
        return 0;
    }
    bytes as i64
}

pub fn indexer_timeout(kind: &str) -> Duration {
    match kind {
        "animetosho" => Duration::from_secs(6),
        "eztv" | "yts" | "torrentio" => Duration::from_secs(10),
        _ => Duration::from_secs(15),
    }
}

pub fn queries_for(kind: &str, queries: &[String]) -> Vec<String> {
    match kind {
        "eztv" | "yts" | "torrentio" => {
            if queries.len() > 1 {
                queries[..1].to_vec()
            } else {
                queries.to_vec()
            }
        }
        "nyaa" | "apibay" | "animetosho" => {
            if queries.len() > 3 {
                queries[..3].to_vec()
            } else {
                queries.to_vec()
            }
        }
        _ => queries.to_vec(),
    }
}

fn loose_query(q: &str) -> String {
    let re_ep = Regex::new(r"(?i)\bS\d{2}E\d{2}\b").unwrap();
    let re_yr = Regex::new(r"\b(19|20)\d{2}\b").unwrap();
    let s = re_ep.replace_all(q, " ");
    let s = re_yr.replace_all(&s, " ");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn trackers_from_magnet(m: &str) -> Vec<String> {
    let u = match url::Url::parse(m) {
        Ok(u) => u,
        Err(_) => return vec![],
    };
    u.query_pairs().filter(|(k, _)| k == "tr").map(|(_, v)| v.to_string()).collect()
}

// ---------- Torznab ----------

#[derive(Debug, Deserialize)]
struct TorznabFeed {
    #[serde(rename = "channel")]
    channel: TorznabChannel,
}

#[derive(Debug, Deserialize, Default)]
struct TorznabChannel {
    #[serde(rename = "item", default)]
    items: Vec<TorznabItem>,
}

#[derive(Debug, Deserialize, Default)]
struct TorznabItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    guid: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    enclosure: Option<TorznabEnclosure>,
    #[serde(rename = "attr", default)]
    attrs: Vec<TorznabAttr>,
}

#[derive(Debug, Deserialize, Default)]
struct TorznabEnclosure {
    #[serde(rename = "@url", default)]
    url: String,
    #[serde(rename = "@type", default)]
    #[allow(dead_code)]
    typ: String,
}

#[derive(Debug, Deserialize, Default)]
struct TorznabAttr {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@value", default)]
    value: String,
}

pub async fn torznab_search(client: &reqwest::Client, ix: &Indexer, q: &str, typ: &str, season: i32, ep: i32, imdb: &str) -> anyhow::Result<Vec<ResultRow>> {
    let mut base = ix.url.trim_end_matches('/').to_string();
    if !base.ends_with("/api") {
        base.push_str("/api");
    }
    let mut u = url::Url::parse(&base)?;
    {
        let mut qp = u.query_pairs_mut();
        if !ix.api_key.is_empty() {
            qp.append_pair("apikey", &ix.api_key);
        }
        qp.append_pair("limit", "100");
        qp.append_pair("extended", "1");
        match typ {
            "series" => {
                qp.append_pair("t", "tvsearch");
                qp.append_pair("q", q);
                if season > 0 {
                    qp.append_pair("season", &season.to_string());
                    qp.append_pair("ep", &ep.to_string());
                }
            }
            "movie" => {
                qp.append_pair("t", "movie");
                qp.append_pair("q", q);
                if !imdb.is_empty() {
                    qp.append_pair("imdbid", imdb.trim_start_matches("tt"));
                }
            }
            _ => {
                qp.append_pair("t", "search");
                qp.append_pair("q", q);
            }
        }
    }
    let req = client.get(u).header("User-Agent", "bitlab/1.0").build()?;
    let resp = client.execute(req).await?;
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    let feed: TorznabFeed = quick_xml::de::from_reader(&body[..]).map_err(|e| anyhow::anyhow!("bad xml: {e}"))?;
    let mut out = vec![];
    for it in feed.channel.items {
        let mut r = ResultRow { title: it.title.trim().to_string(), indexer: ix.name.clone(), size: 0, infohash: String::new(), seeders: 0, magnet: String::new(), score: 0.0, trackers: vec![] };
        let mut size_i: i64 = it.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
        for a in &it.attrs {
            match a.name.to_lowercase().as_str() {
                "infohash" => r.infohash = a.value.to_lowercase().trim().to_string(),
                "seeders" => r.seeders = a.value.parse().unwrap_or(0),
                "size" => {
                    if let Ok(n) = a.value.parse::<i64>() {
                        size_i = n;
                    }
                }
                "magneturl" => r.magnet = a.value.clone(),
                _ => {}
            }
        }
        r.size = size_i;
        if r.infohash.is_empty() {
            for cand in [&it.enclosure.map(|e| e.url).unwrap_or_default(), &it.link, &it.guid, &r.magnet] {
                if let Some(m) = IH_RE.captures(cand) {
                    r.infohash = m.get(1).unwrap().as_str().to_lowercase();
                    break;
                }
            }
        }
        if r.infohash.len() != 40 {
            continue;
        }
        out.push(r);
    }
    Ok(out)
}

// ---------- apibay ----------

#[derive(Debug, Deserialize)]
struct ApibayRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    info_hash: String,
    #[serde(default)]
    seeders: String,
    #[serde(default)]
    #[allow(dead_code)]
    leechers: String,
    #[serde(default)]
    size: String,
    #[serde(default)]
    category: String,
}

pub async fn apibay_search(client: &reqwest::Client, ix: &Indexer, q: &str) -> anyhow::Result<Vec<ResultRow>> {
    let base = if ix.url.trim().is_empty() { "https://apibay.org".to_string() } else { ix.url.trim_end_matches('/').to_string() };
    let u = format!("{base}/q.php?q={}&cat=0", url::form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>());
    let resp = client.get(&u).header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) bitlab/1.0").send().await?;
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    let raw: Vec<ApibayRow> = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))?;
    let mut out = vec![];
    for r in raw {
        if r.id == "0" || r.name == "No results returned" || r.info_hash.len() != 40 {
            continue;
        }
        if !r.category.is_empty() && !r.category.starts_with('2') {
            continue;
        }
        out.push(ResultRow {
            title: r.name,
            infohash: r.info_hash.to_lowercase(),
            size: r.size.parse().unwrap_or(0),
            seeders: r.seeders.parse().unwrap_or(0),
            indexer: ix.name.clone(),
            magnet: String::new(),
            score: 0.0,
            trackers: vec![],
        });
    }
    Ok(out)
}

// ---------- knaben ----------

pub async fn knaben_search(client: &reqwest::Client, ix: &Indexer, q: &str) -> anyhow::Result<Vec<ResultRow>> {
    match knaben_request(client, ix, q).await {
        Ok(v) if !v.is_empty() => Ok(v),
        Ok(v) => {
            if let Some(alt) = {
                let a = loose_query(q);
                if a != q { Some(a) } else { None }
            } {
                if !alt.is_empty() {
                    if let Ok(v2) = knaben_request(client, ix, &alt).await {
                        return Ok(v2);
                    }
                }
            }
            Ok(v)
        }
        Err(e) => {
            let alt = loose_query(q);
            if !alt.is_empty() && alt != q {
                if let Ok(v2) = knaben_request(client, ix, &alt).await {
                    return Ok(v2);
                }
            }
            Err(e)
        }
    }
}

async fn knaben_request(client: &reqwest::Client, ix: &Indexer, q: &str) -> anyhow::Result<Vec<ResultRow>> {
    let mut base = if ix.url.trim().is_empty() { "https://api.knaben.org".to_string() } else { ix.url.trim_end_matches('/').to_string() };
    if !base.ends_with("/v1") {
        base.push_str("/v1");
    }
    let payload = serde_json::json!({
        "search_type": "100%",
        "search_field": "title",
        "query": q,
        "order_by": "seeders",
        "order_direction": "desc",
        "categories": [2000000, 5000000],
        "from": 0, "size": 100,
        "hide_unsafe": true, "hide_xxx": true,
    });
    let resp = client.post(&base).json(&payload)
        .header("Accept", "application/json")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .send().await?;
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        hits: Vec<Hit>,
    }
    #[derive(Deserialize)]
    struct Hit {
        #[serde(default)]
        title: String,
        #[serde(default)]
        hash: String,
        #[serde(default)]
        bytes: i64,
        #[serde(default, rename = "liveSeeders")]
        seeders: i32,
        #[serde(default)]
        category: String,
        #[serde(default, rename = "magnetUrl")]
        magnet: String,
        #[serde(default)]
        #[allow(dead_code)]
        tracker: String,
    }
    let out: Resp = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))?;
    let skip: std::collections::HashSet<&str> = ["audio", "applications", "apps", "pc", "games", "books", "ebook", "ebooks", "software", "android", "mac", "windows", "xxx", "other"].into_iter().collect();
    let mut res = vec![];
    for h in out.hits {
        if h.hash.len() != 40 || skip.contains(h.category.to_lowercase().as_str()) {
            continue;
        }
        res.push(ResultRow {
            title: h.title,
            infohash: h.hash.to_lowercase(),
            size: h.bytes,
            seeders: h.seeders,
            indexer: ix.name.clone(),
            magnet: h.magnet.clone(),
            score: 0.0,
            trackers: trackers_from_magnet(&h.magnet),
        });
    }
    Ok(res)
}

// ---------- nyaa ----------

#[derive(Debug, Deserialize, Default)]
struct NyaaFeed {
    #[serde(rename = "channel", default)]
    channel: NyaaChannel,
}
#[derive(Debug, Deserialize, Default)]
struct NyaaChannel {
    #[serde(rename = "item", default)]
    items: Vec<NyaaItem>,
}
#[derive(Debug, Deserialize, Default)]
struct NyaaItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    #[allow(dead_code)]
    link: String,
    #[serde(default, rename = "infoHash")]
    infohash: String,
    #[serde(default)]
    seeders: i32,
    #[serde(default)]
    size: String,
    #[serde(default)]
    #[allow(dead_code)]
    category: String,
}

pub async fn nyaa_search(client: &reqwest::Client, ix: &Indexer, q: &str) -> anyhow::Result<Vec<ResultRow>> {
    let mut base = if ix.url.trim().is_empty() { "https://nyaa.si".to_string() } else { ix.url.clone() };
    let mut cat = "1_2".to_string();
    if let Some(i) = base.find("/c=") {
        cat = base[i + 3..].trim_end_matches('/').to_string();
        base = base[..i].trim_end_matches('/').to_string();
    }
    let u = format!("{}/?page=rss&q={}&c={}&f=0&s=seeders&o=desc",
        base.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(cat.as_bytes()).collect::<String>());
    let resp = client.get(&u).header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) bitlab/1.0").send().await?;
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    let feed: NyaaFeed = quick_xml::de::from_reader(&body[..]).map_err(|e| anyhow::anyhow!("bad xml: {e}"))?;
    let mut out = vec![];
    for it in feed.channel.items {
        let ih = it.infohash.to_lowercase().trim().to_string();
        if ih.len() != 40 {
            continue;
        }
        out.push(ResultRow {
            title: it.title.trim().to_string(),
            infohash: ih,
            size: parse_size(&it.size),
            seeders: it.seeders,
            indexer: ix.name.clone(),
            magnet: String::new(),
            score: 0.0,
            trackers: vec!["http://nyaa.tracker.wf:7777/announce".to_string()],
        });
    }
    Ok(out)
}

// ---------- eztv ----------

#[derive(Debug, Deserialize)]
struct EztvTorrent {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    filename: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "magnet_url")]
    magnet: String,
    #[serde(default)]
    season: String,
    #[serde(default)]
    episode: String,
    #[serde(default)]
    seeds: i32,
    #[serde(default, deserialize_with = "de_flex_i64")]
    size_bytes: i64,
}

fn de_flex_i64<'de, D>(d: D) -> Result<i64, D::Error>
where D: serde::Deserializer<'de> {
    use serde::de::{self, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = i64;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, "int or string") }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> { Ok(v) }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> { Ok(v.min(i64::MAX as u64) as i64) }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            let s = v.trim().trim_matches('"');
            if s.is_empty() || s == "null" { return Ok(0); }
            Ok(s.parse::<f64>().unwrap_or(0.0).clamp(0.0, i64::MAX as f64) as i64)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<i64, E> { self.visit_str(&v) }
        fn visit_none<E: de::Error>(self) -> Result<i64, E> { Ok(0) }
        fn visit_unit<E: de::Error>(self) -> Result<i64, E> { Ok(0) }
    }
    d.deserialize_any(V)
}

pub async fn eztv_search(client: &reqwest::Client, ix: &Indexer, typ: &str, season: i32, ep: i32, imdb: &str) -> anyhow::Result<Vec<ResultRow>> {
    if typ != "series" || imdb.is_empty() {
        return Ok(vec![]);
    }
    let base = if ix.url.trim().is_empty() { "https://eztvx.to".to_string() } else { ix.url.trim_end_matches('/').to_string() };
    let u = format!("{base}/api/get-torrents?limit=100&imdb_id={}", url::form_urlencoded::byte_serialize(imdb.trim_start_matches("tt").as_bytes()).collect::<String>());
    let resp = client.get(&u).header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) bitlab/1.0").header("Accept", "application/json").send().await?;
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    #[derive(Deserialize)]
    struct Feed {
        #[serde(default)]
        torrents: Vec<EztvTorrent>,
    }
    let feed: Feed = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))?;
    let mut res = vec![];
    for t in feed.torrents {
        let ih = t.hash.to_lowercase().trim().to_string();
        if ih.len() != 40 {
            continue;
        }
        let s: i32 = t.season.trim().parse().unwrap_or(-1);
        let e: i32 = t.episode.trim().parse().unwrap_or(-1);
        if season > 0 {
            if s != season {
                continue;
            }
            if ep > 0 && e != 0 && e != ep {
                continue;
            }
        }
        let mut title = t.title.trim().to_string();
        if title.is_empty() {
            title = t.filename.trim().to_string();
        }
        if title.is_empty() {
            continue;
        }
        res.push(ResultRow {
            title,
            infohash: ih,
            size: t.size_bytes,
            seeders: t.seeds,
            indexer: ix.name.clone(),
            magnet: t.magnet.clone(),
            score: 0.0,
            trackers: trackers_from_magnet(&t.magnet),
        });
    }
    Ok(res)
}

// ---------- yts ----------

#[derive(Debug, Deserialize)]
struct YtsTorrent {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    quality: String,
    #[serde(default, rename = "type")]
    typ: String,
    #[serde(default, rename = "video_codec")]
    codec: String,
    #[serde(default)]
    seeds: i32,
    #[serde(default)]
    size_bytes: i64,
}
#[derive(Debug, Deserialize)]
struct YtsMovie {
    #[serde(default)]
    title: String,
    #[serde(default)]
    year: i32,
    #[serde(default, rename = "imdb_code")]
    imdb: String,
    #[serde(default)]
    torrents: Vec<YtsTorrent>,
}

pub async fn yts_search(client: &reqwest::Client, ix: &Indexer, typ: &str, imdb: &str, q: &str) -> anyhow::Result<Vec<ResultRow>> {
    if typ != "movie" {
        return Ok(vec![]);
    }
    let mut term = imdb.trim().to_string();
    if term.is_empty() {
        term = q.trim().to_string();
    }
    if term.is_empty() {
        return Ok(vec![]);
    }
    let base = if ix.url.trim().is_empty() { "https://yts.lt".to_string() } else { ix.url.trim_end_matches('/').to_string() };
    let u = format!("{base}/api/v2/list_movies.json?query_term={}&limit=20&sort_by=seeds&order_by=desc",
        url::form_urlencoded::byte_serialize(term.as_bytes()).collect::<String>());
    let resp = client.get(&u).header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) bitlab/1.0").header("Accept", "application/json").send().await?;
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    #[derive(Deserialize)]
    struct Feed {
        #[serde(default)]
        data: YtsData,
    }
    #[derive(Deserialize, Default)]
    struct YtsData {
        #[serde(default)]
        movies: Vec<YtsMovie>,
    }
    let feed: Feed = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))?;
    let mut res = vec![];
    for m in feed.data.movies {
        if !imdb.is_empty() && !m.imdb.is_empty() && !m.imdb.eq_ignore_ascii_case(imdb) {
            continue;
        }
        for t in &m.torrents {
            let ih = t.hash.to_lowercase().trim().to_string();
            if ih.len() != 40 {
                continue;
            }
            let mut src = t.typ.to_uppercase();
            if src == "BLURAY" {
                src = "BluRay".to_string();
            }
            let mut bits = vec![m.title.clone()];
            if m.year > 0 {
                bits.push(m.year.to_string());
            }
            for b in [&t.quality, &src, &t.codec] {
                if !b.trim().is_empty() {
                    bits.push(b.clone());
                }
            }
            res.push(ResultRow {
                title: bits.join(" "),
                infohash: ih,
                size: t.size_bytes,
                seeders: t.seeds,
                indexer: ix.name.clone(),
                magnet: String::new(),
                score: 0.0,
                trackers: vec![],
            });
        }
    }
    Ok(res)
}

// ---------- animetosho ----------

#[derive(Debug, Deserialize)]
struct ToshoEntry {
    #[serde(default)]
    title: String,
    #[serde(default)]
    torrent_name: String,
    #[serde(default)]
    info_hash: String,
    #[serde(default)]
    magnet: String,
    #[serde(default, rename = "magnet_uri")]
    magnet_uri: String,
    #[serde(default)]
    seeders: i32,
    #[serde(default)]
    total_size: i64,
}

fn tosho_title_query(q: &str) -> String {
    let re_ep = Regex::new(r"(?i)\bS\d{2}E\d{2}\b").unwrap();
    let re_yr = Regex::new(r"\b(19|20)\d{2}\b").unwrap();
    let s = re_ep.replace_all(q, " ");
    let s = re_yr.replace_all(&s, " ");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn contains_word(hay: &str, needle: &str) -> bool {
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if n.is_empty() {
        return false;
    }
    let hb = h.as_bytes();
    let nb = n.as_bytes();
    let mut start = 0;
    while start + nb.len() <= hb.len() {
        if let Some(pos) = h[start..].find(&n) {
            let i = start + pos;
            let before = i == 0 || !hb[i - 1].is_ascii_alphanumeric();
            let after = i + nb.len() == hb.len() || !hb[i + nb.len()].is_ascii_alphanumeric();
            if before && after {
                return true;
            }
            start = i + 1;
        } else {
            return false;
        }
    }
    false
}

fn tosho_relevant(q: &str, title: &str) -> bool {
    let mut words = vec![];
    let mut long = vec![];
    for f in tosho_title_query(q).split_whitespace() {
        let w = f.trim_matches(|c: char| ".,:;!?'\"()[]-".contains(c));
        if w.is_empty() {
            continue;
        }
        words.push(w.to_string());
        if w.len() >= 4 {
            long.push(w.to_string());
        }
    }
    if !long.is_empty() {
        words = long;
    }
    if words.is_empty() {
        return true;
    }
    for w in words {
        if contains_word(title, &w) {
            return true;
        }
    }
    false
}

pub async fn animetosho_search(client: &reqwest::Client, ix: &Indexer, q: &str) -> anyhow::Result<Vec<ResultRow>> {
    let base = if ix.url.trim().is_empty() { "https://animetosho.org".to_string() } else { ix.url.trim_end_matches('/').to_string() };
    let mut term = tosho_title_query(q);
    if term.is_empty() {
        term = q.to_string();
    }
    let u = format!("{base}/feed/json?q={}", url::form_urlencoded::byte_serialize(term.as_bytes()).collect::<String>());
    let resp = client.get(&u).header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) bitlab/1.0").header("Accept", "application/json").send().await?;
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    // Cap at 16MB like Go
    if body.len() > 16 * 1024 * 1024 {
        anyhow::bail!("response too large");
    }
    let feed: Vec<ToshoEntry> = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))?;
    let mut res = vec![];
    for e in feed {
        let ih = e.info_hash.to_lowercase().trim().to_string();
        if ih.len() != 40 {
            continue;
        }
        let mut title = e.torrent_name.trim().to_string();
        if title.is_empty() {
            title = e.title.trim().to_string();
        }
        if title.is_empty() {
            continue;
        }
        if !tosho_relevant(q, &title) {
            continue;
        }
        let mag = if !e.magnet.is_empty() { e.magnet.clone() } else { e.magnet_uri.clone() };
        res.push(ResultRow {
            title,
            infohash: ih,
            size: e.total_size,
            seeders: e.seeders,
            indexer: ix.name.clone(),
            magnet: mag.clone(),
            score: 0.0,
            trackers: trackers_from_magnet(&mag),
        });
    }
    Ok(res)
}

// ---------- torrentio ----------

static RE_TIO_SEEDS: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"👤\s*(\d+)").unwrap());
static RE_TIO_SIZE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"💾\s*([0-9]+(?:\.[0-9]+)?\s*[KMGT]?i?B)").unwrap());
static RE_TIO_SRC: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"⚙️\s*([^\n]+)").unwrap());
static RE_RES_TOKEN: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"(?i)\b(2160p|1080p|720p|480p|4k|uhd)\b").unwrap());
static RE_LATIN_WORD: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"[A-Za-z]{4,}").unwrap());
static RE_HASH40: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"^[0-9a-f]{40}$").unwrap());
static RE_IMDB: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| Regex::new(r"^tt[0-9]{6,10}$").unwrap());

#[derive(Debug, Deserialize)]
struct TorrentioStream {
    #[serde(default)]
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "infoHash")]
    infohash: String,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default, rename = "behaviorHints")]
    behavior_hints: TorrentioHints,
}
#[derive(Debug, Deserialize, Default)]
struct TorrentioHints {
    #[serde(default)]
    filename: String,
}

fn torrentio_quality(name: &str) -> String {
    if let Some(i) = name.find('\n') {
        name[i + 1..].trim().to_string()
    } else {
        String::new()
    }
}

pub async fn torrentio_search(client: &reqwest::Client, ix: &Indexer, typ: &str, season: i32, ep: i32, imdb: &str) -> anyhow::Result<Vec<ResultRow>> {
    if !RE_IMDB.is_match(imdb) {
        anyhow::bail!("bad imdb id");
    }
    if imdb.is_empty() || (typ == "series" && season <= 0) {
        return Ok(vec![]);
    }
    let base = if ix.url.trim().is_empty() { "https://torrentio.strem.fun".to_string() } else { ix.url.trim_end_matches('/').to_string() };
    let id = if typ == "series" { format!("{imdb}:{season}:{ep}") } else { imdb.to_string() };
    let u = format!("{base}/stream/{typ}/{id}.json");
    let resp = client.get(&u).header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36").send().await?;
    if resp.status() == 404 {
        return Ok(vec![]);
    }
    if resp.status() != 200 {
        anyhow::bail!("http {}", resp.status());
    }
    let body = resp.bytes().await?;
    if body.len() > 16 * 1024 * 1024 {
        anyhow::bail!("response too large");
    }
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        streams: Vec<TorrentioStream>,
    }
    let payload: Payload = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))?;
    let mut out = vec![];
    for st in payload.streams {
        let ih = st.infohash.to_lowercase().trim().to_string();
        if !RE_HASH40.is_match(&ih) {
            continue;
        }
        let lines: Vec<&str> = st.title.split('\n').collect();
        let mut name = lines.first().map(|s| s.trim().to_string()).unwrap_or_default();
        if name.is_empty() {
            name = st.behavior_hints.filename.trim().to_string();
        }
        if name.is_empty() || !RE_LATIN_WORD.is_match(&name) {
            continue;
        }
        let q = torrentio_quality(&st.name);
        if !q.is_empty() && !RE_RES_TOKEN.is_match(&name) {
            name = format!("{name} {q}");
        }
        let meta = if lines.len() > 1 { lines[1..].join("\n") } else { String::new() };
        let mut r = ResultRow { title: name, infohash: ih, indexer: ix.name.clone(), size: 0, seeders: 0, magnet: String::new(), score: 0.0, trackers: vec![] };
        if let Some(m) = RE_TIO_SEEDS.captures(&meta) {
            r.seeders = m.get(1).map(|v| v.as_str().parse().unwrap_or(0)).unwrap_or(0);
        }
        if let Some(m) = RE_TIO_SIZE.captures(&meta) {
            r.size = parse_size(m.get(1).unwrap().as_str());
        }
        if let Some(m) = RE_TIO_SRC.captures(&meta) {
            let src = m.get(1).unwrap().as_str().trim().to_string();
            if !src.is_empty() {
                r.indexer = format!("{}/{}", ix.name, src);
            }
        }
        for src in &st.sources {
            if let Some(t) = src.strip_prefix("tracker:") {
                r.trackers.push(t.to_string());
            }
        }
        out.push(r);
    }
    Ok(out)
}
