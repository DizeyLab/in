//! Server-side plumbing: the application context, the person behind the
//! current request, and the role guards.
//!
//! Every guard here is the real one. The UI hides what a role may not do, but
//! the answer that matters is the one given in this module, on the server.
//!
//! There are no local passwords in v1 — im holds the central session over
//! OIDC and `in-client` (vendored from `im-client`) holds this app's side of
//! it. This module's `current_user` maps im's claims onto the local user row,
//! provisioning it on first sight.

use std::sync::Arc;

use in_core::Config;
use in_core::store::{Store, StoreError, User};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use topcoat::asset::AssetBundle;
use topcoat::context::{Cx, app_context, memoize};
use topcoat::router::request::headers;
use topcoat::router::{Body, HeaderValue, Next, StatusCode, header, response::Response, to_bytes};

/// The whole application context, put into the router by `main.rs`. One
/// struct rather than three separate contexts so a route needing two of them
/// reads both off the same value. The OIDC client state lives in the router
/// separately — `in_client::mount` puts it there — because its login routes
/// read it without going through this struct.
#[derive(Clone)]
pub struct App {
    /// The drive store. `Arc` over the trait so tests can hand a router a
    /// rehearsal double without touching Turso.
    pub store: Arc<dyn Store>,
    /// The loaded `config/in.toml` — every tunable wave 2 reads.
    pub config: Config,
    /// Told when the process is stopping, so the live streams end instead of
    /// being waited out.
    pub shutdown: crate::live::Shutdown,
}

/// The application context this request runs under.
pub fn app(cx: &Cx) -> App {
    app_context::<App>(cx).clone()
}

/// A stable-enough label for the client, for rate limiting. A proxy header is
/// only trusted because In is meant to sit behind one; the address bucket is
/// the limit that actually protects the login round-trip either way.
///
/// topcoat 0.6.2 exposes no peer address; x-forwarded-for or nothing.
pub fn client_label(cx: &Cx) -> String {
    let Some(forwarded) = headers(cx).get("x-forwarded-for") else {
        return "unknown".to_string();
    };
    let Ok(raw) = forwarded.to_str() else {
        return "unknown".to_string();
    };
    match raw.split(',').next() {
        Some(first) if !first.trim().is_empty() => first.trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// The stylesheet this binary serves must be the one compiled into it.
///
/// `asset!` embeds an asset's declaration — its id and source path — into
/// the binary, but the served bytes live in the bundle directory beside the
/// executable, and topcoat loads whatever manifest it finds there. A bundle
/// left behind by another deploy sits beside a newer binary without a word
/// of complaint, and the pages then reference a stylesheet whose bytes are
/// from another generation — the mixed generation a browser once caught on
/// production. `build.rs` stamps the compiled stylesheet's SHA-256 into the
/// binary; this hashes the bundle's bytes against it, so a foreign bundle
/// refuses the boot instead of serving under it.
///
/// Returns the startup log line naming the served fingerprint, or the
/// reason the boot must not proceed.
pub fn stylesheet_guard(bundle: &AssetBundle) -> Result<String, String> {
    let expected = env!("IN_STYLE_FINGERPRINT");
    let stylesheet = bundle
        .catalog()
        .assets()
        .find(|asset| {
            let name = asset.name();
            name.starts_with("main-") && name.ends_with(".css")
        })
        .ok_or_else(|| {
            format!(
                "the asset bundle at {} carries no stylesheet",
                bundle.dir().display()
            )
        })?;
    let bytes = std::fs::read(bundle.dir().join(stylesheet.name())).map_err(|err| {
        format!(
            "the bundled stylesheet {} could not be read: {err}",
            stylesheet.name()
        )
    })?;
    let actual = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!(
            "the asset bundle at {} is from another build: stylesheet {} is {actual} but this binary was compiled against {expected}; run `topcoat asset bundle` and redeploy",
            bundle.dir().display(),
            stylesheet.name()
        ));
    }
    Ok(format!(
        "assets  stylesheet {} ({actual})",
        stylesheet.name()
    ))
}

