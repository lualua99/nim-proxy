//! Authentication: store-backed user sessions for the dashboard and
//! observability endpoints, plus the primitives (constant-time compare,
//! PBKDF2 password hashing, HMAC-signed session cookies, a failed-attempt
//! throttle) the rest of the app uses. The `/v1/*` API keeps its own
//! client-key check in `proxy.rs`; this module protects the operator surface.
//!
//! A session token binds three things: an expiry, the username, and a short
//! fragment of the user's *current* password hash. The fragment means a
//! password change (or admin reset) invalidates that user's outstanding
//! sessions instantly, and the username lookup happens against the live
//! config store on every request — deleting a user kills their session the
//! same moment.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::config::StoredConfig;
use crate::AppState;

const COOKIE: &str = "nimproxy_session";
const SESSION_TTL_SECS: u64 = 12 * 3600;

/// Constant-time byte equality (avoids leaking content via timing). `subtle`
/// short-circuits only on a *length* mismatch — that leaks the secret's length,
/// which is acceptable; the bytes themselves are always compared in full.
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

pub fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// PBKDF2-HMAC-SHA256 iteration count for newly minted hashes (OWASP's
/// recommendation). Every stored hash encodes its own count, so this can be
/// raised later without invalidating existing credentials.
const PBKDF2_ITERS: u32 = 600_000;

/// One PBKDF2-HMAC-SHA256 block (dkLen = 32, one SHA-256 output — all a
/// password hash needs). Hand-rolled over the installed hmac/sha2 pair,
/// pinned by the RFC 7914 §11 test vectors below.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iters: u32) -> [u8; 32] {
    // Key the HMAC once and clone the initialized state per iteration —
    // rekeying every round would double the SHA-256 compressions.
    let keyed = Hmac::<Sha256>::new_from_slice(password).expect("HMAC accepts any key length");
    let prf = |data: &[u8], extra: &[u8]| {
        let mut m = keyed.clone();
        m.update(data);
        m.update(extra);
        m.finalize().into_bytes()
    };
    let mut u = prf(salt, &1u32.to_be_bytes()); // U1 = PRF(P, S || INT(1))
    let mut out: [u8; 32] = u.into();
    for _ in 1..iters {
        u = prf(&u, &[]);
        for (o, b) in out.iter_mut().zip(u.iter()) {
            *o ^= b;
        }
    }
    out
}

/// Hash a password for storage: `pbkdf2-sha256$<iters>$<salt>$<hash>` (hex).
pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).expect("OS RNG for salt");
    let dk = pbkdf2_sha256(password.as_bytes(), &salt, PBKDF2_ITERS);
    format!("pbkdf2-sha256${PBKDF2_ITERS}${}${}", hex(&salt), hex(&dk))
}

/// Verify a password against a stored hash string; malformed strings fail
/// closed. Honors the hash's own iteration count. CPU-bound (~hundreds of
/// ms by design) — call inside `spawn_blocking` on request paths.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some("pbkdf2-sha256"), Some(iters), Some(salt), Some(hash), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let Ok(iters @ 1..) = iters.parse::<u32>() else {
        return false;
    };
    let Some(salt) = unhex(salt) else {
        return false;
    };
    let dk = pbkdf2_sha256(password.as_bytes(), &salt, iters);
    ct_eq(&hex(&dk), hash)
}

