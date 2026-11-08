use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use std::time::{SystemTime, UNIX_EPOCH};

const TOTP_PERIOD: i64 = 30;
const TOTP_DIGITS: usize = 6;
const TOTP_WINDOW: i64 = 1;

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn b32_decode_nopad(s: &str) -> anyhow::Result<Vec<u8>> {
    let s = s.trim_end_matches('=').to_uppercase();
    // base32 crate 0.4 has decode with alphabet?
    base32::decode(base32::Alphabet::RFC4648 { padding: false }, &s).ok_or_else(|| anyhow::anyhow!("bad base32"))
}

pub fn totp_code(secret: &str, t: i64) -> anyhow::Result<String> {
    let key = b32_decode_nopad(secret)?;
    let counter = (t / TOTP_PERIOD) as u64;
    let mut msg = [0u8; 8];
    msg.copy_from_slice(&counter.to_be_bytes());
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
    mac.update(&msg);
    let sum = mac.finalize().into_bytes();
    let off = (sum[sum.len() - 1] & 0x0f) as usize;
    let bin = ((sum[off] as u32 & 0x7f) << 24) | ((sum[off + 1] as u32) << 16) | ((sum[off + 2] as u32) << 8) | (sum[off + 3] as u32);
    Ok(format!("{:06}", bin % 1_000_000))
}

pub fn verify_totp_step(secret: &str, code: &str) -> Option<i64> {
    if secret.trim().is_empty() {
        return None;
    }
    let code = code.trim();
    if code.len() != TOTP_DIGITS {
        return None;
    }
    let now = now_unix();
    for i in -TOTP_WINDOW..=TOTP_WINDOW {
        if let Ok(want) = totp_code(secret, now + i * TOTP_PERIOD) {
            use subtle::ConstantTimeEq;
            if want.as_bytes().ct_eq(code.as_bytes()).into() {
                return Some(now / TOTP_PERIOD + i);
            }
        }
    }
    None
}

pub fn new_totp_secret() -> String {
    use rand::RngCore;
    let mut b = vec![0u8; 20];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
    base32::encode(base32::Alphabet::RFC4648 { padding: false }, &b)
}

fn url_escape(s: &str) -> String {
    s.replace(' ', "%20").replace(':', "%3A").replace('/', "%2F").replace('@', "%40").replace('?', "%3F").replace('&', "%26").replace('=', "%3D").replace('#', "%23")
}

pub fn otpauth_uri(secret: &str, issuer: &str, account: &str) -> String {
    let label = format!("{issuer}:{account}");
    format!("otpauth://totp/{}?secret={secret}&issuer={}&period={TOTP_PERIOD}&digits={TOTP_DIGITS}&algorithm=SHA1", url_escape(&label), url_escape(issuer))
}