/// The person behind this request, or nobody, or the store failing to say.
///
/// im is asked on every request — the cookie holds an opaque session token,
/// introspected per call — so an admin revoking the person in im signs them
/// out here at once. The first sight of a `sub` inserts the local row (and
/// the first user ever becomes the admin); every later sight refreshes the
/// address and name and stamps the last-seen time.
///
/// A store error is not "nobody" — a busy database mid-drag is a fact about
/// the database, not about whether this browser is signed in, and a caller
/// that folded the two together would send a signed-in person to a sign-in
/// screen because a write elsewhere held a lock for a moment.
///
/// A disabled account reads as nobody: the cookie is im's business and is
/// left untouched, but every guard below refuses as if signed out.
#[memoize(as_ref)]
pub async fn current_user(cx: &Cx) -> Result<Option<User>, StoreError> {
    let Some(claims) = in_client::current_user(cx).await else {
        return Ok(None);
    };
    let app = app(cx);
    let user = app
        .store
        .provision_user(
            &claims.sub,
            &claims.email,
            &claims.name,
            app.config.default_quota_bytes,
        )
        .await?;
    if user.disabled {
        return Ok(None);
    }
    Ok(Some(user))
}

/// The person behind this request, or a refusal the caller can return as-is.
///
/// A store error becomes `Refusal::Unavailable` rather than `SignInFirst`,
/// which would tell a signed-in person to sign in again over what was only
/// a database hiccup.
pub async fn require_user(cx: &Cx) -> Result<User, Refusal> {
    match current_user(cx).await {
        Ok(Some(user)) => Ok(user.clone()),
        Ok(None) => Err(Refusal::SignInFirst),
        Err(_) => Err(Refusal::Unavailable),
    }
}

/// The admin behind this request. Anyone else is refused *here*, not
/// merely hidden from in the UI.
pub async fn require_admin(cx: &Cx) -> Result<User, Refusal> {
    let user = require_user(cx).await?;
    if user.admin {
        Ok(user)
    } else {
        Err(Refusal::Forbidden)
    }
}

impl From<StoreError> for Refusal {
    fn from(_: StoreError) -> Refusal {
        Refusal::Unavailable
    }
}

/// Everything a refused call is allowed to say.
///
/// The codes are the contract's: a browser without script only ever sees
/// them through the address bar, so each one has a short word form
/// ([`Refusal::code`]) beside its sentence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// No session, a revoked one, or a disabled account — deliberately one
    /// answer for all three.
    SignInFirst,
    /// Signed in, but not an admin (or not the owner). Cross-owner reads are
    /// answered with [`Refusal::NotFound`] instead — a 404, never a 403 —
    /// so this is only for surfaces whose existence is no secret.
    Forbidden,
    /// No such file or folder — or none this account may see. Deliberately
    /// one answer for both.
    NotFound,
    /// The bytes would not fit the remaining quota.
    QuotaExceeded,
    /// A sibling already carries that name.
    NameTaken,
    /// A folder with children stays until it is emptied.
    FolderNotEmpty,
    /// The target belongs to somebody else. Answered like [`Refusal::NotFound`]
    /// on the wire; a distinct code so the carry still names what happened.
    CrossOwner,
    /// A share link spent, expired, revoked, or never real.
    ShareRevoked,
    /// The upload session expired or was aborted before the finish landed.
    UploadExpired,
    /// A chunk arrived out of order, twice, or the wrong size.
    BadChunk,
    /// A restore was asked for while an ancestor folder is still trashed —
    /// restore the ancestor first.
    AncestorTrashed,
    /// The store failed to say. Never a sign-in prompt — see `require_user`.
    Unavailable,
}

