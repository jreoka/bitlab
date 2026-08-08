import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  normalizeInfoHash,
  extractHashFromMagnet,
  isValidInfoHash,
  formatSize,
  decodeHtmlEntities,
  parseQualityMeta,
  parseSeasonsEpisodes,
  parseFilename,
  cleanTitle,
  toCompactTitle,
  isTitleMatch,
  verifyTorrentMatch,
  verifyTorrentMatchWithAbsolute,
  extractTorrentTitle,
  extractSeeds,
  parseSeasonFromPath,
  parseTorrentBytes,
  parseTorboxTorrentFiles,
  isFileMatchWithAbsolute,
} from '../src/scraper.js';
import { isPrivateOrLoopback, clientIp } from '../src/vpn.js';

// -----------------------------------------------------------------------------
// Info hashes
// -----------------------------------------------------------------------------
test('normalizeInfoHash lowercases hex', () => {
  assert.equal(
    normalizeInfoHash('1588987DB4C7D98F74FB436AD8FEDE1CBE9F1F63'),
    '1588987db4c7d98f74fb436ad8fede1cbe9f1f63'
  );
});

test('normalizeInfoHash converts base32 to hex', () => {
  assert.equal(
    normalizeInfoHash('WRN7ZT6NKMA6SSXYKAFRUGDDIFJUNKI2'),
    'b45bfccfcd5301e94af8500b1a1863415346a91a'
  );
  assert.equal(
    normalizeInfoHash('WRN7ZT6NKMA6SSXYKAFRUGDDIFJUNKI2==='),
    'b45bfccfcd5301e94af8500b1a1863415346a91a'
  );
});

test('extractHashFromMagnet handles hex and base32', () => {
  assert.equal(
    extractHashFromMagnet('magnet:?xt=urn:btih:1588987db4c7d98f74fb436ad8fede1cbe9f1f63&dn=Test'),
    '1588987db4c7d98f74fb436ad8fede1cbe9f1f63'
  );
  assert.equal(
    extractHashFromMagnet('magnet:?xt=urn:btih:WRN7ZT6NKMA6SSXYKAFRUGDDIFJUNKI2&dn=Test'),
    'b45bfccfcd5301e94af8500b1a1863415346a91a'
  );
});

test('isValidInfoHash rejects zeros and junk', () => {
  assert.equal(isValidInfoHash('4fbfc100705fed2fc483da3e11d1b4bc5ba97264'), true);
  assert.equal(isValidInfoHash('0000000000000000000000000000000000000000'), false);
  assert.equal(isValidInfoHash('not-a-real-info-hash'), false);
});

// -----------------------------------------------------------------------------
// Formatting helpers
// -----------------------------------------------------------------------------
test('formatSize', () => {
  assert.equal(formatSize('2080103644'), '1.94 GiB');
  assert.equal(formatSize('1048576'), '1.00 MiB');
  assert.equal(formatSize('500'), '500 B');
  assert.equal(formatSize('invalid'), 'Unknown size');
});

test('decodeHtmlEntities', () => {
  assert.equal(decodeHtmlEntities('Frieren&#39;s Journey'), "Frieren's Journey");
  assert.equal(decodeHtmlEntities('Frieren&#x27;s Journey'), "Frieren's Journey");
  assert.equal(decodeHtmlEntities('Spice &amp; Wolf'), 'Spice & Wolf');
  assert.equal(decodeHtmlEntities('&quot;Gate&quot;'), '"Gate"');
});

test('cleanTitle', () => {
  assert.equal(cleanTitle("Clarkson's Farm"), 'Clarksons Farm');
  assert.equal(cleanTitle("Grey's Anatomy"), 'Greys Anatomy');
  assert.equal(cleanTitle('It’s Always Sunny in Philadelphia'), 'Its Always Sunny in Philadelphia');
  assert.equal(cleanTitle('Spider-Man'), 'Spider Man');
  assert.equal(cleanTitle('S.W.A.T.'), 'S W A T');
  assert.equal(cleanTitle("Marvel's Agents of S.H.I.E.L.D."), 'Marvels Agents of S H I E L D');
  assert.equal(cleanTitle('Doctor Who (2005)'), 'Doctor Who 2005');
});

test('parseQualityMeta', () => {
  assert.equal(parseQualityMeta('Show.720p.WEB-DL').quality, '720p');
  assert.equal(parseQualityMeta('Show.HDTV.x264').quality, '720p');
  assert.equal(parseQualityMeta('TheHDClub.Release').quality, 'SD');
  assert.ok(parseQualityMeta('Movie.TrueHD.Atmos.AC3').details.includes('7.1'));
  assert.equal(parseQualityMeta('Show.2160p.HEVC.HDR.DV').details.includes('x265'), true);
  assert.equal(parseQualityMeta('Show.2160p.HEVC.HDR.DV').quality, '4K');
});

