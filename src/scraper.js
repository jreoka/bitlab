import { cacheGet, cachePut } from './cache.js';

export const STREAM_CACHE_TTL_SECS = 3600;

const PUBLIC_TRACKERS = [
  'udp://tracker.opentrackr.org:1337/announce',
  'udp://open.stealth.si:80/announce',
  'udp://tracker.torrent.eu.org:451/announce',
  'udp://tracker.tiny-vps.com:6969/announce',
];

const UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36';
const ACCEPT =
  'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/json';
const ACCEPT_LANG = 'en-US,en;q=0.9';

const TITLE_PREFIXES = [
  '🌸 Nyaa: ',
  '🎬 TPB: ',
  '🎬 APIBay: ',
  '🎬 SolidTorrents: ',
  '🎬 Bitsearch: ',
  '📺 EZTV: ',
  '🎬 YTS: ',
];

// -----------------------------------------------------------------------------
// HTTP helpers
// -----------------------------------------------------------------------------
function baseHeaders(extra) {
  return { 'User-Agent': UA, Accept: ACCEPT, 'Accept-Language': ACCEPT_LANG, ...extra };
}

async function httpGet(url, { timeoutMs = 3000, headers } = {}) {
  try {
    return await fetch(url, {
      headers: baseHeaders(headers),
      signal: AbortSignal.timeout(timeoutMs),
    });
  } catch {
    return null;
  }
}

async function httpJson(url, { timeoutMs = 3000, headers, requireOk = true } = {}) {
  const res = await httpGet(url, { timeoutMs, headers });
  if (!res || (requireOk && !res.ok)) return null;
  try {
    return await res.json();
  } catch {
    return null;
  }
}

async function httpText(url, { timeoutMs = 3000, headers, requireOk = true } = {}) {
  const res = await httpGet(url, { timeoutMs, headers });
  if (!res || (requireOk && !res.ok)) return null;
  try {
    return await res.text();
  } catch {
    return null;
  }
}

// -----------------------------------------------------------------------------
// Regex helper (fresh RegExp per call so /g state never leaks)
// -----------------------------------------------------------------------------
function matches(str, source) {
  const out = [];
  const rx = new RegExp(source, 'g');
  let m;
  while ((m = rx.exec(str)) !== null) {
    out.push(m);
    if (m.index === rx.lastIndex) rx.lastIndex++;
  }
  return out;
}

// -----------------------------------------------------------------------------
// Info hash helpers
// -----------------------------------------------------------------------------
function base32ToHex(b32) {
  let bits = 0;
  let bitCount = 0;
  const bytes = [];
  for (const ch of b32) {
    if (ch === '=') continue;
    const upper = ch.toUpperCase();
    let val;
    if (upper >= 'A' && upper <= 'Z') val = upper.charCodeAt(0) - 65;
    else if (upper >= '2' && upper <= '7') val = upper.charCodeAt(0) - 50 + 26;
    else return null;
    bits = (bits << 5) | val;
    bitCount += 5;
    if (bitCount >= 8) {
      bytes.push((bits >> (bitCount - 8)) & 0xff);
      bitCount -= 8;
    }
  }
  return bytes.map((b) => b.toString(16).padStart(2, '0')).join('');
}

export function normalizeInfoHash(hash) {
  const cleaned = hash.trim();
  const hashNopad = cleaned.replace(/=/g, '');
  if (hashNopad.length === 32) {
    const hex = base32ToHex(hashNopad);
    if (hex) return hex.toLowerCase();
  }
  return cleaned.toLowerCase();
}

function isZeroInfoHash(hash) {
  const h = hash.trim();
  return h.length > 0 && /^0+$/.test(h);
}

export function isValidInfoHash(hash) {
  const normalized = normalizeInfoHash(hash);
  return normalized.length === 40 && /^[0-9a-f]{40}$/.test(normalized) && !isZeroInfoHash(normalized);
}

export function extractHashFromMagnet(magnet) {
  const prefix = 'magnet:?xt=urn:btih:';
  const idx = magnet.toLowerCase().indexOf(prefix);
  if (idx === -1) return null;
  const hashStart = idx + prefix.length;
  const sub = magnet.slice(hashStart);
  const hashPart = sub.includes('&') ? sub.slice(0, sub.indexOf('&')) : sub;
  return normalizeInfoHash(hashPart);
}

function isApibayResultValid(name, infoHash) {
  return (
    name !== '' &&
    infoHash !== '' &&
    name !== 'No results returned' &&
    name !== 'No results found' &&
    isValidInfoHash(infoHash)
  );
}

function getSourcesForTorrent(infoHash) {
  const sources = [`dht:${infoHash}`];
  for (const tracker of PUBLIC_TRACKERS) sources.push(`tracker:${tracker}`);
  return sources;
}

function extractTrackersFromMagnet(magnet, infoHash) {
  const sources = [`dht:${infoHash}`];
  for (const t of PUBLIC_TRACKERS) sources.push(`tracker:${t}`);
  const parts = magnet.split('&tr=');
  if (parts.length > 1) {
    for (const part of parts.slice(1)) {
      const trackerEncoded = part.split('&')[0];
      if (trackerEncoded) {
        try {
          const decoded = decodeURIComponent(trackerEncoded);
          const source = `tracker:${decoded}`;
          if (!sources.includes(source)) sources.push(source);
        } catch {
          // malformed encoding, skip
        }
      }
    }
  }
  return sources;
}

// -----------------------------------------------------------------------------
// String helpers
// -----------------------------------------------------------------------------
export function decodeHtmlEntities(s) {
  return s
    .replace(/&#39;/g, "'")
    .replace(/&#x27;/g, "'")
    .replace(/&amp;/g, '&')
    .replace(/&quot;/g, '"')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>');
}

export function formatSize(bytesStr) {
  const bytes = parseInt(bytesStr, 10);
  if (!Number.isNaN(bytes)) {
    if (bytes >= 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
    if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(2)} MiB`;
    return `${bytes} B`;
  }
  return 'Unknown size';
}

function str(v) {
  if (typeof v === 'string') return v;
  if (typeof v === 'number') return String(v);
  return '';
}

function num(v, def = 0) {
  if (typeof v === 'number') return v;
  if (typeof v === 'string') {
    const n = parseInt(v, 10);
    return Number.isNaN(n) ? def : n;
  }
  return def;
}

export function cleanTitle(title) {
  return title
    .replace(/['’‘`´]/g, '')
    .replace(/[^\p{L}\p{N} ]/gu, ' ')
    .split(/\s+/)
    .join(' ')
    .trim();
}

export function toCompactTitle(title) {
  return title.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, '');
}

function stripTags(html) {
  return html.replace(/<[^>]+>/g, '').trim();
}

// -----------------------------------------------------------------------------
// Quality and Metadata Parsing
// -----------------------------------------------------------------------------
export function parseQualityMeta(name) {
  const lower = name.toLowerCase();

  let quality;
  if (lower.includes('2160p') || lower.includes('4k') || lower.includes('uhd')) quality = '4K';
  else if (lower.includes('1080p') || lower.includes('fhd') || lower.includes('1080i')) quality = '1080p';
  else if (lower.includes('720p') || lower.includes('hdtv')) quality = '720p';
  else if (lower.includes('480p') || lower.includes('576p')) quality = '480p';
  else quality = 'SD';

  const details = [];
  if (lower.includes('x265') || lower.includes('h265') || lower.includes('hevc')) details.push('x265');
  else if (lower.includes('x264') || lower.includes('h264') || lower.includes('avc')) details.push('x264');

  if (lower.includes('7.1') || lower.includes('truehd') || lower.includes('atmos')) details.push('7.1');
  else if (lower.includes('5.1') || lower.includes('dd5') || lower.includes('dts') || lower.includes('ac3'))
    details.push('5.1');

  if (lower.includes('hdr')) details.push('HDR');
  if (lower.includes('dv') || lower.includes('dolby vision') || lower.includes('vision')) details.push('DV');

  if (
    lower.includes('dual') ||
    lower.includes('dual-audio') ||
    lower.includes('multi') ||
    lower.includes('dubbed')
  ) {
    details.push('Dual-Audio');
  }

  return { quality, details };
}

function extractStartYear(year) {
  const m = /\b(19\d{2}|20\d{2})\b/.exec(year);
  return m ? m[1] : null;
}

// -----------------------------------------------------------------------------
// Filename Parser & Matcher
// -----------------------------------------------------------------------------
function isPartOfHint(n, titleHints) {
  for (const hint of titleHints) {
    if (!hint) continue;
    if (new RegExp(`\\b${n}\\b`).test(hint.toLowerCase())) return true;
  }
  return false;
}