/// First 8 hex chars of SHA-256(password_hash): enough to bind a session to
/// a password *generation* (invalidation on change), too short to help brute
/// force the hash itself.
fn pw_fragment(password_hash: &str) -> String {
    sha256_hex(password_hash)[..8].to_owned()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Session/throttle state: a random per-boot signing key (sessions don't
/// survive restarts — deliberate, see the auth-posture ADR), the login
/// throttle, and a memo of the last verified scraper credential so
/// Prometheus polls don't pay PBKDF2 every 15 seconds.
pub struct Admin {
    signing_key: [u8; 32],
    trust_proxy: bool,
    throttle: Mutex<Throttle>,
    /// (HMAC(signing_key, "user:pass"), username) of the last verified
    /// header credential. Cleared whenever users change.
    scraper_memo: Mutex<Option<([u8; 32], String)>>,
}

/// Fixed-window failed-login limiter (per process, not per IP — a reverse
/// proxy should do IP-level limiting; this is a cheap backstop).
struct Throttle {
    window_start: u64,
    failures: u32,
}

const THROTTLE_WINDOW_SECS: u64 = 60;
const THROTTLE_MAX_FAILURES: u32 = 10;

impl Admin {
    pub fn new(trust_proxy: bool) -> Self {
        let mut signing_key = [0u8; 32];
        getrandom::getrandom(&mut signing_key).expect("OS RNG for session key");
        Self {
            signing_key,
            trust_proxy,
            throttle: Mutex::new(Throttle {
                window_start: now(),
                failures: 0,
            }),
            scraper_memo: Mutex::new(None),
        }
    }

    fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.signing_key).expect("HMAC accepts any key length");
        for p in parts {
            mac.update(&(p.len() as u64).to_be_bytes()); // length-prefix each part
            mac.update(p);
        }
        mac.finalize().into_bytes().into()
    }

    /// Mint a session token for `user`:
    /// `hex(expiry).hex(username).pw_fragment.hex(hmac)`.
    pub fn sign_session(&self, expiry: u64, username: &str, password_hash: &str) -> String {
        let frag = pw_fragment(password_hash);
        let tag = self.mac(&[&expiry.to_be_bytes(), username.as_bytes(), frag.as_bytes()]);
        format!(
            "{expiry:x}.{}.{frag}.{}",
            hex(username.as_bytes()),
            hex(&tag)
        )
    }

    /// Verify a session token against the live store: signature intact, not
    /// expired, user still exists, password unchanged since minting.
    /// Returns the authenticated username.
    pub fn verify_session(&self, token: &str, sc: &StoredConfig) -> Option<String> {
        let mut parts = token.split('.');
        let (Some(exp_hex), Some(user_hex), Some(frag), Some(tag_hex), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        let expiry = u64::from_str_radix(exp_hex, 16).ok()?;
        if expiry < now() {
            return None;
        }
        let username = String::from_utf8(unhex(user_hex)?).ok()?;
        let expected = self.mac(&[&expiry.to_be_bytes(), username.as_bytes(), frag.as_bytes()]);
        if !ct_eq(tag_hex, &hex(&expected)) {
            return None;
        }
        let user = sc.user(&username)?;
        ct_eq(frag, &pw_fragment(&user.password_hash)).then_some(username)
    }

    /// Record a failed attempt; returns true if the caller is now throttled.
    pub fn note_failure(&self) -> bool {
        let mut t = self.throttle.lock().unwrap();
        let n = now();
        if n.saturating_sub(t.window_start) >= THROTTLE_WINDOW_SECS {
            t.window_start = n;
            t.failures = 0;
        }
        t.failures += 1;
        t.failures > THROTTLE_MAX_FAILURES
    }

    pub fn is_throttled(&self) -> bool {
        let t = self.throttle.lock().unwrap();
        now().saturating_sub(t.window_start) < THROTTLE_WINDOW_SECS
            && t.failures > THROTTLE_MAX_FAILURES
    }

    /// Forget the memoized scraper credential. Call on any change to users
    /// (password change/reset, user removal) so revocation is immediate.
    pub fn clear_scraper_memo(&self) {
        *self.scraper_memo.lock().unwrap() = None;
    }

    fn memo_hit(&self, cred: &str) -> Option<String> {
        let tag = self.mac(&[cred.as_bytes()]);
        let memo = self.scraper_memo.lock().unwrap();
        let (t, user) = memo.as_ref()?;
        bool::from(tag.ct_eq(t)).then(|| user.clone())
    }

    fn memoize(&self, cred: &str, username: &str) {
        *self.scraper_memo.lock().unwrap() =
            Some((self.mac(&[cred.as_bytes()]), username.to_owned()));
    }

    fn cookie(&self, headers: &HeaderMap, token: &str, max_age: i64) -> String {
        let secure = self.trust_proxy
            && headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|p| p.eq_ignore_ascii_case("https"));
        let secure_attr = if secure { "; Secure" } else { "" };
        format!(
            "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{secure_attr}"
        )
    }
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|c| Some(hex_val(c[0])? << 4 | hex_val(c[1])?))
        .collect()
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{COOKIE}=")) {
            return Some(v.to_owned());
        }
    }
    None
}