// -----------------------------------------------------------------------------
// Filename parsing
// -----------------------------------------------------------------------------
test('parseSeasonsEpisodes basics', () => {
  assert.deepEqual(parseSeasonsEpisodes('Bocchi the Rock! - S01E01.mkv', []), [[1], [1], false]);
  assert.deepEqual(parseSeasonsEpisodes('02.mp4', []), [[], [2], false]);
  assert.deepEqual(parseSeasonsEpisodes('[SubsPlease] Bocchi the Rock! - 12 (1080p).mkv', []), [[], [12], false]);
  assert.deepEqual(parseSeasonsEpisodes('Bocchi the Rock! - Ep 05.mkv', []), [[], [5], false]);
  assert.deepEqual(parseSeasonsEpisodes('2x03.mkv', []), [[2], [3], false]);
  assert.deepEqual(parseSeasonsEpisodes('Clarksons Farm Season 1 Episode 2.mkv', []), [[1], [2], false]);
  assert.deepEqual(parseSeasonsEpisodes('Season 1 - 02.mkv', []), [[1], [2], false]);
});

test('parseSeasonsEpisodes ranges and packs', () => {
  assert.deepEqual(parseSeasonsEpisodes('Bocchi the Rock! - S01E01-E10.mkv', []), [[1], [1, 2, 3, 4, 5, 6, 7, 8, 9, 10], true]);
  const [seasons, episodes, pack] = parseSeasonsEpisodes('Some Show S01E01E02 1080p', ['Some Show']);
  assert.deepEqual(seasons, [1]);
  assert.deepEqual(episodes, [1, 2]);
  assert.equal(pack, true);
  assert.deepEqual(parseSeasonsEpisodes('[SubsPlease] One Piece - 1050 (1080p).mkv', []), [[], [1050], false]);
  assert.deepEqual(parseSeasonsEpisodes('The.Chosen.S01.1080p.WEBRip.DDP5.1.Atmos.x264', []), [[1], [], true]);
});

test('parseFilename', () => {
  const res = parseFilename('[SubsPlease] Bocchi the Rock! - 12 (1080p).mkv', []);
  assert.equal(res.baseTitle, 'Bocchi the Rock');
  assert.deepEqual(res.episodes, [12]);
  assert.equal(res.resolution, '1080p');

  const res2 = parseFilename("Clarkson's Farm S01 Complete 1080p", []);
  assert.equal(res2.baseTitle, "Clarkson's Farm");
  assert.deepEqual(res2.seasons, [1]);
  assert.equal(res2.isPack, true);

  const res3 = parseFilename('Zoey.101.S01.NTSC.DVDR-P2P', ['Zoey 101']);
  assert.equal(res3.baseTitle, 'Zoey 101');
  assert.deepEqual(res3.seasons, [1]);
  assert.equal(res3.isPack, true);
});

test('parseSeasonFromPath', () => {
  assert.equal(parseSeasonFromPath('Season 2/01.mkv'), 2);
  assert.equal(parseSeasonFromPath('S3/01.mkv'), 3);
  assert.equal(parseSeasonFromPath('2nd Season/01.mkv'), 2);
  assert.equal(parseSeasonFromPath('Bocchi the Rock/01.mkv'), null);
});

// -----------------------------------------------------------------------------
// Title matching
// -----------------------------------------------------------------------------
test('verifyTorrentMatch season and episode', () => {
  const show = 'Re:Zero kara Hajimeru Isekai Seikatsu';
  assert.equal(verifyTorrentMatch('[Erai-raws] Re:Zero kara Hajimeru Isekai Seikatsu - 02 [1080p].mkv', show, null, null, 1, 2), true);
  assert.equal(verifyTorrentMatch('[Erai-raws] Re:Zero kara Hajimeru Isekai Seikatsu - 12 [1080p].mkv', show, null, null, 1, 2), false);
  assert.equal(verifyTorrentMatch('[Erai-raws] Re:Zero S2 - 02 [1080p].mkv', show, null, null, 1, 2), false);
  assert.equal(verifyTorrentMatch('[SubsPlease] Re:Zero S2 Complete [1080p]', show, null, null, 1, 2), false);
});