export function parseSeasonsEpisodes(title, titleHints) {
  const isValidEpisode = (e) =>
    e !== 1080 &&
    e !== 720 &&
    e !== 2160 &&
    e !== 480 &&
    e !== 576 &&
    e !== 360 &&
    !(e >= 1900 && e <= 2099);

  const seasons = [];
  const episodes = [];
  let isPack = false;
  const titleLower = title.toLowerCase();

  const pushSeason = (s) => {
    if (!seasons.includes(s)) seasons.push(s);
  };
  const pushEpisode = (e) => {
    if (isValidEpisode(e) && !episodes.includes(e)) episodes.push(e);
  };

  // 1. Batch/complete keywords
  if (/complete|batch|pack|season box|seasons|collection/.test(titleLower)) isPack = true;

  // 2. S01E01-E08 / S01E01-08 / S01E01_E08 / S01E01_08 / S01E01-S01E08
  for (const cap of matches(
    titleLower,
    's(\\d+)\\s*e(\\d+)\\s*(?:-|to|~|_)\\s*(?:s\\d+\\s*)?e?(\\d+)\\b'
  )) {
    const s = parseInt(cap[1], 10);
    const e1 = parseInt(cap[2], 10);
    const e2 = parseInt(cap[3], 10);
    if (Number.isNaN(s) || Number.isNaN(e1) || Number.isNaN(e2)) continue;
    pushSeason(s);
    if (e1 < e2 && e2 - e1 < 100) {
      isPack = true;
      for (let e = e1; e <= e2; e++) pushEpisode(e);
    }
  }

  // 3. S01E01 / S01.E01 / S01_E01 / S01-E01 / S01E01E02 (multi-episode)
  for (const cap of matches(titleLower, 's(\\d+)\\s*(?:e|ep|\\.e?p?|[-_](?:e|p)+)\\s*(\\d+)')) {
    const s = parseInt(cap[1], 10);
    const e = parseInt(cap[2], 10);
    if (Number.isNaN(s) || Number.isNaN(e)) continue;
    pushSeason(s);
    pushEpisode(e);
  }

  // S01E01E02E03 chained markers
  for (const full of matches(titleLower, 's(\\d+)(?:\\s*e\\d+){2,}')) {
    const seasonCap = /^s(\d+)/.exec(full[0]);
    if (seasonCap) {
      const season = parseInt(seasonCap[1], 10);
      if (!Number.isNaN(season)) pushSeason(season);
    }
    for (const epCap of matches(full[0], 'e(\\d+)')) {
      const episode = parseInt(epCap[1], 10);
      if (!Number.isNaN(episode)) pushEpisode(episode);
    }
    isPack = true;
  }

  // 4. 1x01-08 or 1x01-1x08
  for (const cap of matches(
    titleLower,
    '\\b(\\d+)\\s*x\\s*(\\d+)\\s*(?:-|to|~|_)\\s*(?:\\d+\\s*x\\s*)?(\\d+)\\b'
  )) {
    const s = parseInt(cap[1], 10);
    const e1 = parseInt(cap[2], 10);
    const e2 = parseInt(cap[3], 10);
    if (Number.isNaN(s) || Number.isNaN(e1) || Number.isNaN(e2)) continue;
    pushSeason(s);
    if (e1 < e2 && e2 - e1 < 100) {
      isPack = true;
      for (let e = e1; e <= e2; e++) pushEpisode(e);
    }
  }

  // 5. 1x01 or 01x02
  for (const cap of matches(titleLower, '\\b(\\d+)\\s*x\\s*(\\d+)\\b')) {
    const s = parseInt(cap[1], 10);
    const e = parseInt(cap[2], 10);
    if (Number.isNaN(s) || Number.isNaN(e)) continue;
    pushSeason(s);
    pushEpisode(e);
  }

  // 6. S01-S03 or Season 1-3 or Season 1 to 3
  for (const cap of matches(
    titleLower,
    '\\bs(?:easons?)?\\s*(\\d+)\\s*(?:-|~|to|_)\\s*s(?:easons?)?\\s*(\\d+)\\b'
  )) {
    const s1 = parseInt(cap[1], 10);
    const s2 = parseInt(cap[2], 10);
    if (Number.isNaN(s1) || Number.isNaN(s2)) continue;
    if (s1 < s2 && s2 - s1 < 50) {
      isPack = true;
      for (let s = s1; s <= s2; s++) pushSeason(s);
    }
  }

  for (const cap of matches(titleLower, '\\bs(?:easons?)?\\s*(\\d+)(?:-|~|to)(\\d+)\\b')) {
    const s1 = parseInt(cap[1], 10);
    const s2 = parseInt(cap[2], 10);
    if (Number.isNaN(s1) || Number.isNaN(s2)) continue;
    if (s1 < s2 && s2 - s1 < 50) {
      isPack = true;
      for (let s = s1; s <= s2; s++) pushSeason(s);
    }
  }

  // 7. Season 1 or S01
  for (const cap of matches(titleLower, '\\b(?:s|season)\\s*(\\d+)\\b')) {
    const s = parseInt(cap[1], 10);
    if (!Number.isNaN(s)) pushSeason(s);
  }

  // 8. 2nd Season or 4th Season
  for (const cap of matches(titleLower, '\\b(\\d+)(?:st|nd|rd|th)\\s+season\\b')) {
    const s = parseInt(cap[1], 10);
    if (!Number.isNaN(s)) pushSeason(s);
  }

  // 9. Ep 01 or Episode 01 or E01
  for (const cap of matches(titleLower, '\\b(?:ep|episode|e)\\s*(\\d+)\\b')) {
    const e = parseInt(cap[1], 10);
    if (!Number.isNaN(e)) pushEpisode(e);
  }

  // 10. Bare episode span after season marker, e.g. "Season 01 1-12 Complete"
  if (isPack || seasons.length > 0) {
    for (const cap of matches(titleLower, '(?:^|[^a-z0-9])(\\d+)\\s*(?:-|to|~)\\s*(\\d+)\\b')) {
      const group1Start = cap.index + cap[0].indexOf(cap[1]);
      if (titleLower.slice(0, group1Start).trimEnd().endsWith('season')) continue;
      const start = parseInt(cap[1], 10);
      const end = parseInt(cap[2], 10);
      if (Number.isNaN(start) || Number.isNaN(end)) continue;
      if (start > 0 && start <= end && end - start < 100 && isValidEpisode(start) && isValidEpisode(end)) {
        for (let episode = start; episode <= end; episode++) pushEpisode(episode);
        isPack = true;
      }
    }
  }

  // 11. Standalone episode number fallback (excluding digits belonging to
  // season, year or resolution)
  let epClean = titleLower;
  epClean = epClean.replace(new RegExp('\\bs(?:easons?)?\\s*\\d+\\s*(?:-|~|to|_)\\s*s(?:easons?)?\\s*\\d+\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\bs(?:easons?)?\\s*\\d+(?:-|~|to)\\d+\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\b(?:s|season)\\s*\\d+\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\b\\d+(?:st|nd|rd|th)\\s+season\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\b(19\\d{2}|20\\d{2})\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\b(2160p|1080p|720p|480p|576p|360p|4k|8k|1080i)\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\b(?:x|h)?26[45]\\b|\\bhevc\\b|\\bav1\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\d\\.\\d', 'g'), ' ');
  epClean = epClean.replace(new RegExp('\\b\\d+bits?\\b', 'g'), ' ');
  epClean = epClean.replace(new RegExp('v\\d+\\b', 'g'), ' ');

  if (episodes.length === 0) {
    for (const cap of matches(epClean, '(?:^|\\-\\s*|\\[)(\\d+)(?:\\b|\\])')) {
      const n = parseInt(cap[1], 10);
      if (Number.isNaN(n)) continue;
      if (!(n > 0 && n < 10000 && isValidEpisode(n))) continue;
      if (isPartOfHint(n, titleHints)) continue;
      pushEpisode(n);
    }
  }

  if (seasons.length > 0 && episodes.length === 0) isPack = true;

  return [seasons, episodes, isPack];
}

