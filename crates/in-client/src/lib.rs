//! in-client: drop-in OIDC client for topcoat apps authenticating against im.
//!
//! An app adds three things: `in_client::mount(builder, config)` when it
//! builds its router, `.discover()` as usual (the `/auth/login`,
//! `/auth/callback`, `/auth/logout` routes register themselves), and
//! `in_client::current_user(cx)` wherever it needs the person.
//!
//! The model: im holds the central session; this crate holds the app's side
//! of it — an encrypted cookie with the identity claims and the refresh
//! token. When the access token ages out, `current_user` silently rotates it
//! against im; when im has revoked the central session, the rotation is
//! refused and the app sees a signed-out user.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as b64url;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use topcoat::context::{Cx, try_app_context};
use topcoat::cookie::{Cookie, Cookies, cookie, cookies};
use topcoat::router::response::{IntoResponse, Response};
use topcoat::router::{HeaderValue, RouterBuilder, StatusCode, header, route};

// ---------------------------------------------------------------------------
// Configuration and state
// ---------------------------------------------------------------------------

/// Everything the app knows about its im registration. `client_secret` and
/// `cookie_key` are secrets: the first authenticates the app to im, the
/// second seals the browser cookies this crate writes.
#[derive(Clone)]
pub struct Config {
    /// im's base URL, e.g. `http://127.0.0.1:7650` or `https://auth.example.com`.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Must exactly match one of the URIs registered with im.
    pub redirect_uri: String,
    /// The app's session cookie name, e.g. `in_session`.
    pub cookie_name: String,
    /// 32 bytes, generated once per app and kept out of the repository.
    pub cookie_key: [u8; 32],
}

/// The registered state: the config, one HTTP client, and the JWKS cache.
pub struct InClient {
    config: Config,
    http: reqwest::Client,
    jwks: tokio::sync::RwLock<Option<JwksCache>>,
}

