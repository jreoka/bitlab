import { test } from 'node:test';
import assert from 'node:assert/strict';

const store = new Map();
globalThis.caches = {
  default: {
    async match(url) {
      return store.get(String(url)) || undefined;
    },
    async put(url, res) {
      store.set(String(url), res);
    },
  },
};

const { default: worker } = await import('../src/index.js');

async function call(path, env = {}) {
  return worker.fetch(new Request(`https://bitlab.test${path}`), env);
}

test('manifest.json returns the addon manifest', async () => {
  const res = await call('/manifest.json');
  assert.equal(res.status, 200);
  assert.equal(res.headers.get('content-type'), 'application/json');
  const manifest = await res.json();
  assert.equal(manifest.id, 'org.bitlab.stremio');
  assert.equal(manifest.name, 'Bitlab');
  assert.deepEqual(manifest.resources, ['stream']);
  assert.deepEqual(manifest.types, ['movie', 'series']);
  assert.deepEqual(manifest.idPrefixes, ['tt', 'kitsu']);
});

test('landing page contains manifest url for the deployed origin', async () => {
  const res = await call('/');
  assert.equal(res.status, 200);
  const html = await res.text();
  assert.ok(html.includes('https://bitlab.test/manifest.json'));
  assert.ok(html.includes('Bitlab'));
});

test('favicon.svg is served as svg', async () => {
  const res = await call('/favicon.svg');
  assert.equal(res.status, 200);
  assert.equal(res.headers.get('content-type'), 'image/svg+xml');
});

test('unknown routes 404 with CORS header', async () => {
  const res = await call('/nope');
  assert.equal(res.status, 404);
  assert.equal(res.headers.get('access-control-allow-origin'), '*');
});

test('stream route with non-vpn check returns empty streams for bad movie id', async () => {
  const res = await call('/stream/movie/tt123.json', { REQUIRE_VPN: 'false' });
  assert.equal(res.status, 200);
  const body = await res.json();
  assert.deepEqual(body, { streams: [] });
});

test('stream route returns vpn-required stream when vpn check blocked', async () => {
  const res = await call('/stream/movie/tt0111161.json', { REQUIRE_VPN: 'true' });
  assert.equal(res.status, 200);
  const body = await res.json();
  assert.equal(body.streams.length, 1);
  assert.equal(body.streams[0].name, 'VPN Required');
  assert.equal(body.streams[0].url, 'https://bitlab.test/vpn-required.mp4');
});

test('ALLOWED_URL rejects mismatched hosts', async () => {
  const res = await call('/manifest.json', { ALLOWED_URL: 'https://other.example.workers.dev' });
  assert.equal(res.status, 403);
});

test('ALLOWED_URL accepts matching hosts', async () => {
  const res = await call('/manifest.json', { ALLOWED_URL: 'https://bitlab.test' });
  assert.equal(res.status, 200);
});

test('REQUIRE_VPN=false disables the vpn gate', async () => {
  const res = await call('/stream/series/tt0944947:1:1.json', { REQUIRE_VPN: 'false' });
  assert.equal(res.status, 200);
  const body = await res.json();
  assert.ok(Array.isArray(body.streams), 'expected a streams array');
});
