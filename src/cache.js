const NS = 'bitlab';

function cacheUrl(key) {
  return `https://${NS}.cache/${encodeURIComponent(key)}`;
}

export async function cacheGet(key) {
  try {
    const res = await caches.default.match(cacheUrl(key));
    if (!res) return undefined;
    return await res.json();
  } catch {
    return undefined;
  }
}

export async function cachePut(key, value, ttlSecs) {
  try {
    await caches.default.put(
      cacheUrl(key),
      new Response(JSON.stringify(value), {
        headers: { 'Cache-Control': `public, max-age=${ttlSecs}` },
      })
    );
  } catch {
    // cache write failures are non-fatal
  }
}