struct JwksCache {
    fetched_at: OffsetDateTime,
    /// kid -> public key
    keys: Vec<(String, rsa::RsaPublicKey)>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(String),
    #[error("im refused: {0}")]
    Refused(String),
    #[error("bad token: {0}")]
    Token(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl InClient {
    pub fn new(config: Config) -> Self {
        InClient {
            config,
            http: reqwest::Client::new(),
            jwks: tokio::sync::RwLock::new(None),
        }
    }
}

/// Registers the client state on the router. The routes themselves register
/// through `discover()` — call it as usual.
pub fn mount(builder: RouterBuilder, config: Config) -> RouterBuilder {
    builder.app_context(InClient::new(config))
}

fn client(cx: &Cx) -> &InClient {
    try_app_context::<InClient>(cx).expect("in_client::mount was called on the router")
}

/// The authenticated person, if this browser has one. Every call asks im:
/// the cookie holds an opaque session token, introspected per request, so an
/// admin revoking the person signs them out of this app immediately — there
/// is no token-validity window in which a ghost lives.
pub async fn current_user(cx: &Cx) -> Option<User> {
    let state = client(cx);
    let sealed = cookies(cx).get(&state.config.cookie_name)?;
    let session: Session = open_json(&state.config.cookie_key, sealed.value())?;
    if session.exp <= OffsetDateTime::now_utc().unix_timestamp() {
        clear_cookie(cx, &state.config.cookie_name);
        return None;
    }
    match introspect(state, &session.app_session).await {
        Introspected::Active(user, _exp) => Some(user),
        Introspected::Revoked => {
            clear_cookie(cx, &state.config.cookie_name);
            None
        }
        // The question went unanswered — a blackholed route, a dropped
        // connection, a body that is not the JSON im speaks. Nothing was
        // learned about the session, so this request reads as signed-out
        // while the cookie stays for the next request to try again.
        Introspected::Unanswered => None,
    }
}

/// The person's profile photo from im, if im has one. `None` on 404 and on
/// anything else that is not the bytes — a dropped connection, a body that
/// will not read, a reply without a mime — because a missing face must never
/// fail the page around it: the caller renders its initials instead.
///
/// Authenticated as the app, not the browser: `Authorization: Basic
/// base64(client_id ":" client_secret)` against im's `/photo/{user_id}`,
/// the same credentials the introspection round-trip posts with.
pub async fn photo_for(cx: &Cx, user_id: &str) -> Option<(Vec<u8>, String)> {
    let state = client(cx);
    let reply = state
        .http
        .get(format!("{}/photo/{user_id}", state.config.issuer))
        .basic_auth(&state.config.client_id, Some(&state.config.client_secret))
        .send()
        .await
        .ok()?;
    if !reply.status().is_success() {
        return None;
    }
    let mime = reply
        .headers()
        .get(reqwest::header::CONTENT_TYPE)?
        .to_str()
        .ok()?
        .to_string();
    let bytes = reply.bytes().await.ok()?.to_vec();
    Some((bytes, mime))
}

/// Path of im's RFC 7662 introspection endpoint, relative to the issuer.
const INTROSPECT_PATH: &str = "/introspect";

/// What one introspection round-trip settled.
enum Introspected {
    /// im answered `"active": true` with usable claims.
    Active(User, i64),
    /// im answered `"active": false`: the central session is gone and the
    /// app cookie goes with it.
    Revoked,
    /// The question itself went unanswered — the transport failed, the body
    /// was not JSON, or the claims were missing their fields. Nothing about
    /// the session was learned, so the cookie must stay.
    Unanswered,
}

async fn introspect(state: &InClient, token: &str) -> Introspected {
    let Ok(reply) = state
        .http
        .post(format!("{}{}", state.config.issuer, INTROSPECT_PATH))
        .form(&[
            ("token", token),
            ("client_id", &state.config.client_id),
            ("client_secret", &state.config.client_secret),
        ])
        .send()
        .await
    else {
        return Introspected::Unanswered;
    };
    let Ok(answer): Result<serde_json::Value, _> = reply.json().await else {
        return Introspected::Unanswered;
    };
    // Only an explicit `"active": false` revokes. A missing or non-boolean
    // `active` is a malformed answer, not a revocation.
    match answer.get("active") {
        Some(serde_json::Value::Bool(true)) => {}
        Some(serde_json::Value::Bool(false)) => return Introspected::Revoked,
        _ => return Introspected::Unanswered,
    }
    let (Some(sub), Some(email), Some(name), Some(exp)) = (
        answer["sub"].as_str(),
        answer["email"].as_str(),
        answer["name"].as_str(),
        answer["exp"].as_i64(),
    ) else {
        return Introspected::Unanswered;
    };
    Introspected::Active(
        User {
            sub: sub.to_string(),
            email: email.to_string(),
            name: name.to_string(),
        },
        exp,
    )
}

/// Who this browser is, from the claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub sub: String,
    pub email: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// The app session cookie
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Session {
    /// The opaque token im issued at the code exchange; introspected per
    /// request, never trusted on its own.
    app_session: String,
    exp: i64,
}

/// Test seam for integration tests: seals a session cookie exactly like the
/// `/auth/callback` handler does, without a live im. Returns the cookie
/// VALUE only; tests set the `Cookie` header themselves.
#[cfg(feature = "test-seam")]
pub fn mint_session_cookie(
    config: &Config,
    app_session: &str,
    exp: time::OffsetDateTime,
) -> String {
    seal_json(
        &config.cookie_key,
        &Session {
            app_session: app_session.to_string(),
            exp: exp.unix_timestamp(),
        },
    )
}

/// The introspection path `current_user` POSTs to, so a fake-im test server
/// can mount the right route.
#[cfg(feature = "test-seam")]
pub fn introspect_path() -> &'static str {
    INTROSPECT_PATH
}

/// The in-flight login: PKCE verifier, state, nonce, and where to land after.
#[derive(Serialize, Deserialize)]
struct InFlight {
    verifier: String,
    state: String,
    nonce: String,
    next: String,
    exp: i64,
}

const IN_FLIGHT_MINUTES: i64 = 10;

fn seal(key: &[u8; 32], plaintext: &[u8]) -> String {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce_bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
        .expect("XChaCha20-Poly1305 cannot fail on a payload this small");
    let mut payload = nonce_bytes.to_vec();
    payload.extend_from_slice(&ciphertext);
    b64url.encode(payload)
}

fn open(key: &[u8; 32], sealed: &str) -> Option<Vec<u8>> {
    let payload = b64url.decode(sealed).ok()?;
    if payload.len() < 24 {
        return None;
    }
    let (nonce_bytes, ciphertext) = payload.split_at(24);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce_bytes), ciphertext)
        .ok()
}

fn seal_json<T: Serialize>(key: &[u8; 32], value: &T) -> String {
    seal(key, &serde_json::to_vec(value).expect("plain data"))
}

fn open_json<T: for<'de> Deserialize<'de>>(key: &[u8; 32], sealed: &str) -> Option<T> {
    serde_json::from_slice(&open(key, sealed)?).ok()
}

fn app_cookies(cx: &Cx) -> impl Cookies {
    cookies(cx)
        .default_secure(client(cx).config.issuer.starts_with("https://"))
        .default_http_only(true)
        .default_same_site(topcoat::cookie::SameSite::Lax)
        .default_path("/")
}

fn set_session_cookie(cx: &Cx, state: &InClient, session: &Session) {
    let name = state.config.cookie_name.clone();
    app_cookies(cx).add(cookie! {
        name = seal_json(&state.config.cookie_key, session);
        Path = "/";
        HttpOnly;
        SameSite = Lax;
        MaxAge = time::Duration::days(30)
    });
}

fn clear_cookie(cx: &Cx, name: &str) {
    app_cookies(cx).remove(Cookie::build((name.to_string(), "")).path("/").build());
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn see(cx: &Cx, location: &str) -> Result<Response, topcoat::Error> {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, location_value(location))],
    )
        .into_response(cx)
}