export function parseFilename(filename, titleHints) {
  const lower = filename.toLowerCase();
  const [seasons, episodes, isPack] = parseSeasonsEpisodes(filename, titleHints);

  let year = null;
  for (const cap of matches(lower, '\\b(19\\d{2}|20\\d{2})\\b')) {
    const y = parseInt(cap[1], 10);
    if (y !== 1080 && y !== 720 && y !== 2160 && y !== 480 && y !== 576) {
      year = y;
      break;
    }
  }

  let resolution = null;
  const resTags = [
    ['2160p', '4K'],
    ['4k', '4K'],
    ['uhd', '4K'],
    ['1080p', '1080p'],
    ['fhd', '1080p'],
    ['1080i', '1080p'],
    ['720p', '720p'],
    ['hd', '720p'],
    ['480p', '480p'],
    ['sd', 'SD'],
    ['576p', '576p'],
  ];
  for (const [tag, label] of resTags) {
    if (lower.includes(tag)) {
      resolution = label;
      break;
    }
  }

  const splitKeywords = [
    '2160p', '1080p', '720p', '480p', '576p', '360p', 'bluray', 'blu-ray', 'webdl', 'web-dl',
    'webrip', 'hdtv', 'x264', 'x265', 'h264', 'h265', 'hevc', '10bit', '8bit', 'complete',
    'batch', 'pack', 'season', 'episode', 'multi', 'dual',
  ];

  let splitIdx = filename.length;

  const findEarliest = (source) => {
    const m = new RegExp(source).exec(lower);
    if (m && m.index < splitIdx) splitIdx = m.index;
  };

  findEarliest('\\bs\\d+');
  findEarliest('\\b\\d+x\\d+');
  findEarliest('\\bseason\\b');
  findEarliest('\\bepisode\\b');
  findEarliest('\\bep\\d+');
  findEarliest('\\be\\d+');
  findEarliest('\\b\\d+v\\d+\\b');

  const yearMatch = /\b(19\d{2}|20\d{2})\b/.exec(lower);
  if (yearMatch) {
    const yVal = parseInt(yearMatch[0], 10);
    if (yVal !== 1080 && yVal !== 720 && yVal !== 2160 && yearMatch.index < splitIdx) {
      splitIdx = yearMatch.index;
    }
  }

  for (const kw of splitKeywords) {
    const m = new RegExp(`\\b${kw}\\b`).exec(lower);
    if (m && m.index < splitIdx) splitIdx = m.index;
  }

  const numberRe = new RegExp('(?:\\-\\s*|\\[|\\b)(\\d+)(?:\\b|\\])', 'g');
  let m;
  while ((m = numberRe.exec(lower)) !== null) {
    const n = parseInt(m[1], 10);
    if (
      n !== 1080 && n !== 720 && n !== 2160 && n !== 480 && n !== 360 && n !== 576 &&
      !(n >= 1900 && n <= 2099) && n > 0 && n < 10000
    ) {
      if (isPartOfHint(n, titleHints)) continue;
      const before = lower.slice(0, m.index);
      const isSeason =
        before.endsWith('s') || before.endsWith('s ') || before.endsWith('season') || before.endsWith('season ');
      if (!isSeason && m.index < splitIdx) splitIdx = m.index;
    }
  }

  let cleanedPrefix = filename.slice(0, splitIdx).trim();
  while (cleanedPrefix.startsWith('[')) {
    const endIdx = cleanedPrefix.indexOf(']');
    if (endIdx === -1) break;
    cleanedPrefix = cleanedPrefix.slice(endIdx + 1).trim();
  }

  const baseTitle = cleanedPrefix
    .replace(/^[^\p{L}\p{N}]+|[^\p{L}\p{N}]+$/gu, '')
    .replace(/\./g, ' ')
    .replace(/_/g, ' ')
    .trim();

  return { baseTitle, seasons, episodes, year, resolution, isPack };
}

const ALLOWED_EXTRA_WORDS = [
  'season', 'seasons', 'series', 'complete', 'pack', 'boxset', 'box', 'set', 'collection',
  'anthology', 'volume', 'vol', 'part', 'pt', 'book', 'chapters', 'chapter', 'saga', 'arc',
  'cour', 'show', 'tv', 'movie', 'film', 'ova', 'ona', 'special', 'specials', 'bonus',
  'extras', 'extra', 'recap', 'trailer', 'teaser', 'episode', 'episodes', 'edition',
  'versions', 'version', 'cut', 'uncut', 'extended', 'remastered', 'restored', 'unrated',
  'rated', 'censored', 'uncensored', 'directors', 'director', 'imax', 'widescreen',
  'fullscreen', 'theatrical', 'live', 'action', 'animated', 'cartoon', '3d', '2d', '4k',
  'uhd', 'hd', 'sd', 'hdtv', 'classic', 'ultimate', 'remix', 'original', 'digital',
  'copy', 'remaster', 'retail', 'english', 'eng', 'japanese', 'jap', 'jp', 'sub', 'subs',
  'subbed', 'subtitled', 'dub', 'dubs', 'dubbed', 'multi', 'multisubs', 'dual', 'audio',
  'bilingual', 'lat', 'latin', 'esp', 'espanol', 'spanish', 'fra', 'french', 'ger',
  'german', 'ita', 'italian', 'rus', 'russian', 'kor', 'korean', 'chi', 'chinese',
  'mandarin', 'cantonese', 'taiwanese', 'viet', 'vietnamese', 'thai', 'hindi', 'tamil',
  'telugu', 'rip', 'webrip', 'web', 'webdl', 'dl', 'bluray', 'brrip', 'bdrip', 'dvd',
  'dvdrip', 'tvrip', 'pdtv', 'dsr', 'sdtv', 'ldtv', 'h264', 'h265', 'x264', 'x265',
  'hevc', 'av1', 'mpeg', 'divx', 'xvid', 'mp4', 'mkv', 'avi', 'blu', 'ray', 'br', 'bd',
  'tv', 'aac', 'aac2', 'aac5', 'ac3', 'dd5', 'ddp5', 'ddp7', 'dts', 'dtshd', 'truehd',
  'atmos', 'flac', 'mp3', 'soundtrack', 'ost', 'music', 'songs', '8bit', '10bit', '12bit',
  'hi10p', 'hi10', 'yts', 'tgx', 'galaxyrg', 'qxr', 'vyto', 'rarbg', 'ettv', 'eztv',
  'psa', 'meghd', 'megapack', 'megustas', 'megusta', 'ion10', 'fgt', 'screener', 'scr',
  'cam', 'telecined', 'tc', 'ts', 'workprint', 'wp', 'hdr', 'hdr10', 'hdr10plus', 'dv',
  'dolby', 'vision', 'hlg', 'sdr', 'v2', 'v3', 'v4', 'repack', 'proper', 'real',
  'readnfo', 'nfo', 'internal',
];

function isAllowedExtraWord(w) {
  if (w.length <= 1) return true;
  if (/^[0-9]+$/.test(w)) return true;
  if (/(?:st|nd|rd|th)$/.test(w)) {
    const prefix = w.slice(0, -2);
    if (prefix !== '' && /^[0-9]+$/.test(prefix)) return true;
  }
  if (['i', 'ii', 'iii', 'iv', 'v', 'vi', 'vii', 'viii', 'ix', 'x'].includes(w)) return true;
  return ALLOWED_EXTRA_WORDS.includes(w);
}

export function isTitleMatch(torrentBase, metaTitle) {
  const compactTorrent = toCompactTitle(torrentBase);
  const compactMeta = toCompactTitle(metaTitle);
  if (compactTorrent === compactMeta) return true;

  if (torrentBase.includes(':') && metaTitle.toLowerCase().startsWith(torrentBase.toLowerCase())) {
    return true;
  }

  const getTokens = (t) =>
    new Set(
      t
        .toLowerCase()
        .split(/[^\p{L}\p{N}]+/u)
        .filter((w) => w && !['the', 'and', 'for', 'with', 'of', 'in', 'to', 'a', 'an', 'or'].includes(w))
    );

  const wTorrent = getTokens(torrentBase);
  if (wTorrent.size === 0) return false;

  for (const v of [metaTitle]) {
    const wVar = getTokens(v);
    if (wVar.size === 0) continue;

    let isSubset = true;
    for (const tok of wVar) {
      if (!wTorrent.has(tok)) {
        isSubset = false;
        break;
      }
    }
    if (!isSubset) continue;

    const extraWords = [...wTorrent].filter((t) => !wVar.has(t));
    if (extraWords.every((w) => isAllowedExtraWord(w))) return true;
  }

  return false;
}

export function verifyTorrentMatch(torrentTitle, metaTitle, romajiTitle, metaYear, targetSeason, targetEpisode) {
  return verifyTorrentMatchWithAbsolute(
    torrentTitle,
    metaTitle,
    romajiTitle,
    metaYear,
    targetSeason,
    targetEpisode,
    null
  );
}

export function verifyTorrentMatchWithAbsolute(
  torrentTitle,
  metaTitle,
  romajiTitle,
  metaYear,
  targetSeason,
  targetEpisode,
  absoluteEpisode
) {
  const hints = [metaTitle];
  if (romajiTitle) hints.push(romajiTitle);
  const parsed = parseFilename(torrentTitle, hints);

  // 1. Year Match
  if (metaYear != null) {
    const yearMatch = /^\d+/.exec(metaYear);
    if (yearMatch) {
      const my = parseInt(yearMatch[0], 10);
      if (!Number.isNaN(my) && parsed.year != null && Math.abs(my - parsed.year) > 1) {
        return false;
      }
    }
  }

  // 2. Title Match (try English and Romaji)
  let matched = isTitleMatch(parsed.baseTitle, metaTitle);
  if (!matched && romajiTitle) matched = isTitleMatch(parsed.baseTitle, romajiTitle);
  if (!matched) return false;

  // A matching base title is not enough for a movie result
  if (targetSeason == null) {
    const tokenize = (value) =>
      new Set(value.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter((t) => t));
    const torrentTokens = tokenize(torrentTitle);
    const titleTokens = tokenize(metaTitle);
    if (romajiTitle) {
      for (const t of tokenize(romajiTitle)) titleTokens.add(t);
    }
    const nonMovieKeywords = [
      'soundtrack', 'ost', 'trailer', 'teaser', 'commentary', 'sample', 'featurette',
      'interview', 'bloopers', 'outtakes', 'extras', 'bonus', 'collection', 'anthology',
      'season', 'episode', 'series',
    ];
    const hasBadKeyword = nonMovieKeywords.some(
      (keyword) => torrentTokens.has(keyword) && !titleTokens.has(keyword)
    );
    if (hasBadKeyword || parsed.seasons.length > 0) return false;
  }

  // 3. Series Season / Episode Match
  if (targetSeason != null) {
    if (targetSeason > 0) {
      const lowerTitle = torrentTitle.toLowerCase();
      const lowerMeta = metaTitle.toLowerCase();
      const lowerRomaji = romajiTitle ? romajiTitle.toLowerCase() : null;

      const getCleanTokens = (t) => new Set(t.split(/[^\p{L}\p{N}]+/u).filter((w) => w));
      const metaTokens = getCleanTokens(lowerMeta);
      const romajiTokens = lowerRomaji ? getCleanTokens(lowerRomaji) : new Set();
      const torrentTokens = getCleanTokens(lowerTitle);

      const ignoreKeywords = [
        'ova', 'ona', 'special', 'specials', 'movie', 'film', 'recap', 'teaser', 'trailer',
        'bonus', 'extra', 'extras', 'nced', 'ncop', 'ost', 'soundtrack', 'preview',
        'interview', 'commentary',
      ];

      for (const kw of ignoreKeywords) {
        if (
          parsed.isPack &&
          ['special', 'specials', 'bonus', 'extra', 'extras', 'ova', 'ona', 'commentary'].includes(kw)
        ) {
          continue;
        }
        if (torrentTokens.has(kw) && !metaTokens.has(kw) && !romajiTokens.has(kw)) {
          return false;
        }
      }
    }

    if (parsed.seasons.length > 0 && !parsed.seasons.includes(targetSeason)) {
      return false;
    }

    if (targetSeason > 1 && parsed.seasons.length === 0 && !parsed.isPack) {
      return false;
    }

    if (targetEpisode != null) {
      const matchesEpisode =
        parsed.episodes.includes(targetEpisode) ||
        (absoluteEpisode != null && parsed.episodes.includes(absoluteEpisode));

      if (parsed.isPack) {
        if (parsed.episodes.length > 0 && !matchesEpisode) return false;
      } else if (parsed.episodes.length === 0 || !matchesEpisode) {
        return false;
      }
    }
  }

  return true;
}

