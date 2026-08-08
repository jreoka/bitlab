import { getMovieStreams, getSeriesStreams } from './scraper.js';
import { clientIp, isVpn } from './vpn.js';

const MANIFEST = {
  id: 'org.bitlab.stremio',
  version: '1.0.0',
  name: 'Bitlab',
  description: 'A high-performance Stremio scraper addon by Bitlab.',
  resources: ['stream'],
  types: ['movie', 'series'],
  catalogs: [],
  idPrefixes: ['tt', 'kitsu'],
};

const NO_CACHE = 'max-age=0, no-cache, no-store, must-revalidate';
const LONG_CACHE = 'public, max-age=86400';

const FAVICON_SVG =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><circle cx="16" cy="16" r="14" fill="black" stroke="white" stroke-width="2"/></svg>';

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const path = url.pathname;

    if (env.ALLOWED_URL && !hostAllowed(url, env.ALLOWED_URL)) {
      return new Response('Forbidden', { status: 403 });
    }

    let response;

    if (request.method === 'GET' && path === '/') {
      response = landingResponse(url.origin);
    } else if (request.method === 'GET' && path === '/manifest.json') {
      response = jsonResponse(MANIFEST, NO_CACHE);
    } else if (request.method === 'GET' && (path === '/favicon.ico' || path === '/favicon.svg')) {
      response = new Response(FAVICON_SVG, {
        headers: { 'Content-Type': 'image/svg+xml', 'Cache-Control': LONG_CACHE },
      });
    } else if (request.method === 'GET' && path.startsWith('/stream/')) {
      const parts = path.split('/').filter(Boolean);
      if (parts.length >= 3) {
        response = await streamHandler(parts[1], parts[2], request, env, url.origin);
      } else {
        response = jsonResponse({ streams: [] }, NO_CACHE);
      }
    }

    if (!response) {
      response = new Response('Not Found', { status: 404 });
    }
    response.headers.set('Access-Control-Allow-Origin', '*');
    return response;
  },
};

async function streamHandler(type, id, request, env, origin) {
  let cleanId;
  try {
    cleanId = decodeURIComponent(id);
  } catch {
    cleanId = id;
  }
  if (cleanId.endsWith('.json')) cleanId = cleanId.slice(0, -'.json'.length);

  if (vpnRequired(env)) {
    const ip = clientIp(request.headers) || 'unknown';
    if (!(await isVpn(ip))) {
      return jsonResponse(
        {
          streams: [
            {
              name: 'VPN Required',
              title: '⚠️ Please enable your VPN\nBitlab requires an active VPN connection to stream.',
              url: `${origin}/vpn-required.mp4`,
              behaviorHints: { filename: 'vpn-required.mp4' },
            },
          ],
        },
        NO_CACHE
      );
    }
  }

  let streams = [];
  if (type === 'movie') {
    if (isValidImdbId(cleanId)) {
      streams = await getMovieStreams(cleanId);
    }
  } else if (type === 'series') {
    const parts = cleanId.split(':');
    if (parts.length === 3) {
      if (parts[0] === 'kitsu') {
        const kitsuId = parseInt(parts[1], 10);
        const episode = parseInt(parts[2], 10);
        if (!Number.isNaN(kitsuId) && !Number.isNaN(episode) && kitsuId > 0 && episode > 0) {
          streams = await getSeriesStreams(`kitsu:${kitsuId}`, 1, episode);
        }
      } else {
        const season = parseInt(parts[1], 10);
        const episode = parseInt(parts[2], 10);
        if (!Number.isNaN(season) && !Number.isNaN(episode) && isValidImdbId(parts[0]) && episode > 0) {
          streams = await getSeriesStreams(parts[0], season, episode);
        }
      }
    }
  }

  return jsonResponse({ streams }, NO_CACHE);
}

function vpnRequired(env) {
  const v = env.REQUIRE_VPN;
  if (v === undefined || v === null || v === '') return true;
  return !['0', 'false', 'no', 'off'].includes(String(v).toLowerCase());
}

function isValidImdbId(id) {
  if (!id.startsWith('tt')) return false;
  const digits = id.slice(2);
  return digits.length >= 7 && /^[0-9]+$/.test(digits);
}