test('verifyTorrentMatch pack acceptance', () => {
  const show = 'Re:Zero kara Hajimeru Isekai Seikatsu';
  assert.equal(verifyTorrentMatch('[SubsPlease] Re:Zero S1 Complete [1080p]', show, null, null, 1, 2), true);
});

test('verifyTorrentMatch The Chosen vs The Chosen One', () => {
  assert.equal(verifyTorrentMatch('The Chosen One S01E01 1080p', 'The Chosen', null, null, 1, 1), false);
  assert.equal(verifyTorrentMatch('The Chosen S01E01 1080p', 'The Chosen One', null, null, 1, 1), false);
  assert.equal(verifyTorrentMatch('The Chosen S01E01 1080p', 'The Chosen', null, null, 1, 1), true);
});

test('verifyTorrentMatch shows with numbers in name', () => {
  assert.equal(verifyTorrentMatch('Zoey 101 S01E01 1080p', 'Zoey 101', null, null, 1, 1), true);
  assert.equal(verifyTorrentMatch('Zoey.101.S01.NTSC.DVDR-P2P', 'Zoey 101', null, null, 1, 1), true);
  assert.equal(verifyTorrentMatch('Mob Psycho 100 S01E01 1080p', 'Mob Psycho 100', null, null, 1, 1), true);
  assert.equal(verifyTorrentMatch('9-1-1 S01E01 1080p', '9-1-1', null, null, 1, 1), true);
  assert.equal(verifyTorrentMatch('100 Humans S01E01 1080p', '100 Humans', null, null, 1, 1), true);
});

test('verifyTorrentMatch rejects OVA/special for regular seasons', () => {
  const show = 'Re:Zero kara Hajimeru Isekai Seikatsu';
  assert.equal(verifyTorrentMatch('[Erai-raws] Re:Zero kara Hajimeru Isekai Seikatsu - OVA - 02 [1080p].mkv', show, null, null, 1, 2), false);
  assert.equal(verifyTorrentMatch('[Erai-raws] Re:Zero kara Hajimeru Isekai Seikatsu - OVA - 02 [1080p].mkv', show, null, null, 0, 2), true);
});

test('movie extras rejected', () => {
  for (const release of [
    'Dune Soundtrack FLAC',
    'Dune Trailer 1080p',
    'Dune Commentary 1080p',
    'Dune Complete Collection 1080p',
  ]) {
    assert.equal(verifyTorrentMatch(release, 'Dune', null, '2021', null, null), false, release);
  }
});

test('verifyTorrentMatch requires episode or pack evidence', () => {
  assert.equal(verifyTorrentMatch('The Chosen 1080p WEBRip x265', 'The Chosen', null, '2017', 1, 5), false);
});

test('absolute episode is valid matching evidence', () => {
  assert.equal(verifyTorrentMatchWithAbsolute('One Piece - 1050 [1080p]', 'One Piece', null, '1999', 1, 159, 1050), true);
  assert.equal(verifyTorrentMatch('One Piece - 1050 [1080p]', 'One Piece', null, '1999', 1, 159), false);
});

test('1x01 convention accepted for bare query', () => {
  assert.equal(verifyTorrentMatch('Drake and Josh 1x01 HDTV XviD-LOL', 'Drake and Josh', null, '2004', 1, 1), true);
});

test('isTitleMatch rejects truncated franchise titles', () => {
  assert.equal(isTitleMatch('Star Wars', 'Star Wars: The Clone Wars'), false);
  assert.equal(isTitleMatch('Mission Impossible', 'Mission: Impossible - Fallout'), false);
});

test('extractTorrentTitle strips provider prefixes', () => {
  assert.equal(extractTorrentTitle('🌸 Nyaa: Some Show - 02 [1080p]'), 'Some Show - 02 [1080p]');
  assert.equal(extractTorrentTitle('🎬 APIBay: Show'), 'Show');
  assert.equal(extractTorrentTitle('Plain Title'), 'Plain Title');
});

test('extractSeeds', () => {
  assert.equal(extractSeeds('👥 12 seeders | 📥 0 peers'), 12);
  assert.equal(extractSeeds('some text 👥 1234 seeders'), 1234);
  assert.equal(extractSeeds('no emoji here'), 0);
});

// -----------------------------------------------------------------------------
// Torrent file parsing
// -----------------------------------------------------------------------------
const enc = new TextEncoder();

function bs(s) {
  return `${s.length}:${s}`;
}
function bi(n) {
  return `i${n}e`;
}
function bl(items) {
  return `l${items.join('')}e`;
}
function bd(entries) {
  const sorted = [...entries].sort((a, b) => (a[0] < b[0] ? -1 : 1));
  return `d${sorted.map(([k, v]) => bs(k) + v).join('')}e`;
}