/// A `Location` value that cannot panic its handler: anything `HeaderValue`
/// still refuses (a hostile `next` sealed before the strict validation, a
/// query echo) lands on `/` instead of killing the request.
fn location_value(location: &str) -> HeaderValue {
    HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/"))
}

/// A `next` worth honoring: a local absolute path, never `//elsewhere`,
/// never carrying control bytes, never touching a backslash. The query
/// arrives percent-decoded, so `%0D%0A` is a real CR LF by the time it is
/// checked — and a sealed `next` is rendered into a `Location` header, so
/// anything `HeaderValue` would choke on must die here. Backslashes die
/// here too: browsers normalize `\` to `/` in `Location`, turning
/// `/\evil` into a cross-origin hop.
fn safe_next(raw: &str) -> &str {
    if raw.starts_with('/')
        && !raw.starts_with("//")
        && !raw.contains('\\')
        && !raw.chars().any(|c| c.is_control())
    {
        raw
    } else {
        "/"
    }
}

/// A JWKS older than this is refreshed before the next validation.
const JWKS_TTL_SECONDS: i64 = 3600;

async fn jwks_stale(state: &InClient) -> bool {
    match state.jwks.read().await.as_ref() {
        None => true,
        Some(cache) => {
            cache.fetched_at < OffsetDateTime::now_utc() - time::Duration::seconds(JWKS_TTL_SECONDS)
        }
    }
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| urldecode(v))
    })
}

/// Percent-decodes a query value (`+` is a space, form-style).
fn urldecode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    b64url.encode(buf)
}

/// Sends the browser to im's `/authorize` with a fresh PKCE pair.
#[route(GET "/auth/login")]
async fn in_login(cx: &Cx) -> Result<Response, topcoat::Error> {
    let state = client(cx);
    let query = topcoat::router::request::uri(cx)
        .query()
        .unwrap_or("")
        .to_string();
    let next = safe_next(query_value(&query, "next").as_deref().unwrap_or("/")).to_string();

    let flight = InFlight {
        verifier: random_b64(32),
        state: random_b64(16),
        nonce: random_b64(16),
        next,
        exp: OffsetDateTime::now_utc().unix_timestamp() + IN_FLIGHT_MINUTES * 60,
    };
    let challenge = b64url.encode(Sha256::digest(flight.verifier.as_bytes()));
    let cookie_name = format!("{}_pkce", state.config.cookie_name);
    app_cookies(cx).add(cookie! {
        cookie_name = seal_json(&state.config.cookie_key, &flight);
        Path = "/";
        HttpOnly;
        SameSite = Lax;
        MaxAge = time::Duration::minutes(IN_FLIGHT_MINUTES)
    });

    let url = format!(
        "{}/authorize?response_type=code&client_id={}&redirect_uri={}&scope=openid%20profile%20email&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        state.config.issuer,
        state.config.client_id,
        urlencoded(&state.config.redirect_uri),
        flight.state,
        flight.nonce,
        challenge,
    );
    see(cx, &url)
}

/// im's answer lands here: exchange the code, validate the id_token, mint
/// the app session.
#[route(GET "/auth/callback")]
async fn in_callback(cx: &Cx) -> Result<Response, topcoat::Error> {
    let state = client(cx);
    let query = topcoat::router::request::uri(cx)
        .query()
        .unwrap_or("")
        .to_string();
    if let Some(error) = query_value(&query, "error") {
        return see(cx, &format!("/?auth_error={error}"));
    }
    let (Some(code), Some(presented_state)) =
        (query_value(&query, "code"), query_value(&query, "state"))
    else {
        return see(cx, "/?auth_error=invalid_request");
    };
    let cookie_name = format!("{}_pkce", state.config.cookie_name);
    let flight: Option<InFlight> = cookies(cx)
        .get(&cookie_name)
        .and_then(|c| open_json(&state.config.cookie_key, c.value()));
    let Some(flight) = flight else {
        return see(cx, "/?auth_error=invalid_request");
    };
    clear_cookie(cx, &cookie_name);
    if flight.exp <= OffsetDateTime::now_utc().unix_timestamp()
        || flight
            .state
            .as_bytes()
            .ct_eq(presented_state.as_bytes())
            .unwrap_u8()
            != 1
    {
        return see(cx, "/?auth_error=invalid_state");
    }

    match exchange_code(state, &code, &flight).await {
        Ok(session) => {
            set_session_cookie(cx, state, &session);
            // Re-validated, not trusted: the flight cookie may predate the
            // strict `next` rules, and its bytes land in a header.
            see(cx, safe_next(&flight.next))
        }
        Err(_) => see(cx, "/?auth_error=exchange_failed"),
    }
}

/// Signs out of the app only. To end the central session too, post to im's
/// `/logout` — a link there is a silent re-login, by design.
#[route(GET "/auth/logout")]
async fn in_logout(cx: &Cx) -> Result<Response, topcoat::Error> {
    clear_cookie(cx, &client(cx).config.cookie_name);
    see(cx, "/")
}