function hostAllowed(url, allowedUrl) {
  let allowed;
  try {
    allowed = new URL(allowedUrl);
  } catch {
    return false;
  }
  const normalize = (u) => {
    let port = u.port;
    if ((u.protocol === 'http:' && port === '80') || (u.protocol === 'https:' && port === '443')) {
      port = '';
    }
    return `${u.protocol}//${u.hostname.toLowerCase()}${port ? `:${port}` : ''}`;
  };
  return normalize(url) === normalize(allowed);
}

function jsonResponse(obj, cacheControl) {
  return new Response(JSON.stringify(obj), {
    headers: { 'Content-Type': 'application/json', 'Cache-Control': cacheControl || NO_CACHE },
  });
}

function landingResponse(origin) {
  const manifestUrl = `${origin}/manifest.json`;
  return new Response(
    `<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Bitlab</title>
    <link rel="icon" type="image/svg+xml" href="/favicon.svg">
    <link href="https://fonts.googleapis.com/css2?family=Outfit:wght@400;600;800&display=swap" rel="stylesheet">
    <style>
        body {
            font-family: 'Outfit', sans-serif;
            background-color: #000000;
            color: #ffffff;
            box-sizing: border-box;
            min-height: 100dvh;
            display: flex;
            align-items: center;
            justify-content: center;
            padding: 24px;
            margin: 0;
        }
        .container {
            max-width: 480px;
            width: 100%;
            display: flex;
            flex-direction: column;
            gap: 4px;
        }
        h1 {
            font-size: 2.2rem;
            font-weight: 800;
            letter-spacing: -0.03em;
            margin: 0;
        }
        .url-box {
            background: #090909;
            border: 1px solid #1a1a1a;
            border-radius: 12px;
            padding: 8px 8px 8px 16px;
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 12px;
        }
        .url-text {
            font-family: monospace;
            font-size: 0.85rem;
            color: #a1a1aa;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
            user-select: all;
        }
        .btn-copy {
            background: #ffffff;
            color: #000000;
            border: none;
            padding: 10px 18px;
            border-radius: 8px;
            font-size: 0.85rem;
            font-weight: 600;
            cursor: pointer;
            transition: all 0.2s ease;
            white-space: nowrap;
        }
        .btn-copy:hover {
            opacity: 0.9;
        }
        .copied {
            background: #00ff66 !important;
            color: #000000 !important;
        }
        .notice {
            margin-top: 20px;
            background: linear-gradient(135deg, rgba(0, 255, 102, 0.08), rgba(0, 177, 64, 0.04));
            border: 1px solid rgba(0, 255, 102, 0.25);
            border-radius: 12px;
            padding: 16px 18px;
            display: flex;
            align-items: flex-start;
            gap: 14px;
        }
        .notice-icon {
            flex-shrink: 0;
            width: 38px;
            height: 38px;
            border-radius: 10px;
            background: #00ff66;
            color: #000000;
            display: flex;
            align-items: center;
            justify-content: center;
            font-size: 1.15rem;
        }
        .notice-title {
            font-size: 1rem;
            font-weight: 800;
            margin: 0 0 4px 0;
            color: #00ff66;
        }
        .notice-text {
            font-size: 0.85rem;
            line-height: 1.5;
            color: #a1a1aa;
            margin: 0;
        }
        .notice-text strong {
            color: #ffffff;
        }
    </style>
</head>
<body>
    <div class="container">
        <h1>Bitlab</h1>
        <div class="url-box">
            <span class="url-text" id="manifest-url">${manifestUrl}</span>
            <button class="btn-copy" onclick="copyManifestUrl()" id="copy-btn">Copy URL</button>
        </div>
        <div class="notice">
            <div class="notice-icon"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"></rect><path d="M7 11V7a5 5 0 0 1 10 0v4"></path></svg></div>
            <div>
                <p class="notice-title">VPN Required</p>
                <p class="notice-text">Bitlab requires an active VPN connection to stream. <strong>Any VPN provider will do</strong>.</p>
            </div>
        </div>
    </div>

    <script>
        function copyManifestUrl() {
            const urlText = document.getElementById('manifest-url').innerText;
            navigator.clipboard.writeText(urlText).then(() => {
                const copyBtn = document.getElementById('copy-btn');
                copyBtn.innerText = 'Copied';
                copyBtn.classList.add('copied');
                setTimeout(() => {
                    copyBtn.innerText = 'Copy URL';
                    copyBtn.classList.remove('copied');
                }, 2000);
            });
        }
    </script>
</body>
</html>`,
    {
      headers: { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': NO_CACHE },
    }
  );
}
