import { cacheGet, cachePut } from './cache.js';

const VPN_CACHE_TTL_SECS = 24 * 60 * 60;

export function clientIp(headers) {
  for (const name of ['x-forwarded-for', 'x-forwarded-real', 'cf-connecting-ip', 'x-real-ip']) {
    const value = headers.get(name);
    if (value) {
      const first = value.split(',')[0].trim().replace(/^"+|"+$/g, '');
      if (first) return first;
    }
  }
  return null;
}

export function isPrivateOrLoopback(ip) {
  const v4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(ip);
  if (v4) {
    const a = Number(v4[1]);
    const b = Number(v4[2]);
    const c = Number(v4[3]);
    const d = Number(v4[4]);
    if (a > 255 || b > 255 || c > 255 || d > 255) return false;
    if (a === 127) return true; // loopback
    if (a === 10) return true; // private
    if (a === 172 && b >= 16 && b <= 31) return true; // private
    if (a === 192 && b === 168) return true; // private
    if (a === 169 && b === 254) return true; // link-local
    if (a === 0 && b === 0 && c === 0 && d === 0) return true; // unspecified
    return false;
  }
  if (ip === '::1') return true; // loopback
  if (ip === '::') return true; // unspecified
  if (/^f[cd]/i.test(ip)) return true; // unique local (fc00::/7)
  return false;
}

export async function isVpn(ip) {
  if (isPrivateOrLoopback(ip)) return true;

  const cacheKey = `vpn:${ip}`;
  const cached = await cacheGet(cacheKey);
  if (cached !== undefined) return cached;

  let vpn = false;
  try {
    const res = await fetch(
      `http://ip-api.com/json/${ip}?fields=status,message,proxy,hosting`,
      { signal: AbortSignal.timeout(2500) }
    );
    if (res) {
      const info = await res.json();
      if (info && info.status === 'success') vpn = Boolean(info.proxy) || Boolean(info.hosting);
    }
  } catch {
    // fail-open: API errors allow access
  }

  await cachePut(cacheKey, vpn, VPN_CACHE_TTL_SECS);
  return vpn;
}