pub fn qr_data_uri(text: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let img = code.render::<image::Luma<u8>>().max_dimensions(320, 320).build();
    let mut buf = vec![];
    let enc = image::codecs::png::PngEncoder::new(&mut buf);
    use image::ImageEncoder;
    enc.write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::L8).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(format!("data:image/png;base64,{}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, buf)))
}

const RECOVERY_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

pub fn hash_recovery_code(code: &str) -> String {
    let norm: String = code.to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(norm.as_bytes());
    hex::encode(h.finalize())
}

pub fn generate_recovery_codes(n: usize) -> (Vec<String>, Vec<String>) {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut plain = vec![];
    let mut hashes = vec![];
    for _ in 0..n {
        let mut b = String::new();
        for _ in 0..8 {
            let idx = rng.random_range(0..RECOVERY_ALPHABET.len());
            b.push(RECOVERY_ALPHABET[idx] as char);
        }
        let code = format!("{}-{}", &b[..4], &b[4..]);
        hashes.push(hash_recovery_code(&code));
        plain.push(code);
    }
    (plain, hashes)
}

pub fn two_factor_enabled(app: &crate::server::App) -> bool {
    app.store.get_setting("totp_enabled", "") == "1" && !app.store.get_setting("totp_secret", "").is_empty()
}

fn recovery_hashes(app: &crate::server::App) -> Vec<String> {
    serde_json::from_str(&app.store.get_setting("totp_recovery", "[]")).unwrap_or_default()
}

fn save_recovery_hashes(app: &crate::server::App, h: &[String]) -> anyhow::Result<()> {
    let b = serde_json::to_string(h)?;
    app.store.set_setting("totp_recovery", &b)?;
    Ok(())
}

pub fn check_password(app: &crate::server::App, pw: &str) -> bool {
    use subtle::ConstantTimeEq;
    app.cfg.admin_password.as_bytes().ct_eq(pw.as_bytes()).into()
}

pub async fn check_second_factor(app: &crate::server::App, code: &str) -> bool {
    let _g = app.totp_mu.lock().await;
    let code = code.trim();
    if code.is_empty() {
        return false;
    }
    if let Some(step) = verify_totp_step(&app.store.get_setting("totp_secret", ""), code) {
        let last: i64 = app.store.get_setting("totp_last_step", "0").parse().unwrap_or(0);
        if step <= last {
            return false;
        }
        if app.store.set_setting("totp_last_step", &step.to_string()).is_err() {
            return false;
        }
        return true;
    }
    let norm = code.to_uppercase().trim().to_string();
    if norm.replace('-', "").len() < 8 {
        return false;
    }
    let want = hash_recovery_code(&norm);
    let hashes = recovery_hashes(app);
    for (i, h) in hashes.iter().enumerate() {
        use subtle::ConstantTimeEq;
        if h.as_bytes().ct_eq(want.as_bytes()).into() {
            let mut rest = hashes.clone();
            rest.remove(i);
            if save_recovery_hashes(app, &rest).is_err() {
                return false;
            }
            return true;
        }
    }
    false
}

pub fn session_name(ua: &str) -> String {
    let l = ua.to_lowercase();
    let browser = if l.contains("firefox") {
        "Firefox"
    } else if l.contains("edg/") {
        "Edge"
    } else if l.contains("chrome") {
        "Chrome"
    } else if l.contains("safari") {
        "Safari"
    } else if l.contains("curl") {
        "curl"
    } else {
        "Browser"
    };
    let os = if l.contains("iphone") || l.contains("ipad") {
        "iOS"
    } else if l.contains("android") {
        "Android"
    } else if l.contains("mac os") {
        "macOS"
    } else if l.contains("windows") {
        "Windows"
    } else if l.contains("linux") {
        "Linux"
    } else {
        ""
    };
    if os.is_empty() {
        browser.to_string()
    } else {
        format!("{browser} on {os}")
    }
}

pub fn issue_cookie_value(app: &crate::server::App, sess: &crate::store::SessionRecord) -> String {
    format!("{}.{}", sess.id, app.session_mac(&sess.id))
}

// --- handlers ---

pub async fn handle_security(State(app): State<crate::server::App>, headers: HeaderMap) -> Response {
    if crate::admin::current_session(&app, &headers).await.is_none() {
        return crate::server::http_error(401, "not signed in");
    }
    let sessions = app.store.list_sessions().unwrap_or_default();
    let cur = crate::admin::current_session(&app, &headers).await.map(|s| s.id).unwrap_or_default();
    crate::server::json_response(
        200,
        serde_json::json!({
            "two_factor": two_factor_enabled(&app),
            "recovery_left": recovery_hashes(&app).len(),
            "sessions": sessions,
            "current": cur,
        }),
    )
}

pub async fn handle_2fa_begin(State(app): State<crate::server::App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if crate::admin::current_session(&app, &headers).await.is_none() {
        return crate::server::http_error(401, "not signed in");
    }
    #[derive(serde::Deserialize)]
    struct B {
        #[serde(default)]
        password: String,
    }
    let b: B = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return crate::server::http_error(400, "bad json"),
    };
    // rate limit via login window is in admin.rs; replicate minimal check: use password check only
    if !check_password(&app, &b.password) {
        return crate::server::http_error(401, "wrong password");
    }
    if two_factor_enabled(&app) {
        return crate::server::http_error(409, "two-factor is already enabled");
    }
    let secret = new_totp_secret();
    let _ = app.store.set_setting("totp_pending", &secret);
    let uri = otpauth_uri(&secret, "Bitlab", "admin");
    let qr = match qr_data_uri(&uri) {
        Ok(q) => q,
        Err(e) => return crate::server::http_error(500, &e.to_string()),
    };
    crate::server::json_response(200, serde_json::json!({"secret": secret, "otpauth": uri, "qr": qr}))
}

pub async fn handle_2fa_enable(State(app): State<crate::server::App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if crate::admin::current_session(&app, &headers).await.is_none() {
        return crate::server::http_error(401, "not signed in");
    }
    #[derive(serde::Deserialize)]
    struct B {
        #[serde(default)]
        code: String,
        #[serde(default)]
        password: String,
    }
    let b: B = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return crate::server::http_error(400, "bad json"),
    };
    if !check_password(&app, &b.password) {
        return crate::server::http_error(401, "wrong password");
    }
    let pending = app.store.get_setting("totp_pending", "");
    if pending.is_empty() {
        return crate::server::http_error(400, "start setup first");
    }
    let step = match verify_totp_step(&pending, &b.code) {
        Some(s) => s,
        None => return crate::server::http_error(400, "that code did not verify"),
    };
    let (plain, hashes) = generate_recovery_codes(10);
    let _ = app.store.set_setting("totp_secret", &pending);
    let _ = app.store.set_setting("totp_last_step", &step.to_string());
    let _ = app.store.set_setting("totp_enabled", "1");
    let _ = app.store.delete_setting("totp_pending");
    let _ = save_recovery_hashes(&app, &hashes);
    if let Some(cur) = crate::admin::current_session(&app, &headers).await {
        app.store.delete_other_sessions(&cur.id);
    }
    tracing::info!("two-factor enabled");
    crate::server::json_response(200, serde_json::json!({"ok": true, "recovery_codes": plain}))
}