export function extractTorrentTitle(streamTitle) {
  const firstLine = streamTitle.split('\n')[0] || '';
  let title = firstLine;
  for (const prefix of TITLE_PREFIXES) {
    if (title.startsWith(prefix)) {
      title = title.slice(prefix.length);
      break;
    }
  }
  return title;
}

export function extractSeeds(title) {
  const idx = title.indexOf('👥 ');
  if (idx !== -1) {
    const sub = title.slice(idx + 3);
    const spaceIdx = sub.indexOf(' ');
    if (spaceIdx === -1) return 0;
    const n = parseInt(sub.slice(0, spaceIdx), 10);
    return Number.isNaN(n) ? 0 : n;
  }
  return 0;
}

function mergeStream(streams, incoming) {
  if (!incoming.infoHash) {
    streams.push(incoming);
    return;
  }

  const existing = streams.find((s) => s.infoHash === incoming.infoHash);
  if (!existing) {
    streams.push(incoming);
    return;
  }

  const mergedSources = existing.sources || [];
  for (const source of incoming.sources || []) {
    if (!mergedSources.includes(source)) mergedSources.push(source);
  }

  if (extractSeeds(incoming.title) > extractSeeds(existing.title)) {
    existing.name = incoming.name;
    existing.title = incoming.title;
  }
  existing.sources = mergedSources;
}

// -----------------------------------------------------------------------------
// XML RSS Parser Helper
// -----------------------------------------------------------------------------
function extractXmlTag(xml, tagName) {
  const rx = new RegExp(
    `<[a-zA-Z0-9_\\-]+:?${tagName}(?:\\s+[^>]*?)?>([\\s\\S]*?)</[a-zA-Z0-9_\\-]+:?${tagName}>`,
    'i'
  );
  const m = rx.exec(xml);
  return m ? m[1].trim() : null;
}

// -----------------------------------------------------------------------------
// Individual Scrapers
// -----------------------------------------------------------------------------
function makeStream(name, title, infoHash, fileIdx, sources, behaviorHints) {
  const s = { name, title };
  if (infoHash != null) s.infoHash = infoHash;
  if (fileIdx != null) s.fileIdx = fileIdx;
  if (sources) s.sources = sources;
  if (behaviorHints) s.behaviorHints = behaviorHints;
  return s;
}

// 1. YTS Movie Scraper
async function scrapeSingleYts(url) {
  const streams = [];
  const json = await httpJson(url, { requireOk: false });
  if (json && json.status === 'ok' && json.data) {
    for (const movie of json.data.movies || []) {
      for (const torrent of movie.torrents || []) {
        const qmeta = parseQualityMeta(torrent.quality);
        const detailStr = qmeta.details.length ? ` | ${qmeta.details.join(' | ')}` : '';
        const displayQuality = `${qmeta.quality} (${String(torrent.type || '').toUpperCase()})`;
        const peersInfo =
          (torrent.seeds ?? 0) === 0
            ? '👥 Active (YTS Swarm)'
            : `👥 ${torrent.seeds} seeders | 📥 ${torrent.peers ?? 0} peers`;
        const hash = normalizeInfoHash(torrent.hash);
        if (!isValidInfoHash(hash)) continue;
        streams.push(
          makeStream(
            `[Bitlab] ${displayQuality}${detailStr}`,
            `🎬 YTS: ${movie.title}\n📦 ${torrent.size}\n${peersInfo}\n⚡ Direct P2P Torrent Stream`,
            hash,
            null,
            getSourcesForTorrent(hash)
          )
        );
      }
    }
  }
  return streams;
}

async function scrapeYtsMovies(imdbId) {
  const urls = [
    `https://movies-api.accel.li/api/v2/list_movies.json?query_term=${imdbId}`,
    `https://yts.mx/api/v2/list_movies.json?query_term=${imdbId}`,
  ];
  const results = await Promise.allSettled(urls.map((u) => scrapeSingleYts(u)));
  const all = [];
  for (const r of results) {
    if (r.status === 'fulfilled') {
      for (const s of r.value) mergeStream(all, s);
    }
  }
  return all;
}

// 2. APIBay Scraper
export async function scrapeApibay(query, providerLabel) {
  const streams = [];
  const text = await httpText(
    `https://apibay.org/q.php?q=${encodeURIComponent(query)}&cat=200`,
    { requireOk: false }
  );
  if (!text) return streams;

  let torrents;
  try {
    torrents = JSON.parse(text);
  } catch {
    return streams;
  }
  if (!Array.isArray(torrents)) return streams;

  for (const torrent of torrents) {
    const name = str(torrent.name);
    const infoHash = str(torrent.info_hash);
    if (!isApibayResultValid(name, infoHash)) continue;

    const hash = normalizeInfoHash(infoHash);
    const qmeta = parseQualityMeta(name);
    const detailStr = qmeta.details.length ? ` | ${qmeta.details.join(' | ')}` : '';
    const sizeFormatted = formatSize(str(torrent.size));
    const seeds = parseInt(str(torrent.seeders), 10) || 0;
    const peers = parseInt(str(torrent.leechers), 10) || 0;

    streams.push(
      makeStream(
        `[Bitlab] ${qmeta.quality}${detailStr}`,
        `🎬 ${providerLabel}: ${name}\n📦 ${sizeFormatted}\n👥 ${seeds} seeders | 📥 ${peers} peers\n⚡ Direct P2P Torrent Stream`,
        hash,
        null,
        getSourcesForTorrent(hash)
      )
    );
  }

  return streams;
}

// 3. TPB HTML Scraper (Fallback for APIBay)
async function scrapeSingleTpb(url, providerLabel) {
  const streams = [];
  const html = await httpText(url, { requireOk: false });
  if (!html) return streams;

  const rows = html.split(/<tr[^>]*>/i).slice(1);
  for (const row of rows) {
    const magnetMatch = /<a[^>]+href="(magnet:\?[^"]+)"[^>]*>/i.exec(row);
    if (!magnetMatch) continue;

    const infoHash = extractHashFromMagnet(magnetMatch[1]);
    if (!infoHash || !isValidInfoHash(infoHash)) continue;

    const tds = [];
    const tdRe = /<td[^>]*>([\s\S]*?)<\/td>/gi;
    let td;
    while ((td = tdRe.exec(row)) !== null) tds.push(td[1]);

    const nameLink = tds[1] ? /<a[^>]*>([\s\S]*?)<\/a>/i.exec(tds[1]) : null;
    const name = nameLink ? decodeHtmlEntities(stripTags(nameLink[1])) : 'Unknown Torrent';
    const size = tds[4]
      ? stripTags(tds[4]).replace(/&nbsp;/g, ' ').replace(/\u00a0/g, ' ').trim()
      : 'Unknown size';
    const seeds = parseInt(stripTags(tds[5] || '').trim(), 10) || 0;
    const leechers = parseInt(stripTags(tds[6] || '').trim(), 10) || 0;

    const qmeta = parseQualityMeta(name);
    const detailStr = qmeta.details.length ? ` | ${qmeta.details.join(' | ')}` : '';
    const sources = extractTrackersFromMagnet(magnetMatch[1], infoHash);

    streams.push(
      makeStream(
        `[Bitlab] ${qmeta.quality}${detailStr}`,
        `🎬 ${providerLabel}: ${name}\n📦 ${size}\n👥 ${seeds} seeders | 📥 ${leechers} peers\n⚡ Direct P2P Torrent Stream`,
        infoHash,
        null,
        sources
      )
    );
  }

  return streams;
}