impl Refusal {
    /// The refusal in words.
    pub fn message(&self) -> String {
        match self {
            Refusal::SignInFirst => "Sign in first.".to_string(),
            Refusal::Forbidden => "Not permitted.".to_string(),
            Refusal::NotFound => "No such file or folder.".to_string(),
            Refusal::QuotaExceeded => "Over quota.".to_string(),
            Refusal::NameTaken => "That name is already taken here.".to_string(),
            Refusal::FolderNotEmpty => "That folder still has files in it.".to_string(),
            // Same sentence as NotFound: a cross-owner probe must not learn
            // the target exists.
            Refusal::CrossOwner => "No such file or folder.".to_string(),
            Refusal::ShareRevoked => "This link no longer works.".to_string(),
            Refusal::UploadExpired => "This upload expired — start it again.".to_string(),
            Refusal::BadChunk => "That piece arrived damaged — send it again.".to_string(),
            Refusal::AncestorTrashed => "Restore the folder it is in first.".to_string(),
            Refusal::Unavailable => "Something went wrong.".to_string(),
        }
    }

    /// `message()`, in the reader's language.
    pub fn message_in(&self, lang: crate::i18n::Lang) -> String {
        use crate::i18n::Lang::Tr;
        if lang != Tr {
            return self.message();
        }
        match self {
            Refusal::SignInFirst => "Önce oturum aç.".to_string(),
            Refusal::Forbidden => "İzin verilmiyor.".to_string(),
            Refusal::NotFound => "Böyle bir dosya ya da klasör yok.".to_string(),
            Refusal::QuotaExceeded => "Kota doldu.".to_string(),
            Refusal::NameTaken => "Bu ad burada zaten var.".to_string(),
            Refusal::FolderNotEmpty => "Bu klasörde hâlâ dosyalar var.".to_string(),
            Refusal::CrossOwner => "Böyle bir dosya ya da klasör yok.".to_string(),
            Refusal::ShareRevoked => "Bu bağlantı artık çalışmıyor.".to_string(),
            Refusal::UploadExpired => "Bu yüklemenin süresi doldu — yeniden başlat.".to_string(),
            Refusal::BadChunk => "Bu parça bozuk geldi — yeniden gönder.".to_string(),
            Refusal::AncestorTrashed => "Önce içinde olduğu klasörü geri yükle.".to_string(),
            Refusal::Unavailable => "Bir şeyler ters gitti.".to_string(),
        }
    }

    /// The refusal a `code` names, or nothing. Nothing for an unknown word: the
    /// query is whatever the address bar holds, so a code that is not one of
    /// ours says nothing at all rather than something invented.
    pub fn from_code(code: &str) -> Option<Refusal> {
        Some(match code {
            "sign-in-first" => Refusal::SignInFirst,
            "forbidden" => Refusal::Forbidden,
            "not-found" => Refusal::NotFound,
            "quota-exceeded" => Refusal::QuotaExceeded,
            "name-taken" => Refusal::NameTaken,
            "folder-not-empty" => Refusal::FolderNotEmpty,
            "cross-owner" => Refusal::CrossOwner,
            "share-revoked" => Refusal::ShareRevoked,
            "upload-expired" => Refusal::UploadExpired,
            "bad-chunk" => Refusal::BadChunk,
            "ancestor-trashed" => Refusal::AncestorTrashed,
            "unavailable" => Refusal::Unavailable,
            _ => return None,
        })
    }

    /// The refusal as a short word, for the address bar.
    ///
    /// A browser without script never sees a call's return value: it posts the
    /// form, follows the redirect, and the page it lands on has to be told what
    /// happened. That telling goes through the query, so every refusal needs a
    /// name that survives a round trip through a URL.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::SignInFirst => "sign-in-first",
            Refusal::Forbidden => "forbidden",
            Refusal::NotFound => "not-found",
            Refusal::QuotaExceeded => "quota-exceeded",
            Refusal::NameTaken => "name-taken",
            Refusal::FolderNotEmpty => "folder-not-empty",
            Refusal::CrossOwner => "cross-owner",
            Refusal::ShareRevoked => "share-revoked",
            Refusal::UploadExpired => "upload-expired",
            Refusal::BadChunk => "bad-chunk",
            Refusal::AncestorTrashed => "ancestor-trashed",
            Refusal::Unavailable => "unavailable",
        }
    }
}