pub async fn handle_2fa_disable(State(app): State<crate::server::App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if crate::admin::current_session(&app, &headers).await.is_none() {
        return crate::server::http_error(401, "not signed in");
    }
    #[derive(serde::Deserialize)]
    struct B {
        #[serde(default)]
        password: String,
        #[serde(default)]
        code: String,
    }
    let b: B = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return crate::server::http_error(400, "bad json"),
    };
    if !check_password(&app, &b.password) {
        return crate::server::http_error(401, "wrong password");
    }
    if !check_second_factor(&app, &b.code).await {
        return crate::server::http_error(401, "a current two-factor code or recovery code is required");
    }
    let _ = app.store.delete_setting("totp_secret");
    let _ = app.store.delete_setting("totp_enabled");
    let _ = app.store.delete_setting("totp_pending");
    let _ = save_recovery_hashes(&app, &[]);
    let _ = app.store.set_setting("totp_last_step", "0");
    if let Some(cur) = crate::admin::current_session(&app, &headers).await {
        app.store.delete_other_sessions(&cur.id);
    }
    tracing::info!("two-factor disabled");
    crate::server::json_response(200, serde_json::json!({"ok": true}))
}

pub async fn handle_delete_session(State(app): State<crate::server::App>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if crate::admin::current_session(&app, &headers).await.is_none() {
        return crate::server::http_error(401, "not signed in");
    }
    if id.len() < 8 {
        return crate::server::http_error(404, "session not found");
    }
    if app.store.delete_session(&id).is_err() {
        return crate::server::http_error(500, "delete failed");
    }
    // if deleted current, clear cookie
    let mut r = crate::server::json_response(200, serde_json::json!({"ok": true}));
    if let Some(cur) = crate::admin::current_session(&app, &headers).await {
        if cur.id == id {
            r.headers_mut().insert("set-cookie", "bitlab_session=; Path=/; Max-Age=-1".parse().unwrap());
        }
    }
    tracing::info!("session {} revoked", &id[..8.min(id.len())]);
    r
}

pub async fn handle_revoke_others(State(app): State<crate::server::App>, headers: HeaderMap) -> Response {
    let cur = match crate::admin::current_session(&app, &headers).await {
        Some(s) => s,
        None => return crate::server::http_error(401, "not signed in"),
    };
    app.store.delete_other_sessions(&cur.id);
    crate::server::json_response(200, serde_json::json!({"ok": true}))
}

// --- TTL ---

pub fn parse_ttl(v: &str) -> anyhow::Result<i64> {
    let raw = v.trim().to_lowercase();
    if raw.is_empty() || raw == "never" || raw == "none" || raw == "-" || raw == "0" {
        return Ok(0);
    }
    // try date layouts
    for layout in ["%Y-%m-%d", "%Y/%m/%d", "%d-%m-%Y"] {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(&raw, layout) {
            let dt = d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
            let until = dt - now_unix();
            if until <= 0 {
                anyhow::bail!("that date has already passed");
            }
            return Ok(until);
        }
    }
    // RFC3339
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&raw) {
        let until = dt.timestamp() - now_unix();
        if until <= 0 {
            anyhow::bail!("that date has already passed");
        }
        return Ok(until);
    }
    // "2006-01-02 15:04"
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&raw, "%Y-%m-%d %H:%M") {
        let until = dt.and_utc().timestamp() - now_unix();
        if until <= 0 {
            anyhow::bail!("that date has already passed");
        }
        return Ok(until);
    }
    // duration
    let mut i = 0;
    let b = raw.as_bytes();
    while i < b.len() && (b[i] == b'.' || b[i].is_ascii_digit()) {
        i += 1;
    }
    let (num, unit) = (raw[..i].trim(), raw[i..].trim());
    if num.is_empty() || unit.is_empty() {
        anyhow::bail!("use a duration like 3d, 2w, 3m, 1y or a date like 2027-06-01");
    }
    let n: f64 = num.parse().map_err(|_| anyhow::anyhow!("invalid amount {num:?}"))?;
    if !(n > 0.0) {
        anyhow::bail!("invalid amount {num:?}");
    }
    let mult: f64 = match unit {
        "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 24.0 * 3600.0,
        "w" | "wk" | "week" | "weeks" => 7.0 * 24.0 * 3600.0,
        "m" | "mo" | "mon" | "month" | "months" => 30.0 * 24.0 * 3600.0,
        "y" | "yr" | "year" | "years" => 365.0 * 24.0 * 3600.0,
        _ => anyhow::bail!("unknown unit {unit:?} — use h, d, w, m (months) or y"),
    };
    let secs = n * mult;
    if !secs.is_finite() || secs <= 0.0 {
        anyhow::bail!("invalid duration");
    }
    let d = secs as i64;
    if d <= 0 || d > 50 * 365 * 24 * 3600 {
        anyhow::bail!("that is longer than 50 years");
    }
    Ok(d)
}