fn urlencoded(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Token exchange, refresh, validation
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenAnswer {
    /// Validated for its nonce and signature; the identity the app serves
    /// comes from introspection, not from these claims.
    id_token: String,
    /// Not OIDC — im's own: the opaque session this crate introspects per
    /// request. Absent means the issuer is not a current im.
    app_session: Option<String>,
}

async fn exchange_code(state: &InClient, code: &str, flight: &InFlight) -> Result<Session> {
    let answer: TokenAnswer = state
        .http
        .post(format!("{}/token", state.config.issuer))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &state.config.redirect_uri),
            ("client_id", &state.config.client_id),
            ("client_secret", &state.config.client_secret),
            ("code_verifier", &flight.verifier),
        ])
        .send()
        .await
        .map_err(|e| Error::Http(e.to_string()))?
        .error_for_status()
        .map_err(|e| Error::Refused(e.to_string()))?
        .json()
        .await
        .map_err(|e| Error::Http(e.to_string()))?;
    let claims = validate_id_token(state, &answer.id_token).await?;
    if claims["nonce"].as_str() != Some(flight.nonce.as_str()) {
        return Err(Error::Token("nonce mismatch".into()));
    }
    let app_session = answer
        .app_session
        .ok_or_else(|| Error::Token("issuer gave no app session".into()))?;
    // The session's expiry and identity come from a live introspection, not
    // from the token answer — the same check every later request will make.
    // Any non-answer (revoked or unanswered alike) means there is no fresh
    // session to mint.
    let (_user, exp) = match introspect(state, &app_session).await {
        Introspected::Active(user, exp) => (user, exp),
        Introspected::Revoked | Introspected::Unanswered => {
            return Err(Error::Refused("fresh app session does not introspect".into()));
        }
    };
    Ok(Session { app_session, exp })
}

/// Validates an id_token: RS256 against im's JWKS, issuer, audience, expiry.
/// The nonce is the caller's to check (only the callback holds one).
async fn validate_id_token(state: &InClient, token: &str) -> Result<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(Error::Token("not a compact JWT".into()));
    }
    let header: serde_json::Value = serde_json::from_slice(
        &b64url
            .decode(parts[0])
            .map_err(|e| Error::Token(e.to_string()))?,
    )
    .map_err(|e| Error::Token(e.to_string()))?;
    if header["alg"] != "RS256" {
        return Err(Error::Token("unexpected alg".into()));
    }
    let kid = header["kid"]
        .as_str()
        .ok_or_else(|| Error::Token("no kid".into()))?
        .to_string();

    if jwks_stale(state).await {
        refetch_jwks(state).await?;
    }
    let key = match cached_key(state, &kid).await {
        Some(key) => key,
        None => {
            refetch_jwks(state).await?;
            cached_key(state, &kid)
                .await
                .ok_or_else(|| Error::Token("unknown kid after refetch".into()))?
        }
    };

    verify_claims(&parts, &key, &state.config.issuer, &state.config.client_id)
}

/// The pure half of [`validate_id_token`]: signature, issuer, audience,
/// expiry. Split out so the test suite signs its own tokens without standing
/// a server up.
fn verify_claims(
    parts: &[&str],
    key: &rsa::RsaPublicKey,
    issuer: &str,
    client_id: &str,
) -> Result<serde_json::Value> {
    use rsa::signature::Verifier;
    let signature_bytes = b64url
        .decode(parts[2])
        .map_err(|e| Error::Token(e.to_string()))?;
    let signature = rsa::pkcs1v15::Signature::try_from(signature_bytes.as_slice())
        .map_err(|e| Error::Token(e.to_string()))?;
    let verifying = rsa::pkcs1v15::VerifyingKey::<sha2_for_rsa::Sha256>::new(key.clone());
    verifying
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .map_err(|_| Error::Token("bad signature".into()))?;

    let claims: serde_json::Value = serde_json::from_slice(
        &b64url
            .decode(parts[1])
            .map_err(|e| Error::Token(e.to_string()))?,
    )
    .map_err(|e| Error::Token(e.to_string()))?;
    if claims["iss"].as_str() != Some(issuer) {
        return Err(Error::Token("wrong issuer".into()));
    }
    if claims["aud"].as_str() != Some(client_id) {
        return Err(Error::Token("wrong audience".into()));
    }
    match claims["exp"].as_i64() {
        Some(exp) if exp > OffsetDateTime::now_utc().unix_timestamp() => {}
        _ => return Err(Error::Token("expired".into())),
    }
    Ok(claims)
}

async fn cached_key(state: &InClient, kid: &str) -> Option<rsa::RsaPublicKey> {
    state
        .jwks
        .read()
        .await
        .as_ref()?
        .keys
        .iter()
        .find(|(k, _)| k == kid)
        .map(|(_, key)| key.clone())
}

