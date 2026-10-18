# Marlin

Self-hosted streaming hub that replaces the **AIOStreams + TorBox** combo with a single binary.
Marlin is a Stremio/Nuvio addon server: it resolves IMDb ids, searches your indexers, runs its own
BitTorrent client, and serves the video over HTTP with full `Range` support and read-ahead
prioritisation — so seeking is instant and time-to-first-frame is a couple of seconds.

No debrid account, no external addon, no transcoding, one process, one directory of state.

```
Stremio / Nuvio
   │  GET /k/<key>/stream/movie/tt0111161.json      → ranked releases
   │  GET /k/<key>/play/<infohash>                  → 206 Partial Content, video/mp4
   ▼
Marlin ──► Cinemeta (title lookup) ──► indexers (knaben / torznab)
   └─────► BitTorrent client ──► local disk cache ──► HTTP byte stream
```

## What you get

- **One addon URL per user.** `https://marlin.example.com/k/<key>/manifest.json`. Keys are created in
  the admin UI and can be revoked, expired, or deleted individually — revoking one leaves every other
  device working, and no key ever sees another key's traffic.
- **Admin UI** at `/admin`: keys with live usage counters, indexer management with a search tester,
  live torrent list (peers, rate, progress), and streaming tunables. Nothing needs a restart.
- **Streaming that behaves like a CDN.** Byte-range requests are mapped onto piece priorities with a
  configurable read-ahead (default 64 MiB), metadata is warmed in the background while you browse the
  stream list, and DHT/PEX/LSD plus a public tracker list are used so the first peer typically answers
  in ~1–2 s.
- **Disk cache with a budget.** Idle torrents are dropped; completed ones are evicted LRU-style once
  the cache exceeds its budget. Sparse-file aware, so the number you see is real disk usage.
- **Quality ranking.** Resolution, source (REMUX/BluRay/WEB), codec, HDR and swarm health are scored;
  CAM/TS rips are pushed to the bottom, and for series the requested `SxxExx` is preferred over
  season packs.
- **Indexers:** `knaben` (public meta-index, works out of the box), `torznab` (Prowlarr, Jackett,
  nZEDb), `apibay`. Add as many as you like; results are merged and deduped by infohash.

## Quick start

### Binary

```bash
go build -o marlin .
MARLIN_ADMIN_PASSWORD=change-me ./marlin
# open http://localhost:8090/admin, create a key, paste the manifest URL into Stremio
```

On first run without `MARLIN_ADMIN_PASSWORD`, a random password is written to `data/ADMIN_PASSWORD`
(mode 0600). A `knaben` indexer is seeded so the addon returns results immediately.

### Docker

```bash
docker compose up -d
docker compose logs -f marlin | grep "admin password"   # if you didn't set one
```

```yaml
services:
  marlin:
    image: ghcr.io/jreoka/marlin:latest
    restart: unless-stopped
    ports: ["8090:8090"]
    environment:
      MARLIN_ADMIN_PASSWORD: change-me
      MARLIN_BASE_URL: https://marlin.example.com
      MARLIN_CACHE_GB: "40"
    volumes: ["./data:/data"]
```

## Configuration

All settings are environment variables; anything set in the admin UI wins over the env default and is
persisted in SQLite.

| Variable | Default | Meaning |
|---|---|---|
| `MARLIN_ADDR` | `:8090` | listen address |
| `MARLIN_DATA` | `./data` | state, cache and SQLite live here |
| `MARLIN_BASE_URL` | derived from request | absolute origin used in stream URLs (set this behind a proxy) |
| `MARLIN_ADMIN_PASSWORD` | generated | admin console password |
| `MARLIN_CACHE_GB` | `40` | disk cache budget; `0` disables eviction |
| `MARLIN_IDLE_MINUTES` | `20` | drop a torrent after this long with no readers |
| `MARLIN_READAHEAD_MB` | `64` | how far ahead of the playhead to prioritise |
| `MARLIN_INFO_TIMEOUT` | `45` | seconds to wait for torrent metadata before answering 504 |
| `MARLIN_TRACKERS` | public list | comma-separated announce URLs added to every magnet |
| `MARLIN_SEED_INDEXER` | `on` | set `off` to start with no indexers |

Behind a reverse proxy, pass `X-Forwarded-Proto`/`X-Forwarded-Host` (or set `MARLIN_BASE_URL`) so the
manifest hands out `https://` links.

## Install

**Stremio** (desktop/mobile/web): Addons → Install from URL → paste
`https://<host>/k/<key>/manifest.json`.

**Nuvio**: Addons → Add repository → paste the same URL.

The landing page at `/` lists every active key's manifest URL, so you can copy it from a phone.

## Admin API

Everything the UI does is a small JSON API under `/admin/api`, session-cookie authenticated:

```
POST   /admin/api/login              {"password":"..."}
GET    /admin/api/overview           keys, indexers, torrents, cache, uptime
POST   /admin/api/keys               {"label","note","days"}
POST   /admin/api/keys/{id}/revoke   {"revoked":true|false}
DELETE /admin/api/keys/{id}
POST   /admin/api/indexers           {id?,kind,name,url,api_key,enabled}
POST   /admin/api/indexers/test?q=   search a single indexer without saving it
GET    /admin/api/search?q=tt0111161&type=movie
POST   /admin/api/settings           {"cache_gb","idle_minutes","readahead_mb"}
DELETE /admin/api/torrents/{infohash}
POST   /admin/api/play               {"magnet":"magnet:?xt=..."} → direct stream URL
```

## Kubernetes

`deploy/marlin.yaml` is a complete manifest (namespace, cache PVC, Deployment,
Service, nginx Ingress with streaming-safe annotations). The binary is pulled
from the pinned GitHub release and checksum-verified in an initContainer, so no
registry credentials are needed. See `deploy/README.md`.

## Operational notes

- **Disk** grows to the cache budget and no further; the cache holds partial files while streaming,
  which is why the budget matters on small volumes.
- **Bandwidth** is the real constraint: a 20 GB remux needs ~20 Mbps sustained. The default read-ahead
  keeps a few seconds buffered, not the whole file.
- **No transcoding.** If the client can't play the codec, pick a different release from the list.
- **Torrent port**: outbound connections are enough to stream; `MARLIN_TORRENT_PORT` pins the listen
  port if you want to accept inbound peers too.
- The admin console is the only authenticated surface. Put it behind your reverse proxy / VPN if the
  host is public, and give each household member their own key so usage is attributable.

## Legal

Marlin is a media server: it indexes, downloads and serves torrents that *you* point it at. Running it
against copyrighted material without permission is illegal in most jurisdictions. The default
`knaben` indexer can be removed (`MARLIN_SEED_INDEXER=off`) or replaced with a private Torznab
endpoint.

## License

MIT