test('parseTorrentBytes multi-file', () => {
  const bytes = enc.encode(
    bd([
      ['info', bd([['files', bl([bd([['length', bi(123456)], ['path', bl([bs('Show'), bs('S01E01.mkv')])]])])], ['name', bs('Show')]])],
    ])
  );
  const files = parseTorrentBytes(bytes);
  assert.ok(files);
  assert.equal(files.length, 1);
  assert.equal(files[0].path, 'Show/S01E01.mkv');
  assert.equal(files[0].size, 123456);
  assert.equal(files[0].index, 0);
});

test('parseTorrentBytes single-file', () => {
  const bytes = enc.encode(
    bd([
      ['info', bd([['length', bi(100)], ['name', bs('Show.mkv')]])],
      ['name', bs('Show')],
    ])
  );
  const files = parseTorrentBytes(bytes);
  assert.ok(files);
  assert.equal(files.length, 1);
  assert.equal(files[0].path, 'Show.mkv');
  assert.equal(files[0].size, 100);
  assert.equal(files[0].index, 0);
});

test('parseTorrentBytes rejects garbage', () => {
  assert.equal(parseTorrentBytes(enc.encode('not bencode')), null);
});

test('parseTorboxTorrentFiles preserves ids and validates hash', () => {
  const hash = '8FA30FAFE88B8516A545113E9B732FEE17D4CB06';
  const body = JSON.stringify({
    success: true,
    data: {
      hash: '8fa30fafe88b8516a545113e9b732fee17d4cb06',
      files: [
        { id: 1, name: 'Zoey 101 S01E01 Welcome To PCA.mkv', size: 236847582 },
        { id: 3, name: 'Zoey 101 S01E02 New Roomies.mkv', size: 186201824 },
      ],
    },
  });
  const files = parseTorboxTorrentFiles(body, hash);
  assert.equal(files.length, 2);
  assert.equal(files[1].index, 3);
  assert.equal(files[1].path, 'Zoey 101 S01E02 New Roomies.mkv');
  assert.equal(parseTorboxTorrentFiles(body, '1111111111111111111111111111111111111111'), null);
});

test('isFileMatchWithAbsolute', () => {
  assert.equal(isFileMatchWithAbsolute('Season 01/SpongeBob SquarePants S01E01 Help Wanted.mkv', 1, 1, null, 'SpongeBob SquarePants'), true);
  assert.equal(isFileMatchWithAbsolute('Season 01/S01E01 Commentary by the Cast.mkv', 1, 1, null, 'SpongeBob SquarePants'), false);
  assert.equal(isFileMatchWithAbsolute('Season 01/Sample.mkv', 1, 1, null, 'SpongeBob SquarePants'), false);
  assert.equal(isFileMatchWithAbsolute('Season 01/S01E01.nfo', 1, 1, null, 'SpongeBob SquarePants'), false);
  assert.equal(isFileMatchWithAbsolute('Show Pack/Show - 1050.mkv', 1, 159, 1050, 'Show'), true);
});

// -----------------------------------------------------------------------------
// VPN helpers
// -----------------------------------------------------------------------------
test('isPrivateOrLoopback', () => {
  assert.equal(isPrivateOrLoopback('127.0.0.1'), true);
  assert.equal(isPrivateOrLoopback('192.168.1.10'), true);
  assert.equal(isPrivateOrLoopback('10.0.0.1'), true);
  assert.equal(isPrivateOrLoopback('172.16.4.5'), true);
  assert.equal(isPrivateOrLoopback('::1'), true);
  assert.equal(isPrivateOrLoopback('fc00::1'), true);
  assert.equal(isPrivateOrLoopback('8.8.8.8'), false);
  assert.equal(isPrivateOrLoopback('not-an-ip'), false);
});

test('clientIp header priority', () => {
  const headers = new Headers({ 'x-forwarded-for': '1.2.3.4, 5.6.7.8' });
  assert.equal(clientIp(headers), '1.2.3.4');
  const headers2 = new Headers({ 'x-real-ip': '9.9.9.9' });
  assert.equal(clientIp(headers2), '9.9.9.9');
  assert.equal(clientIp(new Headers()), null);
});

// -----------------------------------------------------------------------------
// Misc ports
// -----------------------------------------------------------------------------
test('toCompactTitle', () => {
  assert.equal(toCompactTitle('Star Wars: The Clone Wars'), 'starwarstheclonewars');
});