/// Resolve the request's identity: a valid session cookie, or scraper-style
/// header credentials (`Authorization: Bearer user:pass` or HTTP Basic)
/// verified against the store. Header verification pays PBKDF2 once, then
/// hits an HMAC memo on subsequent polls. Returns the username.
pub async fn identify(state: &Arc<AppState>, headers: &HeaderMap) -> Option<String> {
    if let Some(tok) = cookie_token(headers) {
        let sc = state.store.lock().unwrap();
        if let Some(user) = state.admin.verify_session(&tok, &sc) {
            return Some(user);
        }
    }
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let cred = if let Some(bearer) = auth.strip_prefix("Bearer ") {
        bearer.trim().to_owned()
    } else {
        let basic = auth.strip_prefix("Basic ")?;
        String::from_utf8(base64_decode(basic.trim())?).ok()?
    };
    if let Some(user) = state.admin.memo_hit(&cred) {
        return Some(user);
    }
    let (username, password) = cred.split_once(':')?;
    let stored_hash = {
        let sc = state.store.lock().unwrap();
        sc.user(username)?.password_hash.clone()
    };
    let password = password.to_owned();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &stored_hash))
        .await
        .unwrap_or(false);
    if !ok {
        return None;
    }
    state.admin.memoize(&cred, username);
    Some(username.to_owned())
}

/// Minimal base64 decoder (standard alphabet, optional padding) — avoids a
/// dependency for the one place we need it (HTTP Basic).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        for &c in chunk {
            acc = (acc << 6) | val(c)?;
        }
        acc <<= 6 * (4 - chunk.len());
        let bytes = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        for i in 0..bytes {
            out.push((acc >> (16 - i * 8)) as u8);
        }
    }
    Some(out)
}

/// axum middleware: gate the operator surface. Pre-setup everything routes
/// to the wizard (browsers) or a 503 (API clients); post-setup a session is
/// required. The authenticated username is stored in request extensions for
/// downstream role checks.
pub async fn require_session(
    State(state): State<Arc<AppState>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if state
        .setup_required
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return if wants_html(req.headers()) {
            redirect("/setup")
        } else {
            setup_required_json()
        };
    }
    match identify(&state, req.headers()).await {
        Some(username) => {
            req.extensions_mut().insert(Identity(username));
            next.run(req).await
        }
        None if wants_html(req.headers()) => redirect_found("/login"),
        None => unauthorized_json(),
    }
}

/// The authenticated username, inserted by [`require_session`] for the
/// settings handlers' role and ownership checks.
#[derive(Clone)]
#[allow(dead_code)] // read by the settings API (next phase)
pub struct Identity(pub String);

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"))
}

fn unauthorized_json() -> Response {
    let body = serde_json::json!({
        "error": {
            "message": "authentication required (session cookie, or Authorization: Bearer <username>:<password>)",
            "type": "proxy_error",
            "code": "unauthorized"
        }
    });
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(body),
    )
        .into_response()
}

pub fn setup_required_json() -> Response {
    let body = serde_json::json!({
        "error": {
            "message": "first-time setup has not been completed; open the dashboard to create the superuser",
            "type": "proxy_error",
            "code": "setup_required"
        }
    });
    (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response()
}

/// `GET /login` — serve the form, bounce to `/setup` pre-setup, or to `/`
/// if already authed.
pub async fn login_page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if state
        .setup_required
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return redirect("/setup");
    }
    if identify(&state, &headers).await.is_some() {
        return redirect_found("/");
    }
    login_html(false)
}