async function scrapeTpbHtml(query, providerLabel) {
  const encoded = encodeURIComponent(query);
  const urls = [
    `https://tpb.party/search/${encoded}/1/99/200`,
    `https://thepiratebay10.org/search/${encoded}/1/99/200`,
    `https://thepiratebay0.org/search/${encoded}/1/99/200`,
  ];

  const primary = await scrapeSingleTpb(urls[0], providerLabel);
  if (primary.length) return primary;

  const results = await Promise.allSettled(urls.slice(1).map((u) => scrapeSingleTpb(u, providerLabel)));
  for (const r of results) {
    if (r.status === 'fulfilled' && r.value.length) return r.value;
  }
  return [];
}

// 4. Bitsearch Scraper
const BITSEARCH_CATEGORIES = { movie: 2, series: 3, anime: 4 };

export async function scrapeBitsearch(query, providerLabel, category) {
  const streams = [];
  const catId = BITSEARCH_CATEGORIES[category];
  const json = await httpJson(
    `https://bitsearch.to/api/v1/search?q=${encodeURIComponent(query)}&category=${catId}&limit=50&sort=seeders`
  );
  if (!json) return streams;

  for (const t of json.results || []) {
    const title = str(t.title);
    const infohash = str(t.infohash);
    if (!title || !infohash) continue;

    const hash = normalizeInfoHash(infohash);
    if (!isValidInfoHash(hash)) continue;

    const qmeta = parseQualityMeta(title);
    const detailStr = qmeta.details.length ? ` | ${qmeta.details.join(' | ')}` : '';
    const sizeFormatted = formatSize(str(t.size));
    const seeds = num(t.seeders);
    const peers = num(t.leechers);

    streams.push(
      makeStream(
        `[Bitlab] ${qmeta.quality}${detailStr}`,
        `🎬 ${providerLabel}: ${title}\n📦 ${sizeFormatted}\n👥 ${seeds} seeders | 📥 ${peers} peers\n⚡ Direct P2P Torrent Stream`,
        hash,
        null,
        getSourcesForTorrent(hash)
      )
    );
  }

  return streams;
}

// 5. SolidTorrents Scraper
async function scrapeSingleSolidtorrent(url, providerLabel) {
  const json = await httpJson(url);
  if (!json) return [];

  const streams = [];
  for (const t of json.results || []) {
    const title = str(t.title);
    const magnet = str(t.magnet);
    if (!title || !magnet) continue;

    const infoHash = extractHashFromMagnet(magnet);
    if (!infoHash || !isValidInfoHash(infoHash)) continue;

    const sizeFormatted = formatSize(str(t.size));
    const swarm = t.swarm || {};
    const seeds = num(swarm.seeders);
    const leechers = num(swarm.leechers);

    const qmeta = parseQualityMeta(title);
    const detailStr = qmeta.details.length ? ` | ${qmeta.details.join(' | ')}` : '';
    const sources = extractTrackersFromMagnet(magnet, infoHash);

    streams.push(
      makeStream(
        `[Bitlab] ${qmeta.quality}${detailStr}`,
        `🎬 ${providerLabel}: ${title}\n📦 ${sizeFormatted}\n👥 ${seeds} seeders | 📥 ${leechers} peers\n⚡ Direct P2P Torrent Stream`,
        infoHash,
        null,
        sources
      )
    );
  }

  return streams;
}

async function scrapeSolidtorrents(query, providerLabel) {
  const encoded = encodeURIComponent(query);
  const urls = [
    `https://solidtorrents.to/api/v1/search?q=${encoded}&category=video&sort=seeders`,
    `https://solidtorrents.net/api/v1/search?q=${encoded}&category=video&sort=seeders`,
  ];
  for (const url of urls) {
    const streams = await scrapeSingleSolidtorrent(url, providerLabel);
    if (streams.length) return streams;
  }
  return [];
}

// 6. Nyaa Anime Scraper
async function scrapeSingleNyaa(url) {
  const xml = await httpText(url);
  if (!xml) return [];

  const streams = [];
  const items = xml.split('<item>').slice(1);
  for (const itemXml of items) {
    const itemContent = itemXml.split('</item>')[0] || '';
    const rawTitle = extractXmlTag(itemContent, 'title') || '';
    const title = decodeHtmlEntities(rawTitle);
    const hashRaw = extractXmlTag(itemContent, 'infoHash') || '';
    const size = extractXmlTag(itemContent, 'size') || '';
    const seeders = parseInt(extractXmlTag(itemContent, 'seeders') || '', 10) || 0;
    const leechers = parseInt(extractXmlTag(itemContent, 'leechers') || '', 10) || 0;

    if (!hashRaw || !title) continue;

    const hash = normalizeInfoHash(hashRaw);
    if (!isValidInfoHash(hash)) continue;

    const qmeta = parseQualityMeta(title);
    const detailStr = qmeta.details.length ? ` | ${qmeta.details.join(' | ')}` : '';
    const sources = getSourcesForTorrent(hash);

    streams.push(
      makeStream(
        `[Bitlab] ${qmeta.quality}${detailStr}`,
        `🌸 Nyaa: ${title}\n📦 ${size}\n👥 ${seeders} seeders | 📥 ${leechers} peers\n⚡ Direct P2P Torrent Stream`,
        hash,
        null,
        sources
      )
    );
  }

  return streams;
}

async function scrapeNyaa(query) {
  const encoded = encodeURIComponent(query);
  const urls = [
    `https://nyaa.si/?page=rss&c=1_2&q=${encoded}`,
    `https://nyaa.land/?page=rss&c=1_2&q=${encoded}`,
  ];
  for (const url of urls) {
    const streams = await scrapeSingleNyaa(url);
    if (streams.length) return streams;
  }
  return [];
}

// 7. EZTV Series Scraper
async function scrapeSingleEztv(domain, imdbId, targetSeason, targetEpisode) {
  const streams = [];
  const cleanImdbId = imdbId.startsWith('tt') ? imdbId.slice(2) : imdbId;
  const text = await httpText(
    `${domain}/api/get-torrents?imdb_id=${cleanImdbId}&limit=50&page=1`,
    { requireOk: false }
  );
  if (!text) return streams;

  let json;
  try {
    json = JSON.parse(text);
  } catch {
    return streams;
  }
  const torrents = Array.isArray(json && json.torrents) ? json.torrents : [];

  for (const item of torrents) {
    const hash = str(item.hash);
    const title = str(item.title);
    if (!hash || !title) continue;

    const season = parseInt(str(item.season), 10) || 0;
    const episode = parseInt(str(item.episode), 10) || 0;
    if (season !== targetSeason || episode !== targetEpisode) continue;

    const sizeFormatted = formatSize(str(item.size_bytes));
    const qmeta = parseQualityMeta(title);
    const detailStr = qmeta.details.length ? ` | ${qmeta.details.join(' | ')}` : '';

    const normalized = normalizeInfoHash(hash);
    if (!isValidInfoHash(normalized)) continue;
    const sources = getSourcesForTorrent(normalized);

    streams.push(
      makeStream(
        `[Bitlab] ${qmeta.quality}${detailStr}`,
        `📺 EZTV: ${title}\n📦 ${sizeFormatted}\n👥 ${num(item.seeds)} seeders | 📥 ${num(item.peers)} peers\n⚡ Direct P2P Torrent Stream`,
        normalized,
        null,
        sources
      )
    );
  }

  return streams;
}

async function scrapeEztv(imdbId, targetSeason, targetEpisode) {
  const primary = await scrapeSingleEztv('https://eztvx.to', imdbId, targetSeason, targetEpisode);
  if (primary.length) return primary;

  const domains = ['https://eztv.re', 'https://eztv.yt', 'https://eztv.ag', 'https://eztv.tf', 'https://eztv.wf'];
  const results = await Promise.allSettled(
    domains.map((d) => scrapeSingleEztv(d, imdbId, targetSeason, targetEpisode))
  );
  for (const r of results) {
    if (r.status === 'fulfilled' && r.value.length) return r.value;
  }
  return [];
}

// -----------------------------------------------------------------------------
// Backup Metadata Providers
// -----------------------------------------------------------------------------
function parseKitsuMeta(json) {
  const attributes = json && json.data && json.data.attributes;
  if (!attributes) return null;
  const titles = attributes.titles || {};
  const name =
    (typeof titles.en_us === 'string' && titles.en_us) ||
    (typeof titles.en === 'string' && titles.en) ||
    (typeof attributes.canonicalTitle === 'string' && attributes.canonicalTitle) ||
    null;
  if (!name || !name.trim()) return null;
  const nameStr = name.trim();

  let year = null;
  if (typeof attributes.startDate === 'string' && attributes.startDate) {
    const y = attributes.startDate.split('-')[0];
    if (/^\d{4}$/.test(y)) year = y;
  }

  let romaji = null;
  if (typeof titles.en_jp === 'string' && titles.en_jp.trim() && cleanTitle(titles.en_jp) !== cleanTitle(nameStr)) {
    romaji = titles.en_jp.trim();
  }

  return [nameStr, year, romaji];
}

async function fetchKitsuMeta(kitsuId) {
  const json = await httpJson(`https://kitsu.io/api/edge/anime/${kitsuId}`, {
    timeoutMs: 1500,
    headers: { Accept: 'application/vnd.api+json' },
  });
  if (!json) return null;
  return parseKitsuMeta(json);
}