/// Which call a carried refusal belongs to. Two forms on one page must not both
/// claim the same sentence, so the redirect names the call and this is the name:
/// the last piece of the server function's path, with anything that is not a
/// plain word dropped so it can only ever be compared, never rendered.
pub fn call_id(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// What a call refused with, however the browser without script carried it
/// back — read straight off the query, since there is no client-side action
/// value to check first the way a hydrated page in the old UI had.
///
/// Every topcoat page is rendered server-side on every request, so there is
/// only ever the query to read.
pub fn refusal_of(cx: &Cx, call: &str) -> Option<Refusal> {
    let query = topcoat::router::request::uri(cx).query()?;
    let mut code = None;
    let mut on = None;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "refusal" => code = Some(value),
                "on" => on = Some(value),
                _ => {}
            }
        }
    }
    if on? != call {
        return None;
    }
    Refusal::from_code(code?)
}

/// The page a form was posted from: the `Referer`, with the answer any
/// earlier post left on its query dropped, or `nowhere` when no `Referer`
/// came with the request.
///
/// The feedback pairs — `refusal=`, `on=`, `why=`, `saved=` — are how a page
/// renders what the last post did. Sending the browser back with them still
/// on the query re-renders that old answer under the new post's own: a change
/// that succeeded right after one that was refused announces the refusal
/// again, and reads as having failed. The answer this redirect carries — the
/// pairs it was built with, or the body [`carry_refusal_on_redirect`] copies
/// onto the query — is the one that shows; an earlier one never survives the
/// trip.
pub fn back_to(cx: &Cx, nowhere: &str) -> String {
    let referer = headers(cx)
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(nowhere);
    let (path, query) = referer.split_once('?').unwrap_or((referer, ""));
    let pairs: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            !pair.is_empty()
                && !["refusal", "on", "why", "saved"].iter().any(|key| {
                    pair.strip_prefix(*key)
                        .is_some_and(|rest| rest.starts_with('='))
                })
        })
        .collect();
    if pairs.is_empty() {
        return path.to_string();
    }
    format!("{path}?{}", pairs.join("&"))
}

/// Puts a refusal on the redirect a browser without script follows.
///
/// A hydrated page reads the call's return value straight off the action. A
/// browser without script has no such thing: it posts the form, the server
/// function handler answers with a redirect back to the page it came from, and
/// the value — the whole refusal — sits in a body nobody will ever look at.
/// The click then looks like nothing happening, which is the worst answer
/// In can give.
///
/// So the refusal is copied onto the `Location`, as `?refusal=<code>&on=<call>`,
/// and the page renders it from the query. This is one place rather than a
/// dozen because the shape is the same for every refusing call, present
/// and future: nothing here knows what any of them do.
///
/// Requests carrying script are untouched — they are answered with the value
/// itself and never see a redirect.
///
/// Ported from axum's `middleware::from_fn` onto a topcoat `#[layer]`. topcoat's
/// Post/Redirect/Get helper (`error::see_other`) answers with `303 See Other`,
/// not the `302 Found` axum's server-function redirect used, so the status this
/// checks for is `SEE_OTHER` — the guard's three conditions are otherwise
/// unchanged.
#[topcoat::router::layer("/api")]
async fn carry_refusal_on_redirect(
    cx: &Cx,
    body: Body,
    next: Next<'_>,
) -> topcoat::Result<Response> {
    // A form post from a browser asks for a page back. A server-function call
    // from the hydrated bundle does not.
    let wants_page = headers(cx)
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    let called = call_id(topcoat::router::request::uri(cx).path());
    let has_referer = headers(cx).contains_key(header::REFERER);
    let response = next.run(cx, body).await?;
    if !wants_page || !has_referer || response.status() != StatusCode::SEE_OTHER {
        return Ok(response);
    }

    let (mut parts, body) = response.into_parts();
    // The body of one of these redirects is a serialised `Option<Refusal>` and
    // nothing else; the cap is there so a response that is something else
    // entirely cannot be read into memory whole. A body that fails to parse —
    // an empty one included, the shape a route with nothing to say back sends
    // — is read as "no refusal", never as "leave the Location alone": the
    // Referer sanitization below has to run on every redirect this layer
    // sees, not only the ones that happen to carry a refusal.
    let Ok(bytes) = to_bytes(body, 64 * 1024).await else {
        return Ok(Response::from_parts(parts, Body::empty()));
    };
    let refusal = serde_json::from_slice::<Option<Refusal>>(&bytes)
        .ok()
        .flatten();
    if let Some(location) = parts
        .headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
    {
        let rewritten = match refusal {
            Some(refusal) => carrying(location, refusal.code(), &called),
            None => Some(same_origin(location).to_string()),
        };
        if let Some(carried) = rewritten
            && let Ok(value) = HeaderValue::from_str(&carried)
        {
            parts.headers.insert(header::LOCATION, value);
        }
    }
    Ok(Response::from_parts(parts, Body::from(bytes)))
}