/// `POST /login` — verify username + password, set the session cookie.
pub async fn login_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if state
        .setup_required
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return redirect("/setup");
    }
    let admin = &state.admin;
    if admin.is_throttled() {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return too_many();
    }
    let username = form_field(&body, "username").unwrap_or_default();
    let password = form_field(&body, "password").unwrap_or_default();
    let stored_hash = {
        let sc = state.store.lock().unwrap();
        sc.user(&username).map(|u| u.password_hash.clone())
    };
    // Verify even for unknown users (against a burner hash) so the response
    // time doesn't reveal which usernames exist.
    let hash = stored_hash.clone().unwrap_or_else(|| {
        "pbkdf2-sha256$600000$00000000000000000000000000000000$0000000000000000000000000000000000000000000000000000000000000000".to_owned()
    });
    let pw = password.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&pw, &hash))
        .await
        .unwrap_or(false)
        && stored_hash.is_some();
    if ok {
        let hash = stored_hash.expect("checked above");
        let cookie = mint_session_cookie(&state, &headers, &username, &hash);
        return Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, "/")
            .header(header::SET_COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
    }
    metrics::counter!("nimproxy_login_failures_total").increment(1);
    admin.note_failure();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    login_html(true)
}

/// Mint a Set-Cookie value for a just-verified user — shared by login and
/// the setup wizard's completion.
pub fn mint_session_cookie(
    state: &AppState,
    headers: &HeaderMap,
    username: &str,
    password_hash: &str,
) -> String {
    let expiry = now() + SESSION_TTL_SECS;
    let token = state.admin.sign_session(expiry, username, password_hash);
    state.admin.cookie(headers, &token, SESSION_TTL_SECS as i64)
}

/// `POST /logout` — clear the cookie.
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let cookie = state.admin.cookie(&headers, "", 0);
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/login")
        .header(header::SET_COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

fn redirect(to: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, to)
        .body(Body::empty())
        .unwrap()
}

fn redirect_found(to: &str) -> Response {
    redirect(to)
}

fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "60")],
        "too many failed attempts, try again shortly\n",
    )
        .into_response()
}