export async function fetchMetaCached(type, imdbId) {
  const cacheKey = `meta:${type}:${imdbId}`;
  const cached = await cacheGet(cacheKey);
  if (cached) return cached;

  if (imdbId.startsWith('kitsu:')) {
    const kitsuId = imdbId.slice('kitsu:'.length);
    const res = await fetchKitsuMeta(kitsuId);
    if (res) {
      const meta = { name: res[0], year: res[1] };
      await cachePut(cacheKey, meta, 86400);
      return meta;
    }
    return null;
  }

  // 1. Cinemeta (Main Stremio Metadata Provider)
  const cinemeta = await httpJson(`https://v3-cinemeta.strem.io/meta/${type}/${imdbId}.json`, {
    timeoutMs: 1500,
    requireOk: false,
  });
  if (cinemeta && cinemeta.meta && typeof cinemeta.meta.name === 'string') {
    const meta = {
      name: cinemeta.meta.name,
      year: cinemeta.meta.year != null ? String(cinemeta.meta.year) : null,
    };
    await cachePut(cacheKey, meta, 86400);
    return meta;
  }

  // 2. TVmaze fallback (series only)
  if (type === 'series') {
    const show = await httpJson(`https://api.tvmaze.com/lookup/shows?imdb=${imdbId}`, {
      timeoutMs: 1500,
    });
    if (show && typeof show.name === 'string') {
      const year = show.premiered ? show.premiered.split('-')[0] : null;
      const meta = { name: show.name, year };
      await cachePut(cacheKey, meta, 86400);
      return meta;
    }
  }

  // 3. Community TMDb Stremio Addon fallback
  const tmdb = await httpJson(
    `https://94c8cb97ae04-tmdb-addon.baby-beamup.club/meta/${type}/${imdbId}.json`,
    { timeoutMs: 1500 }
  );
  if (tmdb && tmdb.meta && typeof tmdb.meta.name === 'string') {
    const date = tmdb.meta.release_date || tmdb.meta.first_air_date;
    const year = date ? date.split('-')[0] : null;
    const meta = { name: tmdb.meta.name, year };
    await cachePut(cacheKey, meta, 86400);
    return meta;
  }

  return null;
}

async function checkIfAnimeAndGetRomaji(englishTitle, targetYear) {
  const json = await httpJson(
    `https://kitsu.io/api/edge/anime?filter[text]=${encodeURIComponent(englishTitle)}`,
    {
      timeoutMs: 1500,
      headers: { Accept: 'application/vnd.api+json', 'Content-Type': 'application/vnd.api+json' },
    }
  );
  if (!json || !Array.isArray(json.data)) return [false, null];

  const target = englishTitle.toLowerCase();
  const cleanTarget = cleanTitle(target);
  for (const item of json.data.slice(0, 3)) {
    const attributes = item && item.attributes;
    if (!attributes) continue;

    let isMatch = false;
    let romajiTitle = null;

    if (typeof attributes.canonicalTitle === 'string' && cleanTitle(attributes.canonicalTitle.toLowerCase()) === cleanTarget) {
      isMatch = true;
    }
    const titles = attributes.titles || {};
    if (typeof titles.en === 'string' && cleanTitle(titles.en.toLowerCase()) === cleanTarget) {
      isMatch = true;
    }
    if (typeof titles.en_us === 'string' && cleanTitle(titles.en_us.toLowerCase()) === cleanTarget) {
      isMatch = true;
    }
    if (typeof titles.en_jp === 'string') {
      romajiTitle = titles.en_jp;
      if (cleanTitle(titles.en_jp.toLowerCase()) === cleanTarget) {
        isMatch = true;
      }
    }

    if (isMatch && targetYear != null) {
      const yearMatch = /^\d+/.exec(targetYear);
      if (yearMatch) {
        const tYear = parseInt(yearMatch[0], 10);
        if (!Number.isNaN(tYear) && typeof attributes.startDate === 'string') {
          const kYear = parseInt(attributes.startDate.split('-')[0], 10);
          if (!Number.isNaN(kYear) && Math.abs(tYear - kYear) > 2) {
            isMatch = false;
          }
        }
      }
    }

    if (isMatch) {
      if (romajiTitle && cleanTitle(romajiTitle.toLowerCase()) !== cleanTarget) {
        return [true, romajiTitle];
      }
      return [true, null];
    }
  }

  return [false, null];
}

// -----------------------------------------------------------------------------
// Auxiliary caches (in-isolate memory, TTL-based)
// -----------------------------------------------------------------------------
const AUX_SUCCESS_TTL = 6 * 3600 * 1000;
const AUX_FAILURE_TTL = 5 * 60 * 1000;
const AUX_CACHE_MAX_ITEMS = 5000;
const auxCache = new Map();

function auxGet(key) {
  const entry = auxCache.get(key);
  if (!entry) return undefined;
  if (Date.now() - entry.at >= entry.ttl) {
    auxCache.delete(key);
    return undefined;
  }
  return entry.value;
}

function auxSet(key, value, ttl) {
  if (auxCache.size >= AUX_CACHE_MAX_ITEMS) auxCache.clear();
  auxCache.set(key, { value, at: Date.now(), ttl });
}

async function checkIfAnimeAndGetRomajiCached(englishTitle, targetYear) {
  const cacheKey = `${englishTitle.toLowerCase()}:${targetYear}`;
  const cached = auxGet(cacheKey);
  if (cached !== undefined) return cached;
  const res = await checkIfAnimeAndGetRomaji(englishTitle, targetYear);
  auxSet(cacheKey, res, res[0] ? AUX_SUCCESS_TTL : AUX_FAILURE_TTL);
  return res;
}

async function fetchAnizipAbsoluteEpisode(imdbOrKitsu, episode) {
  const url = imdbOrKitsu.startsWith('kitsu:')
    ? `https://api.ani.zip/mappings?kitsu_id=${imdbOrKitsu.slice('kitsu:'.length)}`
    : `https://api.ani.zip/mappings?imdb_id=${imdbOrKitsu}`;
  const json = await httpJson(url, { timeoutMs: 3000, headers: { 'User-Agent': 'Mozilla/5.0' } });
  if (!json || !json.episodes) return null;
  const ep = json.episodes[String(episode)];
  if (ep && typeof ep.absoluteEpisodeNumber === 'number') return ep.absoluteEpisodeNumber;
  return null;
}

async function fetchAnizipAbsoluteEpisodeCached(imdbOrKitsu, episode) {
  const cacheKey = `${imdbOrKitsu}:${episode}`;
  const cached = auxGet(cacheKey);
  if (cached !== undefined) return cached;
  const res = await fetchAnizipAbsoluteEpisode(imdbOrKitsu, episode);
  auxSet(cacheKey, res, res != null ? AUX_SUCCESS_TTL : AUX_FAILURE_TTL);
  return res;
}

// -----------------------------------------------------------------------------
// Task loop helper (mirrors the Rust JoinSet + deadline pattern)
// -----------------------------------------------------------------------------
async function runTaskLoop(pending, deadline, onResult) {
  while (pending.length > 0 && Date.now() < deadline) {
    const resolved = await Promise.race(
      pending.map((p) =>
        p.then(
          (v) => ({ p, v }),
          () => ({ p, v: null })
        )
      )
    );
    const idx = pending.indexOf(resolved.p);
    if (idx !== -1) pending.splice(idx, 1);
    onResult(resolved.v);
  }
}