/// `location` with the refusal in its query.
///
/// The redirect goes back to the page the form was posted from, and that page
/// may already carry a query — `?folder=<id>` is how a browser without script
/// opens a folder at all — so the two pairs are merged in, and the pair from
/// any earlier refusal is dropped rather than stacked on top of.
fn carrying(location: &str, code: &str, called: &str) -> Option<String> {
    if called.is_empty() {
        return None;
    }
    // The Location we are rewriting came from the form post's Referer, and on a
    // cross-origin post the Referer is whatever the other site is. Sending the
    // browser back there would make In an open redirect, so the address is
    // rebuilt from its path and query alone and anything that is not a plain
    // absolute path is answered with the drive.
    let here = same_origin(location);
    let (path, query) = match here.split_once('?') {
        Some((path, query)) => (path, query),
        None => (here, ""),
    };
    let mut pairs: Vec<String> = query
        .split('&')
        .filter(|pair| {
            !pair.is_empty() && !pair.starts_with("refusal=") && !pair.starts_with("on=")
        })
        .map(str::to_string)
        .collect();
    pairs.push(format!("refusal={code}&on={called}"));
    Some(format!("{path}?{}", pairs.join("&")))
}

/// The path and query of `location`, with scheme and authority dropped. A
/// protocol-relative address (`//elsewhere.example/`) is another host wearing a
/// path's clothes, and a browser reads a backslash there as a slash, so both
/// are answered with the drive rather than trusted.
fn same_origin(location: &str) -> &str {
    let rest = match location.split_once("://") {
        Some((_scheme, rest)) => match rest.find(['/', '?']) {
            Some(at) => &rest[at..],
            None => "/",
        },
        None => location,
    };
    let mut characters = rest.chars();
    match (characters.next(), characters.next()) {
        (Some('/'), Some('/' | '\\')) => "/",
        (Some('/'), _) => rest,
        _ => "/",
    }
}

/// The cache directives a response cannot choose for itself.
///
/// HTML is revalidated on every load: a page the browser kept from before a
/// deploy is the old app answering under the new one's address, which is how a
/// fixed bug once came back after shipping. So every response whose body is a
/// document gets `no-cache`. Responses that already carry a directive keep it —
/// topcoat stamps the fingerprinted assets under `/_topcoat/assets` with a year
/// of `immutable` and the live stream with `no-cache`, and the file and
/// thumbnail handlers stamp their own — and everything else (redirects, JSON
/// answers) ships none, the idiom they already ride on: no directive, nothing
/// cached. One layer at `/`, because the decision is about what the bytes are,
/// not which route built them.
#[topcoat::router::layer("/")]
async fn cache_directives(cx: &Cx, body: Body, next: Next<'_>) -> topcoat::Result<Response> {
    let response = next.run(cx, body).await?;
    let (mut parts, body) = response.into_parts();
    let is_html = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    if !is_html || parts.headers.contains_key(header::CACHE_CONTROL) {
        return Ok(Response::from_parts(parts, body));
    }
    parts
        .headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(Response::from_parts(parts, body))
}