/// Parse a single field from an application/x-www-form-urlencoded body.
pub fn form_field(body: &str, field: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == field {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let src = s.replace('+', " ");
    let bytes = src.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Decode a "%XX" escape purely from bytes — never re-slice the &str,
        // which would panic if the window lands inside a multibyte char.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let (hi, lo) = (bytes[i + 1], bytes[i + 2]);
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of a single ASCII hex digit, or None.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The dashboard's 68×68 PNG mark, inlined as base64 so the login card shares
/// the exact same brand logo as the sidebar without an extra request.
const LOGO_PNG_B64: &str =
    "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";

fn login_html(error: bool) -> Response {
    let err = if error {
        r#"<p class="acct-err">Incorrect username or password.</p>"#
    } else {
        ""
    };
    let html = format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>NIM Proxy — Sign in</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap" rel="stylesheet">
<script>
(function () {{
  var t = null;
  try {{ t = localStorage.getItem('np-theme'); }} catch (e) {{}}
  var dark = t ? t === 'dark' : window.matchMedia('(prefers-color-scheme: dark)').matches;
  document.documentElement.setAttribute('data-theme', dark ? 'dark' : 'light');
}})();
</script>
<style>
:root {{
  --bg: #f8f9fb; --card: #ffffff; --card-border: #E5E7EC;
  --ink-1: #111827; --ink-2: #374151; --ink-25: #6B7280; --ink-3: #99A2AF;
  --brand: #6366F1; --brand-lt: #4F46E5; --brand-ring: #C7D2FE;
  --ring-soft: rgba(99,102,241,0.13); --chip-bad: rgba(239,68,68,0.1); --red-dk: #DC2626;
  --shadow-card: 0 1px 2px rgba(16,24,40,0.04), 0 1px 3px rgba(16,24,40,0.06);
  --glow-top: rgba(99,102,241,0.08);
  --mono: 'Inter', system-ui, -apple-system, sans-serif;
}}
:root[data-theme="dark"] {{
  --bg: #111625; --card: #1A2135; --card-border: rgba(255,255,255,0.09);
  --ink-1: #F2F4F8; --ink-2: #CDD5E0; --ink-25: #96A0B4; --ink-3: #6B7688;
  --brand: #818CF8; --brand-lt: #A5B4FC; --brand-ring: #6366F1;
  --ring-soft: rgba(129,140,248,0.2); --chip-bad: rgba(248,113,113,0.16); --red-dk: #FCA5A5;
  --shadow-card: 0 1px 2px rgba(0,0,0,0.25);
  --glow-top: rgba(99,102,241,0.1);
}}
* {{ box-sizing: border-box; }}
body {{
  margin: 0; min-height: 100vh; display: grid; place-items: center; padding: 24px;
  font: 14px/1.45 'Inter', system-ui, sans-serif; color: var(--ink-1);
  background: radial-gradient(1100px 400px at 82% -14%, var(--glow-top), transparent 66%), var(--bg);
}}
.theme-toggle {{
  position: fixed; top: 18px; right: 18px; width: 30px; height: 30px;
  display: inline-flex; align-items: center; justify-content: center;
  border: 1px solid var(--card-border); border-radius: 9px; background: var(--card);
  color: var(--ink-25); box-shadow: var(--shadow-card); cursor: pointer;
}}
.theme-toggle:hover {{ color: var(--ink-1); border-color: var(--brand-ring); }}
.theme-toggle svg {{ flex: none; }}
.theme-toggle .sun {{ display: none; }}
:root[data-theme="dark"] .theme-toggle .sun {{ display: block; }}
:root[data-theme="dark"] .theme-toggle .moon {{ display: none; }}
.card {{
  width: 400px; max-width: 92%; background: var(--card); border: 1px solid var(--card-border);
  border-radius: 14px; padding: 30px 28px; box-shadow: var(--shadow-card);
  animation: fadeIn .3s ease;
}}
@keyframes fadeIn {{ from {{ opacity: 0; transform: translateY(6px); }} to {{ opacity: 1; transform: none; }} }}
.brand {{ display: flex; align-items: center; gap: 11px; margin: 0 0 6px; }}
.brand img {{ flex: none; display: block; border-radius: 10px; box-shadow: var(--shadow-card); }}
.wordmark {{ font-size: 19px; font-weight: 700; letter-spacing: -0.2px; line-height: 1; }}
.wordmark b {{ color: var(--brand-lt); font-weight: 700; }}
p.sub {{ color: var(--ink-3); font-size: 13px; margin: 0 0 20px; }}
.field {{ position: relative; margin-bottom: 14px; }}
.field svg {{ position: absolute; left: 11px; top: 50%; transform: translateY(-50%); stroke: var(--ink-3); }}
.field input {{
  width: 100%; font: 500 13px var(--mono); color: var(--ink-1); background: var(--card);
  border: 1px solid var(--card-border); border-radius: 8px; padding: 10px 12px 10px 34px;
  box-shadow: var(--shadow-card); transition: border-color .15s, box-shadow .15s;
}}
.field input:focus {{ outline: none; border-color: var(--brand-ring); box-shadow: 0 0 0 3px var(--ring-soft); }}
.acct-err {{ font: 600 11px var(--mono); color: var(--red-dk); background: var(--chip-bad);
  border-radius: 99px; padding: 4px 10px; margin: 0 0 16px; }}
button[type="submit"] {{
  width: 100%; font: 600 13px var(--mono); color: #fff; background: var(--brand);
  border: 0; border-radius: 99px; padding: 10px; cursor: pointer;
  transition: background .15s;
}}
button[type="submit"]:hover {{ background: var(--brand-lt); }}
@media (max-width: 480px) {{ .card {{ padding: 24px 20px; }} }}
</style></head><body>
<button class="theme-toggle" id="theme-toggle" title="Toggle dark / light theme" aria-label="Toggle theme">
  <svg class="moon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke-width="1.5"><path d="M13.5 9.5A6 6 0 0 1 6.5 2.5a6 6 0 1 0 7 7z" stroke-linecap="round" stroke-linejoin="round"/></svg>
  <svg class="sun" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke-width="1.5"><circle cx="8" cy="8" r="3.2"/><path d="M8 1v2M8 13v2M1.5 8h2M12.5 8h2M3 3l1.4 1.4M11.6 11.6L13 13M13 3l-1.4 1.4M4.4 11.6L3 13" stroke-linecap="round"/></svg>
</button>
<form class="card" method="post" action="/login">
  <div class="brand">
    <img src="data:image/png;base64,{LOGO_PNG_B64}" width="40" height="40" alt="">
    <span class="wordmark"><b>NIM</b> Proxy</span>
  </div>
  <p class="sub">Sign in to the dashboard.</p>
  {err}
  <div class="field">
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M8 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM2.5 14c.6-2.4 2-3.5 5.5-3.5s4.9 1.1 5.5 3.5"/></svg>
    <input type="text" name="username" placeholder="Username" autofocus autocomplete="username">
  </div>
  <div class="field">
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="7" width="10" height="7" rx="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/></svg>
    <input type="password" name="password" placeholder="Password" autocomplete="current-password">
  </div>
  <button type="submit">Sign in</button>
</form>
<script>
(function () {{
  document.getElementById('theme-toggle').addEventListener('click', function () {{
    var root = document.documentElement;
    var next = root.getAttribute('data-theme') === 'dark' ? 'light' : 'dark';
    root.setAttribute('data-theme', next);
    try {{ localStorage.setItem('np-theme', next); }} catch (e) {{}}
  }});
}})();
</script>
</body></html>"##
    );
    let status = if error {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::OK
    };
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Role, User};

    fn admin() -> Admin {
        Admin::new(false)
    }

    fn store_with(username: &str, password_hash: &str) -> StoredConfig {
        StoredConfig {
            users: vec![User {
                username: username.into(),
                password_hash: password_hash.into(),
                role: Role::Superuser,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn pbkdf2_matches_rfc7914_vectors() {
        // RFC 7914 §11 PBKDF2-HMAC-SHA256 vectors (first 32 of dkLen=64 —
        // blocks are independent, and we only ever derive one).
        assert_eq!(
            hex(&pbkdf2_sha256(b"passwd", b"salt", 1)),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"Password", b"NaCl", 80_000)),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56"
        );
    }

    #[test]
    fn password_hash_round_trips_and_rejects_wrong_password() {
        // Manually built low-iteration hash: the count is read back from the
        // string, so verification honors it (this is also what keeps test
        // fixtures cheap without a prod knob).
        let dk = pbkdf2_sha256(b"hunter22", b"\x01\x02\x03\x04", 1_000);
        let stored = format!("pbkdf2-sha256$1000$01020304${}", hex(&dk));
        assert!(verify_password("hunter22", &stored));
        assert!(!verify_password("hunter23", &stored));
    }

    #[test]
    fn base64_decode_handles_alphabet_padding_and_rejects_garbage() {
        // Round-trips a real Basic credential (letters, digits, ':').
        assert_eq!(
            base64_decode("dXNlcjpwYXNz").unwrap(),
            b"user:pass".to_vec()
        );
        // The '+' (62) and '/' (63) alphabet slots.
        assert_eq!(base64_decode("+AAA").unwrap(), vec![248u8, 0, 0]);
        assert_eq!(base64_decode("/AAA").unwrap(), vec![252u8, 0, 0]);
        assert_eq!(base64_decode("+/AA").unwrap(), vec![251u8, 240, 0]);
        // A 2-byte tail (3-char chunk + one '=' pad).
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma".to_vec());
        // An illegal character and a lone 1-char chunk both fail closed.
        assert!(base64_decode("ab*c").is_none());
        assert!(base64_decode("A").is_none());
    }

    #[test]
    fn unhex_rejects_odd_length() {
        assert!(unhex("abc").is_none());
        assert_eq!(unhex("0a0b").unwrap(), vec![0x0au8, 0x0b]);
    }

    #[test]
    fn verify_session_rejects_malformed_token_shape() {
        let a = admin();
        let sc = store_with("alice", "hash-v1");
        // A token that isn't exactly 5 dotted parts fails closed before any crypto.
        assert!(a.verify_session("a.b.c", &sc).is_none());
        assert!(a
            .verify_session("too.many.dots.here.and.more", &sc)
            .is_none());
    }

    #[test]
    fn cookie_marks_secure_only_behind_a_trusted_https_proxy() {
        let mut https = HeaderMap::new();
        https.insert("x-forwarded-proto", "https".parse().unwrap());
        // trust_proxy = true AND forwarded https -> Secure attribute set.
        assert!(Admin::new(true)
            .cookie(&https, "tok", 3600)
            .contains("; Secure"));
        // The same header is untrusted when trust_proxy = false (can't believe it).
        assert!(!Admin::new(false)
            .cookie(&https, "tok", 3600)
            .contains("; Secure"));
        // Trusted proxy but plain http -> no Secure.
        let mut http = HeaderMap::new();
        http.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!Admin::new(true)
            .cookie(&http, "tok", 3600)
            .contains("; Secure"));
    }

    #[test]
    fn note_failure_resets_after_the_throttle_window_rolls_over() {
        let a = admin();
        {
            let mut t = a.throttle.lock().unwrap();
            t.window_start = now() - THROTTLE_WINDOW_SECS - 1;
            t.failures = 10_000; // would be throttled if the window hadn't rolled
        }
        // The rolled-over window resets the counter, so one fresh failure is
        // well under the limit.
        assert!(!a.note_failure());
    }

    #[test]
    fn malformed_hash_strings_fail_closed() {
        for bad in [
            "",
            "plaintext",
            "pbkdf2-sha256$0$aa$bb",       // zero iterations
            "pbkdf2-sha256$x$aa$bb",       // non-numeric iterations
            "pbkdf2-sha256$1000$zz$bb",    // bad salt hex
            "pbkdf2-sha256$1000$aa",       // missing field
            "pbkdf2-sha256$1000$aa$bb$cc", // extra field
            "scrypt$1000$aa$bb",           // unknown scheme
        ] {
            assert!(!verify_password("x", bad), "accepted: {bad}");
        }
    }

    #[test]
    fn hash_password_emits_current_format() {
        let h = hash_password("correct horse");
        assert!(h.starts_with("pbkdf2-sha256$600000$"), "{h}");
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong horse", &h));
        // Distinct salts: hashing the same password twice differs.
        assert_ne!(h, hash_password("correct horse"));
    }

    #[test]
    fn ct_eq_matches_std_eq() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("", "x"));
    }

    #[test]
    fn valid_session_round_trips_and_carries_identity() {
        let a = admin();
        let sc = store_with("alice", "hash-v1");
        let tok = a.sign_session(now() + 100, "alice", "hash-v1");
        assert_eq!(a.verify_session(&tok, &sc).as_deref(), Some("alice"));
    }

    #[test]
    fn expired_session_rejected() {
        let a = admin();
        let sc = store_with("alice", "hash-v1");
        let tok = a.sign_session(now() - 1, "alice", "hash-v1");
        assert!(a.verify_session(&tok, &sc).is_none());
    }

    #[test]
    fn tampered_session_rejected() {
        let a = admin();
        let sc = store_with("alice", "hash-v1");
        let tok = a.sign_session(now() + 100, "alice", "hash-v1");
        let mut chars: Vec<char> = tok.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        assert!(a.verify_session(&tampered, &sc).is_none());
    }

    #[test]
    fn foreign_key_session_rejected() {
        let a = admin();
        let b = admin(); // different random signing key
        let sc = store_with("alice", "hash-v1");
        let tok = a.sign_session(now() + 100, "alice", "hash-v1");
        assert!(b.verify_session(&tok, &sc).is_none());
    }

    #[test]
    fn password_change_invalidates_existing_sessions() {
        let a = admin();
        let tok = a.sign_session(now() + 100, "alice", "hash-v1");
        let rotated = store_with("alice", "hash-v2");
        assert!(
            a.verify_session(&tok, &rotated).is_none(),
            "session minted against the old password hash must die on change"
        );
    }

    #[test]
    fn deleted_user_session_rejected() {
        let a = admin();
        let tok = a.sign_session(now() + 100, "alice", "hash-v1");
        let sc = store_with("bob", "hash-v1");
        assert!(a.verify_session(&tok, &sc).is_none());
    }

    #[test]
    fn session_username_is_authenticated_not_just_parsed() {
        // Re-labeling the username segment without re-signing must fail.
        let a = admin();
        let sc = StoredConfig {
            users: vec![
                User {
                    username: "alice".into(),
                    password_hash: "h".into(),
                    role: Role::User,
                },
                User {
                    username: "admin".into(),
                    password_hash: "h".into(),
                    role: Role::Superuser,
                },
            ],
            ..Default::default()
        };
        let tok = a.sign_session(now() + 100, "alice", "h");
        let mut parts: Vec<&str> = tok.split('.').collect();
        let admin_hex = hex(b"admin");
        parts[1] = &admin_hex;
        let forged = parts.join(".");
        assert!(a.verify_session(&forged, &sc).is_none());
    }

    #[test]
    fn scraper_memo_round_trips_and_clears() {
        let a = admin();
        assert!(a.memo_hit("alice:pw").is_none());
        a.memoize("alice:pw", "alice");
        assert_eq!(a.memo_hit("alice:pw").as_deref(), Some("alice"));
        assert!(a.memo_hit("alice:other").is_none());
        a.clear_scraper_memo();
        assert!(a.memo_hit("alice:pw").is_none());
    }

    #[test]
    fn basic_auth_decodes() {
        // base64("alice:hunter2")
        let decoded = base64_decode("YWxpY2U6aHVudGVyMg==").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "alice:hunter2");
    }

    #[test]
    fn form_field_parses() {
        assert_eq!(
            form_field("password=hunter2", "password").as_deref(),
            Some("hunter2")
        );
        assert_eq!(
            form_field("a=1&password=p%40ss", "password").as_deref(),
            Some("p@ss")
        );
    }

    #[test]
    fn url_decode_survives_multibyte_and_malformed_escapes() {
        // A multibyte char right after '%' must not panic (a '%XX' window that
        // lands on a non-char-boundary of the original &str). Reachable pre-auth
        // via POST /login, so this must never crash the handler.
        assert_eq!(url_decode("%\u{20ac}"), "%\u{20ac}"); // "%€"
        assert_eq!(url_decode("%a\u{20ac}"), "%a\u{20ac}"); // "%a€"
        assert_eq!(url_decode("caf\u{e9}%20x"), "caf\u{e9} x"); // valid escape amid UTF-8
                                                                // Malformed / truncated escapes pass through untouched.
        assert_eq!(url_decode("%"), "%");
        assert_eq!(url_decode("%z"), "%z");
        assert_eq!(url_decode("%zz"), "%zz");
        assert_eq!(url_decode("100%"), "100%");
        // Well-formed escapes still decode, and '+' still becomes space.
        assert_eq!(url_decode("p%40ss+word"), "p@ss word");
    }
}