// -----------------------------------------------------------------------------
// Movie Stream Resolution
// -----------------------------------------------------------------------------
export async function getMovieStreams(imdbId) {
  const cacheKey = `streams:movie:${imdbId}`;
  const cached = await cacheGet(cacheKey);
  if (cached) return cached;

  const deadline = Date.now() + 6000;
  const pending = [];
  const allStreams = [];
  let resolvedShowName = null;
  let resolvedRomajiName = null;
  let resolvedYear = null;
  let metaResolved = false;

  const streamsResult = (s) => ({ kind: 'streams', streams: s });

  // 1. Spawn ID-based searches immediately
  pending.push(scrapeYtsMovies(imdbId).then(streamsResult));
  pending.push(scrapeApibay(imdbId, 'APIBay').then(streamsResult));

  // 2. Fetch metadata and check if anime
  const metaTask = (async () => {
    const meta = await fetchMetaCached('movie', imdbId);
    if (!meta) return { kind: 'meta', meta: null };
    const [isAnime, romaji] = await checkIfAnimeAndGetRomajiCached(meta.name, meta.year);
    return { kind: 'meta', meta: { name: meta.name, year: meta.year, isAnime, romaji } };
  })();
  pending.push(metaTask);

  await runTaskLoop(pending, deadline, (res) => {
    if (!res) return;
    if (res.kind === 'streams') {
      for (const s of res.streams) {
        if (
          resolvedShowName &&
          !verifyTorrentMatch(
            extractTorrentTitle(s.title),
            resolvedShowName,
            resolvedRomajiName,
            resolvedYear,
            null,
            null
          )
        ) {
          continue;
        }
        mergeStream(allStreams, s);
      }
    } else if (res.kind === 'meta') {
      if (metaResolved) return;
      metaResolved = true;
      const m = res.meta;
      if (!m) return;
      resolvedShowName = m.name;
      resolvedYear = m.year;
      resolvedRomajiName = m.romaji;

      const queries = [m.name];
      if (m.romaji) queries.push(m.romaji);

      for (const q of queries) {
        const cleanedQ = cleanTitle(q);
        let queryWithYear = cleanedQ;
        if (m.year) {
          const startYr = extractStartYear(m.year);
          if (startYr) queryWithYear = `${cleanedQ} ${startYr}`;
        }

        pending.push(scrapeSolidtorrents(queryWithYear, 'SolidTorrents').then(streamsResult));
        pending.push(scrapeApibay(queryWithYear, 'APIBay').then(streamsResult));
        pending.push(scrapeTpbHtml(queryWithYear, 'TPB').then(streamsResult));
        pending.push(scrapeBitsearch(queryWithYear, 'Bitsearch', 'movie').then(streamsResult));
        if (m.isAnime) {
          pending.push(scrapeNyaa(queryWithYear).then(streamsResult));
          pending.push(scrapeNyaa(cleanedQ).then(streamsResult));
        }
      }
    }
  });

  // Re-run movie validation after all tasks so completion order cannot bypass
  // title/content checks.
  if (resolvedShowName) {
    for (let i = allStreams.length - 1; i >= 0; i--) {
      if (
        !verifyTorrentMatch(
          extractTorrentTitle(allStreams[i].title),
          resolvedShowName,
          resolvedRomajiName,
          resolvedYear,
          null,
          null
        )
      ) {
        allStreams.splice(i, 1);
      }
    }
  } else {
    // Without metadata, retain only YTS's IMDb-bound response
    for (let i = allStreams.length - 1; i >= 0; i--) {
      const firstLine = allStreams[i].title.split('\n')[0] || '';
      if (!firstLine.includes('YTS: ')) allStreams.splice(i, 1);
    }
  }

  allStreams.sort((a, b) => extractSeeds(b.title) - extractSeeds(a.title));

  if (metaResolved && allStreams.length > 0) {
    await cachePut(cacheKey, allStreams, STREAM_CACHE_TTL_SECS);
  }

  return allStreams;
}

// -----------------------------------------------------------------------------
// Series Stream Resolution
// -----------------------------------------------------------------------------
export async function getSeriesStreams(imdbId, season, episode) {
  const cacheKey = `streams:series:${imdbId}:${season}:${episode}`;
  const cached = await cacheGet(cacheKey);
  if (cached) return cached;

  const deadline = Date.now() + 6000;
  const pending = [];
  const allStreams = [];
  let resolvedShowName = null;
  let resolvedRomajiName = null;
  let resolvedYear = null;
  let resolvedAbsoluteEpisode = null;
  let resolvedIsAnime = imdbId.startsWith('kitsu:');
  let metaResolved = false;

  const streamsResult = (s) => ({ kind: 'streams', streams: s });

  // 1. Spawn EZTV ID search immediately for actual IMDb IDs
  if (imdbId.startsWith('tt')) {
    pending.push(scrapeEztv(imdbId, season, episode).then(streamsResult));
  }

  // 2. Fetch metadata, check anime & absolute episode
  const metaTask = (async () => {
    if (imdbId.startsWith('kitsu:')) {
      const [metaRes, absoluteEpisode] = await Promise.all([
        fetchKitsuMeta(imdbId.slice('kitsu:'.length)),
        fetchAnizipAbsoluteEpisodeCached(imdbId, episode),
      ]);
      if (metaRes) {
        await cachePut(`meta:series:${imdbId}`, { name: metaRes[0], year: metaRes[1] }, 86400);
        return {
          kind: 'meta',
          meta: { name: metaRes[0], year: metaRes[1], isAnime: true, romaji: metaRes[2], absoluteEpisode },
        };
      }
      return { kind: 'meta', meta: null };
    }

    const meta = await fetchMetaCached('series', imdbId);
    if (!meta) return { kind: 'meta', meta: null };

    const anizipPromise = fetchAnizipAbsoluteEpisodeCached(imdbId, episode);
    const [detectedAnime, romaji] = await checkIfAnimeAndGetRomajiCached(meta.name, meta.year);
    const absoluteEpisode = detectedAnime ? await anizipPromise : null;
    return {
      kind: 'meta',
      meta: { name: meta.name, year: meta.year, isAnime: detectedAnime, romaji, absoluteEpisode },
    };
  })();
  pending.push(metaTask);

  await runTaskLoop(pending, deadline, (res) => {
    if (!res) return;
    if (res.kind === 'streams') {
      for (const s of res.streams) {
        if (
          resolvedShowName &&
          !verifyTorrentMatchWithAbsolute(
            extractTorrentTitle(s.title),
            resolvedShowName,
            resolvedRomajiName,
            resolvedYear,
            season,
            episode,
            resolvedAbsoluteEpisode
          )
        ) {
          continue;
        }
        mergeStream(allStreams, s);
      }
    } else if (res.kind === 'meta') {
      if (metaResolved) return;
      metaResolved = true;
      const m = res.meta;
      if (!m) return;
      resolvedShowName = m.name;
      resolvedYear = m.year;
      resolvedRomajiName = m.romaji;
      resolvedAbsoluteEpisode = m.absoluteEpisode;
      resolvedIsAnime = m.isAnime;

      const queries = [m.name];
      if (m.romaji) queries.push(m.romaji);

      for (const q of queries) {
        const cleanedQ = cleanTitle(q);
        const sTag = `S${String(season).padStart(2, '0')}`;
        const queryExact = `${cleanedQ} ${sTag}E${String(episode).padStart(2, '0')}`;
        const querySeason = `${cleanedQ} ${sTag}`;
        const queryBare = cleanedQ;
        const searchQueries = [queryExact, querySeason, queryBare];
        if (m.year) {
          const startYr = extractStartYear(m.year);
          if (startYr) searchQueries.push(`${cleanedQ} ${startYr}`);
        }

        for (const sq of searchQueries) {
          pending.push(scrapeSolidtorrents(sq, 'SolidTorrents').then(streamsResult));
          pending.push(scrapeApibay(sq, 'APIBay').then(streamsResult));
          pending.push(scrapeTpbHtml(sq, 'TPB').then(streamsResult));
          if (sq.includes(` ${sTag}`)) {
            pending.push(
              scrapeBitsearch(sq, 'Bitsearch', m.isAnime ? 'anime' : 'series').then(streamsResult)
            );
          }
        }

        if (m.isAnime) {
          const nyaaQueries = [queryExact, querySeason];
          if (m.absoluteEpisode != null) {
            nyaaQueries.push(`${cleanedQ} ${String(m.absoluteEpisode).padStart(2, '0')}`);
          } else {
            nyaaQueries.push(`${cleanedQ} ${String(episode).padStart(2, '0')}`);
          }
          for (const nq of nyaaQueries) {
            pending.push(scrapeNyaa(nq).then(streamsResult));
          }
        }
      }
    }
  });

  allStreams.sort((a, b) => extractSeeds(b.title) - extractSeeds(a.title));

  let absoluteEpisode = resolvedAbsoluteEpisode;
  if (absoluteEpisode == null && resolvedIsAnime) {
    absoluteEpisode = await fetchAnizipAbsoluteEpisodeCached(imdbId, episode);
  }

  await resolveFileIndices(allStreams, season, episode, absoluteEpisode, resolvedShowName);

  // Final filter
  for (let i = allStreams.length - 1; i >= 0; i--) {
    const s = allStreams[i];
    const torrentTitle = extractTorrentTitle(s.title);

    if (
      resolvedShowName &&
      !verifyTorrentMatchWithAbsolute(
        torrentTitle,
        resolvedShowName,
        resolvedRomajiName,
        resolvedYear,
        season,
        episode,
        absoluteEpisode
      )
    ) {
      allStreams.splice(i, 1);
      continue;
    }

    if (s.fileIdx != null) continue;

    const hints = [];
    if (resolvedShowName) hints.push(resolvedShowName);
    if (resolvedRomajiName) hints.push(resolvedRomajiName);
    const parsed = parseFilename(torrentTitle, hints);

    // Unresolvable packs are dropped
    if (parsed.isPack) {
      allStreams.splice(i, 1);
      continue;
    }

    if (parsed.episodes.length > 0) {
      const matchesRelative = parsed.episodes.includes(episode);
      const matchesAbsolute = absoluteEpisode != null && parsed.episodes.includes(absoluteEpisode);
      if (!matchesRelative && !matchesAbsolute) {
        allStreams.splice(i, 1);
        continue;
      }
    }

    if (parsed.seasons.length > 0 && !parsed.seasons.includes(season)) {
      allStreams.splice(i, 1);
    }
  }

  if (metaResolved && allStreams.length > 0) {
    await cachePut(cacheKey, allStreams, STREAM_CACHE_TTL_SECS);
  }

  return allStreams;
}

// -----------------------------------------------------------------------------
// Torrent File Index Resolver & Torrent File Parser
// -----------------------------------------------------------------------------
export function parseTorboxTorrentFiles(body, expectedInfoHash) {
  let response;
  try {
    response = JSON.parse(body);
  } catch {
    return null;
  }
  const data = response && response.data;
  if (!response.success || !data) return null;
  if (normalizeInfoHash(data.hash) !== normalizeInfoHash(expectedInfoHash)) return null;

  const files = (data.files || [])
    .filter((file) => String(file.name).trim() !== '')
    .map((file) => ({ path: String(file.name), size: Number(file.size) || 0, index: Number(file.id) || 0 }));
  return files.length > 0 ? files : null;
}