/// Fetches and caches im's JWKS. Cached for an hour at most; an unknown kid
/// forces an immediate refetch above.
async fn refetch_jwks(state: &InClient) -> Result<()> {
    let doc: serde_json::Value = state
        .http
        .get(format!("{}/jwks.json", state.config.issuer))
        .send()
        .await
        .map_err(|e| Error::Http(e.to_string()))?
        .error_for_status()
        .map_err(|e| Error::Refused(e.to_string()))?
        .json()
        .await
        .map_err(|e| Error::Http(e.to_string()))?;
    let mut keys = Vec::new();
    for jwk in doc["keys"].as_array().into_iter().flatten() {
        let (Some(kid), Some(n), Some(e)) =
            (jwk["kid"].as_str(), jwk["n"].as_str(), jwk["e"].as_str())
        else {
            continue;
        };
        let (Ok(n), Ok(e)) = (b64url.decode(n), b64url.decode(e)) else {
            continue;
        };
        let key = rsa::RsaPublicKey::new(
            rsa::BigUint::from_bytes_be(&n),
            rsa::BigUint::from_bytes_be(&e),
        )
        .map_err(|err| Error::Token(format!("bad jwk {kid}: {err}")))?;
        keys.push((kid.to_string(), key));
    }
    *state.jwks.write().await = Some(JwksCache {
        fetched_at: OffsetDateTime::now_utc(),
        keys,
    });
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::traits::PublicKeyParts;
    use topcoat::cookie::RouterBuilderCookieExt;
    use topcoat::router::{Body, Router, to_bytes};
    use std::sync::{Arc, OnceLock};

    fn keypair() -> (rsa::RsaPrivateKey, rsa::RsaPublicKey) {
        // 2048-bit generation is slow for a unit test; 1024 is the smallest
        // rsa 0.9 accepts and the signature math under test is identical.
        let private = rsa::RsaPrivateKey::new(&mut rand_core06::OsRng, 1024).unwrap();
        let public = private.to_public_key();
        (private, public)
    }

    fn sign(claims: serde_json::Value, key: &rsa::RsaPrivateKey) -> String {
        let header = serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "test"});
        let mut out = b64url.encode(header.to_string());
        out.push('.');
        out.push_str(&b64url.encode(claims.to_string()));
        let signing = rsa::pkcs1v15::SigningKey::<sha2_for_rsa::Sha256>::new(key.clone());
        let signature = signing.sign(out.as_bytes());
        format!("{}.{}", out, b64url.encode(signature.to_bytes()))
    }

    fn good_claims() -> serde_json::Value {
        serde_json::json!({
            "iss": "http://im.test",
            "sub": "user-1",
            "aud": "client-1",
            "exp": OffsetDateTime::now_utc().unix_timestamp() + 600,
            "iat": OffsetDateTime::now_utc().unix_timestamp(),
            "email": "ann@example.com",
            "name": "Ann",
        })
    }

    #[test]
    fn valid_token_validates() {
        let (private, public) = keypair();
        let token = sign(good_claims(), &private);
        let parts: Vec<&str> = token.split('.').collect();
        let claims = verify_claims(&parts, &public, "http://im.test", "client-1").unwrap();
        assert_eq!(claims["sub"], "user-1");
    }

    #[test]
    fn tampered_payload_rejected() {
        let (private, public) = keypair();
        let token = sign(good_claims(), &private);
        let parts: Vec<&str> = token.split('.').collect();
        let forged_payload = b64url.encode(r#"{"sub":"mallory"}"#);
        let forged = vec![parts[0], forged_payload.as_str(), parts[2]];
        assert!(verify_claims(&forged, &public, "http://im.test", "client-1").is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let (private, _) = keypair();
        let (_, other_public) = keypair();
        let token = sign(good_claims(), &private);
        let parts: Vec<&str> = token.split('.').collect();
        assert!(verify_claims(&parts, &other_public, "http://im.test", "client-1").is_err());
    }

    #[test]
    fn wrong_audience_rejected() {
        let (private, public) = keypair();
        let token = sign(good_claims(), &private);
        let parts: Vec<&str> = token.split('.').collect();
        assert!(verify_claims(&parts, &public, "http://im.test", "another-app").is_err());
    }

    #[test]
    fn wrong_issuer_rejected() {
        let (private, public) = keypair();
        let token = sign(good_claims(), &private);
        let parts: Vec<&str> = token.split('.').collect();
        assert!(verify_claims(&parts, &public, "https://elsewhere.example", "client-1").is_err());
    }

    #[test]
    fn expired_rejected() {
        let (private, public) = keypair();
        let mut claims = good_claims();
        claims["exp"] = (OffsetDateTime::now_utc().unix_timestamp() - 10).into();
        let token = sign(claims, &private);
        let parts: Vec<&str> = token.split('.').collect();
        assert!(verify_claims(&parts, &public, "http://im.test", "client-1").is_err());
    }

    #[test]
    fn pkce_rfc7636_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = b64url.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn cookie_seal_roundtrip() {
        let key = [7u8; 32];
        let session = Session {
            app_session: "opaque-token".into(),
            exp: 1,
        };
        let sealed = seal_json(&key, &session);
        let back: Session = open_json(&key, &sealed).unwrap();
        assert_eq!(back.app_session, "opaque-token");
        assert!(open_json::<Session>(&[8u8; 32], &sealed).is_none());
        assert!(open_json::<Session>(&key, "garbage").is_none());
    }

    #[cfg(feature = "test-seam")]
    #[test]
    fn seam_cookie_reads_back() {
        let config = Config {
            issuer: "http://im.test".into(),
            client_id: "client-1".into(),
            client_secret: "secret".into(),
            redirect_uri: "http://app.test/auth/callback".into(),
            cookie_name: "in_session".into(),
            cookie_key: [7u8; 32],
        };
        assert_eq!(introspect_path(), "/introspect");
        let exp = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let sealed = mint_session_cookie(&config, "opaque-token", exp);
        let back: Session = open_json(&config.cookie_key, &sealed).unwrap();
        assert_eq!(back.app_session, "opaque-token");
        assert_eq!(back.exp, exp.unix_timestamp());
    }
    // -- review regressions -------------------------------------------------
    //
    // Strict `next` (CRLF injection + backslash), a `Location` that cannot
    // panic, and a `current_user` that only clears the cookie on an explicit
    // `active:false`.

    #[test]
    fn next_allows_plain_local_paths() {
        assert_eq!(safe_next("/"), "/");
        assert_eq!(safe_next("/drive/folder/a"), "/drive/folder/a");
        assert_eq!(safe_next("/?auth_error=exchange_failed"), "/?auth_error=exchange_failed");
    }

    #[test]
    fn login_next_with_crlf_injection_is_neutralized() {
        // The login half of the repro: the query arrives percent-encoded,
        // `query_value` decodes it, `safe_next` judges the decoded bytes.
        let decoded = query_value("next=/%0d%0aX-evil:1", "next").unwrap();
        assert_eq!(decoded, "/\r\nX-evil:1");
        assert_eq!(safe_next(&decoded), "/");
        assert_eq!(safe_next("/\r\nX-evil:1"), "/");
        assert_eq!(safe_next("/ok\r/evil"), "/");
        assert_eq!(safe_next("/tab\there"), "/");
    }

    #[test]
    fn next_rejects_backslash_paths() {
        // Browsers normalize `\` to `/` in `Location`, so a backslash-led
        // path is an open redirect wearing a local costume.
        assert_eq!(safe_next("/\\evil"), "/");
        assert_eq!(safe_next("/drive/\\evil"), "/");
        assert_eq!(safe_next("\\evil"), "/");
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("https://evil.example/"), "/");
        assert_eq!(safe_next(""), "/");
    }

    #[test]
    fn location_value_falls_back_to_root() {
        // `HeaderValue` refuses the injected bytes; the redirect must land
        // on `/` rather than panic its handler.
        assert!(HeaderValue::from_str("/\r\nX-evil:1").is_err());
        assert_eq!(location_value("/\r\nX-evil:1"), HeaderValue::from_static("/"));
        assert_eq!(
            location_value("/drive"),
            HeaderValue::from_str("/drive").unwrap()
        );
    }

    #[route(GET "/whoami")]
    async fn whoami(cx: &Cx) -> Result<Response, topcoat::Error> {
        match current_user(cx).await {
            Some(user) => user.email.into_response(cx),
            None => "anon".into_response(cx),
        }
    }

    fn test_config(issuer: String) -> Config {
        Config {
            issuer,
            client_id: "client-1".into(),
            client_secret: "s3cr3t".into(),
            redirect_uri: "http://app.test/auth/callback".into(),
            cookie_name: "in_session".into(),
            cookie_key: [7u8; 32],
        }
    }

    fn test_router(config: Config) -> Router {
        mount(Router::builder(), config)
            .discover_routes()
            .cookies()
            .build()
    }

    fn get(uri: &str, cookie: Option<&str>) -> http::Request<Body> {
        let mut builder = http::Request::builder().method("GET").uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(http::header::COOKIE, cookie);
        }
        builder.body(Body::empty()).unwrap()
    }

    struct TestResponse {
        status: http::StatusCode,
        location: Option<String>,
        set_cookies: Vec<String>,
        body: Vec<u8>,
    }

    impl TestResponse {
        async fn of(response: Response) -> Self {
            let status = response.status();
            let headers = response.headers().clone();
            let location = headers
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let set_cookies = headers
                .get_all(http::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_string))
                .collect();
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec();
            Self {
                status,
                location,
                set_cookies,
                body,
            }
        }

        fn text(&self) -> &str {
            std::str::from_utf8(&self.body).unwrap()
        }

        /// The session cookie was cleared iff a `Set-Cookie` names it —
        /// the test routes set no cookies of their own.
        fn clears_session(&self) -> bool {
            self.set_cookies
                .iter()
                .any(|c| c.starts_with("in_session="))
        }
    }

    fn session_cookie(token: &str) -> String {
        let session = Session {
            app_session: token.into(),
            exp: OffsetDateTime::now_utc().unix_timestamp() + 3600,
        };
        format!("in_session={}", seal_json(&[7u8; 32], &session))
    }

    /// Canned bodies served per path over plain HTTP/1.1 with keep-alive,
    /// behind locks so the test learns the URL first and fills them after.
    /// Raw strings, so a test can answer garbage on purpose.
    #[derive(Clone, Default)]
    struct CannedIm {
        token: Arc<OnceLock<String>>,
        jwks: Arc<OnceLock<String>>,
        introspect: Arc<OnceLock<String>>,
    }

    struct FakeIm {
        addr: std::net::SocketAddr,
    }

    impl FakeIm {
        async fn spawn(canned: CannedIm) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    let canned = canned.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut socket = socket;
                        let mut buf = vec![0u8; 8192];
                        let mut req = Vec::new();
                        loop {
                            let body_start = loop {
                                let Ok(n) = socket.read(&mut buf).await else {
                                    return;
                                };
                                if n == 0 {
                                    return;
                                }
                                req.extend_from_slice(&buf[..n]);
                                if let Some(end) = headers_end(&req) {
                                    let head =
                                        String::from_utf8_lossy(&req[..end]).to_string();
                                    let len = content_length(&head);
                                    if req.len() >= end + len {
                                        break end;
                                    }
                                }
                                if req.len() > 1_000_000 {
                                    return;
                                }
                            };
                            let head =
                                String::from_utf8_lossy(&req[..body_start]).to_string();
                            let first = head.lines().next().unwrap_or("").to_string();
                            let len = content_length(&head);
                            req.drain(..body_start + len);
                            let payload = if first.starts_with("POST /token ") {
                                canned.token.get().cloned().unwrap_or_default()
                            } else if first.starts_with("GET /jwks.json ") {
                                canned.jwks.get().cloned().unwrap_or_default()
                            } else {
                                canned.introspect.get().cloned().unwrap_or_default()
                            };
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                                payload.len()
                            );
                            if socket.write_all(response.as_bytes()).await.is_err() {
                                return;
                            }
                            if socket.write_all(payload.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            Self { addr }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    fn headers_end(req: &[u8]) -> Option<usize> {
        req.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|pos| pos + 4)
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0)
    }

    fn jwk_of(public: &rsa::RsaPublicKey, kid: &str) -> serde_json::Value {
        serde_json::json!({
            "kty": "RSA",
            "kid": kid,
            "n": b64url.encode(public.n().to_bytes_be()),
            "e": b64url.encode(public.e().to_bytes_be()),
        })
    }

    /// The named repro end to end: `GET /auth/login?next=/%0d%0aX-evil:1`,
    /// then a completed callback. Pre-fix the sealed `next` carried the CR
    /// LF into `see()` and the callback died (500 via the panic boundary);
    /// now the browser lands on `/`.
    #[tokio::test]
    async fn login_then_callback_with_injected_next_lands_on_root() {
        let (private, public) = keypair();
        // Login touches no im endpoint, so any issuer will do; the flight
        // cookie below is opened with the test-known key.
        let login_router = test_router(test_config("http://im.test".into()));
        let login = TestResponse::of(
            login_router
                .handle(get("/auth/login?next=/%0d%0aX-evil:1", None))
                .await,
        )
        .await;
        assert_eq!(login.status, http::StatusCode::SEE_OTHER);
        let sealed = login
            .set_cookies
            .iter()
            .find(|c| c.starts_with("in_session_pkce="))
            .expect("login sets the flight cookie");
        let sealed = sealed["in_session_pkce=".len()..]
            .split(';')
            .next()
            .unwrap();
        let flight: InFlight = open_json(&[7u8; 32], sealed).unwrap();
        // Neutralized at login: the hostile bytes never reach the seal.
        assert_eq!(flight.next, "/");

        // Now complete the dance against a fake im.
        let canned = CannedIm::default();
        let fake = FakeIm::spawn(canned.clone()).await;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let id_token = sign(
            serde_json::json!({
                "iss": fake.url(),
                "sub": "user-1",
                "aud": "client-1",
                "exp": now + 600,
                "iat": now,
                "nonce": flight.nonce,
            }),
            &private,
        );
        let _ = canned
            .jwks
            .set(serde_json::json!({"keys": [jwk_of(&public, "test")]}).to_string());
        let _ = canned
            .token
            .set(serde_json::json!({
                "id_token": id_token,
                "app_session": "tok-1",
            })
            .to_string());
        let _ = canned
            .introspect
            .set(serde_json::json!({
                "active": true,
                "sub": "user-1",
                "email": "ann@example.com",
                "name": "Ann",
                "exp": now + 3600,
            })
            .to_string());

        let router = test_router(test_config(fake.url()));
        let callback = TestResponse::of(
            router
                .handle(get(
                    &format!("/auth/callback?code=code-1&state={}", flight.state),
                    Some(&format!("in_session_pkce={sealed}")),
                ))
                .await,
        )
        .await;
        assert_eq!(callback.status, http::StatusCode::SEE_OTHER);
        assert_eq!(callback.location.as_deref(), Some("/"));
    }

    /// A flight cookie sealed before the strict rules (hand-sealed hostile
    /// here): the callback re-validates before rendering the header, so
    /// even this lands on `/` instead of panicking.
    #[tokio::test]
    async fn callback_with_hostile_stored_next_lands_on_root() {
        let (private, public) = keypair();
        let canned = CannedIm::default();
        let fake = FakeIm::spawn(canned.clone()).await;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let flight = InFlight {
            verifier: "verifier".into(),
            state: "state-1".into(),
            nonce: "nonce-1".into(),
            next: "/\r\nX-evil:1".into(),
            exp: now + 600,
        };
        let sealed = seal_json(&[7u8; 32], &flight);
        let id_token = sign(
            serde_json::json!({
                "iss": fake.url(),
                "sub": "user-1",
                "aud": "client-1",
                "exp": now + 600,
                "iat": now,
                "nonce": "nonce-1",
            }),
            &private,
        );
        let _ = canned
            .jwks
            .set(serde_json::json!({"keys": [jwk_of(&public, "test")]}).to_string());
        let _ = canned
            .token
            .set(serde_json::json!({
                "id_token": id_token,
                "app_session": "tok-1",
            })
            .to_string());
        let _ = canned
            .introspect
            .set(serde_json::json!({
                "active": true,
                "sub": "user-1",
                "email": "ann@example.com",
                "name": "Ann",
                "exp": now + 3600,
            })
            .to_string());

        let router = test_router(test_config(fake.url()));
        let callback = TestResponse::of(
            router
                .handle(get(
                    "/auth/callback?code=code-1&state=state-1",
                    Some(&format!("in_session_pkce={sealed}")),
                ))
                .await,
        )
        .await;
        assert_eq!(callback.status, http::StatusCode::SEE_OTHER);
        assert_eq!(callback.location.as_deref(), Some("/"));
    }

    #[tokio::test]
    async fn current_user_keeps_cookie_when_im_is_blackholed() {
        // Nothing listens: the POST fails, the request reads as signed-out,
        // and the cookie survives for the next try.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let router = test_router(test_config(format!("http://127.0.0.1:{port}")));
        let res = TestResponse::of(
            router
                .handle(get("/whoami", Some(&session_cookie("tok-1"))))
                .await,
        )
        .await;
        assert_eq!(res.text(), "anon");
        assert!(
            !res.clears_session(),
            "a blackholed introspection must not clear the cookie: {:?}",
            res.set_cookies
        );
    }

    #[tokio::test]
    async fn current_user_keeps_cookie_on_unparseable_introspection() {
        let canned = CannedIm::default();
        let _ = canned.introspect.set("not-json{{{".into());
        let fake = FakeIm::spawn(canned).await;
        let router = test_router(test_config(fake.url()));
        let res = TestResponse::of(
            router
                .handle(get("/whoami", Some(&session_cookie("tok-1"))))
                .await,
        )
        .await;
        assert_eq!(res.text(), "anon");
        assert!(
            !res.clears_session(),
            "an unparseable introspection must not clear the cookie: {:?}",
            res.set_cookies
        );
    }

    #[tokio::test]
    async fn current_user_keeps_cookie_when_active_is_missing() {
        // A malformed answer is not a revocation.
        let canned = CannedIm::default();
        let _ = canned
            .introspect
            .set(serde_json::json!({"sub": "user-1"}).to_string());
        let fake = FakeIm::spawn(canned).await;
        let router = test_router(test_config(fake.url()));
        let res = TestResponse::of(
            router
                .handle(get("/whoami", Some(&session_cookie("tok-1"))))
                .await,
        )
        .await;
        assert_eq!(res.text(), "anon");
        assert!(
            !res.clears_session(),
            "an answer without `active` must not clear the cookie: {:?}",
            res.set_cookies
        );
    }

    #[tokio::test]
    async fn current_user_clears_cookie_when_session_revoked() {
        let canned = CannedIm::default();
        let _ = canned
            .introspect
            .set(serde_json::json!({"active": false}).to_string());
        let fake = FakeIm::spawn(canned).await;
        let router = test_router(test_config(fake.url()));
        let res = TestResponse::of(
            router
                .handle(get("/whoami", Some(&session_cookie("tok-1"))))
                .await,
        )
        .await;
        assert_eq!(res.text(), "anon");
        assert!(
            res.clears_session(),
            "an `active:false` answer must clear the cookie: {:?}",
            res.set_cookies
        );
    }

    #[tokio::test]
    async fn current_user_serves_the_introspected_user() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let canned = CannedIm::default();
        let _ = canned
            .introspect
            .set(serde_json::json!({
                "active": true,
                "sub": "user-1",
                "email": "ann@example.com",
                "name": "Ann",
                "exp": now + 3600,
            })
            .to_string());
        let fake = FakeIm::spawn(canned).await;
        let router = test_router(test_config(fake.url()));
        let res = TestResponse::of(
            router
                .handle(get("/whoami", Some(&session_cookie("tok-1"))))
                .await,
        )
        .await;
        assert_eq!(res.text(), "ann@example.com");
        assert!(
            !res.clears_session(),
            "a live session must not clear the cookie: {:?}",
            res.set_cookies
        );
    }
}