function parseBencode(data) {
  let pos = 0;
  const decoder = new TextDecoder();

  function read() {
    if (pos >= data.length) return undefined;
    const c = data[pos];
    if (c === 0x69) {
      // 'i'
      pos++;
      const start = pos;
      while (pos < data.length && data[pos] !== 0x65) pos++;
      if (pos >= data.length) return undefined;
      const s = decoder.decode(data.subarray(start, pos));
      pos++;
      if (!/^-?\d+$/.test(s)) return undefined;
      return parseInt(s, 10);
    }
    if (c === 0x6c) {
      // 'l'
      pos++;
      const list = [];
      while (pos < data.length && data[pos] !== 0x65) {
        const v = read();
        if (v === undefined) return undefined;
        list.push(v);
      }
      if (pos >= data.length) return undefined;
      pos++;
      return list;
    }
    if (c === 0x64) {
      // 'd'
      pos++;
      const dict = new Map();
      while (pos < data.length && data[pos] !== 0x65) {
        const key = read();
        if (!(key instanceof Uint8Array)) return undefined;
        const val = read();
        if (val === undefined) return undefined;
        dict.set(decoder.decode(key), val);
      }
      if (pos >= data.length) return undefined;
      pos++;
      return dict;
    }
    if (c >= 0x30 && c <= 0x39) {
      // '0'-'9'
      const start = pos;
      while (pos < data.length && data[pos] !== 0x3a) pos++;
      if (pos >= data.length) return undefined;
      const lenStr = decoder.decode(data.subarray(start, pos));
      const len = /^\d+$/.test(lenStr) ? parseInt(lenStr, 10) : NaN;
      if (Number.isNaN(len)) return undefined;
      pos++;
      if (pos + len > data.length) return undefined;
      const bytes = data.subarray(pos, pos + len);
      pos += len;
      return bytes;
    }
    return undefined;
  }

  const root = read();
  return root instanceof Map ? root : undefined;
}

export function parseTorrentBytes(bytes) {
  const root = parseBencode(bytes);
  if (!(root instanceof Map)) return null;
  const info = root.get('info');
  if (!(info instanceof Map)) return null;

  const filesList = [];
  const files = info.get('files');
  if (files instanceof Array) {
    for (let idx = 0; idx < files.length; idx++) {
      const fileDict = files[idx];
      if (!(fileDict instanceof Map)) continue;
      const lengthVal = fileDict.get('length');
      const length = typeof lengthVal === 'number' ? lengthVal : 0;
      const pathVal = fileDict.get('path');
      if (!(pathVal instanceof Array)) continue;
      const pathParts = [];
      for (const part of pathVal) {
        if (part instanceof Uint8Array) {
          pathParts.push(new TextDecoder().decode(part));
        }
      }
      if (pathParts.length > 0) {
        filesList.push({ path: pathParts.join('/'), size: length, index: idx });
      }
    }
  } else {
    const nameBytes = info.get('name');
    if (!(nameBytes instanceof Uint8Array)) return null;
    const name = new TextDecoder().decode(nameBytes);
    const lengthVal = info.get('length');
    const length = typeof lengthVal === 'number' ? lengthVal : 0;
    filesList.push({ path: name, size: length, index: 0 });
  }

  return filesList.length > 0 ? filesList : null;
}

async function fetchTorboxFiles(infoHash) {
  const res = await httpGet(
    `https://api.torbox.app/v1/api/torrents/torrentinfo?hash=${encodeURIComponent(infoHash)}&timeout=2&use_cache_lookup=true`,
    { timeoutMs: 2500, headers: { Accept: 'application/json' } }
  );
  if (!res || !res.ok) return null;
  const text = await res.text();
  return parseTorboxTorrentFiles(text, infoHash);
}

async function fetchTorrentFile(url) {
  const res = await httpGet(url, { timeoutMs: 3000, headers: { 'User-Agent': UA } });
  if (!res || !res.ok) return null;
  const ct = res.headers.get('content-type') || '';
  if (ct.includes('html')) return null;
  const buf = new Uint8Array(await res.arrayBuffer());
  return parseTorrentBytes(buf);
}

async function fetchTorrentFilesList(infoHash) {
  const hashUpper = infoHash.toUpperCase();
  const urls = [
    `https://itorrents.net/torrent/${hashUpper}.torrent`,
    `https://itorrents.org/torrent/${hashUpper}.torrent`,
    `https://torrage.info/torrent.php?h=${hashUpper}`,
    `https://btcache.me/torrent/${hashUpper}`,
  ];

  const results = await Promise.allSettled([
    fetchTorboxFiles(infoHash),
    ...urls.map((url) => fetchTorrentFile(url)),
  ]);

  for (const r of results) {
    if (r.status === 'fulfilled' && r.value) return r.value;
  }
  return null;
}

async function fetchTorrentFilesListCached(infoHash) {
  const cacheKey = `torrentfiles:${infoHash}`;
  const cached = await cacheGet(cacheKey);
  if (cached) return cached;
  const files = await fetchTorrentFilesList(infoHash);
  if (files && files.length) await cachePut(cacheKey, files, 86400);
  return files;
}

export function parseSeasonFromPath(path) {
  const lower = path.toLowerCase();
  const components = lower.split(/[\\/]/);
  if (components.length > 1) {
    for (let i = components.length - 2; i >= 0; i--) {
      const folder = components[i];
      let m = /\bs(?:eason)?\s*(\d+)\b/.exec(folder);
      if (m) return parseInt(m[1], 10);
      m = /\b(\d+)(?:st|nd|rd|th)\s+season\b/.exec(folder);
      if (m) return parseInt(m[1], 10);
      m = /^\s*(\d+)\s*$/.exec(folder);
      if (m) {
        const s = parseInt(m[1], 10);
        if (s > 0 && s < 100) return s;
      }
    }
  }
  return null;
}

export function isFileMatchWithAbsolute(filePath, targetSeason, targetEpisode, absoluteEpisode, showName) {
  const lowerPath = filePath.toLowerCase();
  const isVideo = ['.mkv', '.mp4', '.avi', '.mov', '.wmv', '.flv', '.webm', '.m4v', '.mpg', '.mpeg'].some(
    (ext) => lowerPath.endsWith(ext)
  );
  if (!isVideo) return false;

  if (lowerPath.includes('sample')) return false;

  if (targetSeason > 0) {
    const ignoreKeywords = [
      'nced', 'ncop', 'ost', 'soundtrack', 'bonus', 'extras', 'extra', 'special', 'ova',
      'preview', 'trailer', 'recap', 'interview', 'commentary', 'featurette', 'making of',
      'behind the scenes', 'bloopers', 'gag reel', 'deleted scene', 'outtakes',
    ];
    for (const kw of ignoreKeywords) {
      if (lowerPath.includes(kw)) return false;
    }
  }

  const filename = lowerPath.split(/[\\/]/).pop() || lowerPath;
  const hints = showName ? [showName] : [];
  const [filenameSeasons, episodes] = parseSeasonsEpisodes(filename, hints);

  if (episodes.includes(targetEpisode) || (absoluteEpisode != null && episodes.includes(absoluteEpisode))) {
    let season = filenameSeasons.length > 0 ? filenameSeasons[0] : null;
    if (season == null) season = parseSeasonFromPath(lowerPath);
    if (season != null) return season === targetSeason;
    return targetSeason === 1;
  }
  return false;
}

function runWithConcurrency(jobs, limit) {
  let nextIdx = 0;
  const workers = [];
  const count = Math.min(limit, jobs.length);
  for (let w = 0; w < count; w++) {
    workers.push(
      (async () => {
        while (nextIdx < jobs.length) {
          const idx = nextIdx++;
          await jobs[idx]();
        }
      })()
    );
  }
  return Promise.all(workers);
}

async function resolveFileIndices(streams, season, episode, absoluteEpisode, showName) {
  const deadline = Date.now() + 2000;
  const jobs = [];
  const n = Math.min(streams.length, 15);

  for (let idx = 0; idx < n; idx++) {
    const stream = streams[idx];
    const torrentTitle = extractTorrentTitle(stream.title);
    const hints = showName ? [showName] : [];
    const parsed = parseFilename(torrentTitle, hints);
    const isMultiFile = parsed.isPack || parsed.episodes.length > 1;
    if (!isMultiFile) continue;
    if (!stream.infoHash) continue;

    const hash = stream.infoHash;
    jobs.push(async () => {
      if (Date.now() >= deadline) return;
      const files = await fetchTorrentFilesListCached(hash);
      if (Date.now() >= deadline || !files) return;
      for (const file of files) {
        if (isFileMatchWithAbsolute(file.path, season, episode, absoluteEpisode, showName)) {
          stream.fileIdx = file.index;
          stream.behaviorHints = { bingeGroup: `bitlab|${hash}`, filename: file.path };
          break;
        }
      }
    });
  }

  await runWithConcurrency(jobs, 4);
}
