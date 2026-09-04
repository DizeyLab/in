//! Sharing: public links, per-user grants, and the shared-with-me page.
//!
//! `POST /api/share/link/create|revoke` mints and kills bearer links over a
//! file or folder (`can_download`, optional expiry in days; only the token
//! hash is stored). `POST /api/share/user/add|remove` grants and revokes
//! named users the same way. `GET /s/{token}` is public — no auth —
//! rendering the viewer page, the download only when `can_download` allows,
//! and the stored webp preview on `?thumb=1` regardless of the flag; a spent,
//! expired or revoked token answers the dead card, never a stack.
//! `GET /shared` lists what others shared with the reader.
//!
//! Every mutation answers the way `board.rs` in iz does: a 303 back to the
//! page the form was posted from, the refusal (if any) on the redirect's
//! query. The one exception is link creation, whose plaintext token exists
//! only at creation: the redirect carries it once as `?created=<token>` so
//! a browser without script can copy it, and the drive and settings pages
//! render the copy-once banner off that pair.

use in_core::hash_share_token;
use in_core::store::{ShareKind, Store, StoreError, User};
use serde::Deserialize;
use time::OffsetDateTime;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::request::{headers as request_headers, uri};
use topcoat::router::response::IntoResponse;
use topcoat::router::{HeaderName, StatusCode, header, page, path_param, route};
use topcoat::view::view;

use crate::i18n::{Key, Lang, lang, t};
use crate::layout::{NavPage, topbar, wordmark};
use crate::server::{Refusal, app, back_to, require_user};

path_param!(token);

/// A refusal surfaced as a banner: the `?refusal=<code>&on=<call>` pair the
/// mutation redirects carry, rendered only when `on` names one of `calls` —
/// and the `?saved=<call>` chip a clean save carries, the way iz's
/// `saved_or_refused` marks one. Shared by the trash and settings pages,
/// which own no banner of their own.
pub(crate) async fn refusal_banner(cx: &Cx, language: Lang, calls: &[&str]) -> Result {
    let query = uri(cx).query().unwrap_or("");
    let refusal = query_value(query, "refusal").and_then(|code| Refusal::from_code(&code));
    let on = query_value(query, "on").unwrap_or_default();
    let saved = query_value(query, "saved").unwrap_or_default();
    let refused = calls.iter().any(|call| *call == on);
    let kept = calls.iter().any(|call| *call == saved);
    view! {
        cx =>
        if refused {
            if let Some(refusal) = refusal {
                <p class="field-error" role="alert">(refusal.message_in(language))</p>
            }
        }
        if kept {
            <p class="field-note" role="status">(t(language, Key::Saved))</p>
        }
    }
}

/// The value of one query pair, if present.
fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_string())
    })
}

type Redirect = Result<(StatusCode, [(HeaderName, String); 1])>;

/// Back to the posting page, the refusal (if any) on the query.
fn redirect_back(cx: &Cx, nowhere: &str, call: &str, refusal: Option<Refusal>) -> Redirect {
    let back = back_to(cx, nowhere);
    let separator = if back.contains('?') { '&' } else { '?' };
    let location = match refusal {
        Some(refusal) => format!("{back}{separator}refusal={}&on={call}", refusal.code()),
        None => back,
    };
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]))
}

/// The store's failure in the route's words. Cross-owner reads answer as
/// not-found — a 404, never a 403 — so a stranger cannot probe which ids
/// exist.
pub(crate) fn refusal_of(error: StoreError) -> Refusal {
    match error {
        StoreError::NotFound | StoreError::CrossOwner => Refusal::NotFound,
        StoreError::NameTaken => Refusal::NameTaken,
        StoreError::QuotaExceeded => Refusal::QuotaExceeded,
        StoreError::AncestorTrashed => Refusal::AncestorTrashed,
        _ => Refusal::Unavailable,
    }
}

/// `file` or `folder`, or nothing a route may act on.
fn parse_kind(raw: &str) -> Option<ShareKind> {
    match raw {
        "file" => Some(ShareKind::File),
        "folder" => Some(ShareKind::Folder),
        _ => None,
    }
}

/// A checkbox-ish flag: absent means the control's default, present is true
/// unless it reads plainly false.
fn parse_flag(raw: Option<&str>, when_absent: bool) -> bool {
    match raw {
        None => when_absent,
        Some(value) => !matches!(
            value.trim().to_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
    }
}
/// The link's expiry off `expires_in_days`: absent or empty means no expiry,
/// anything else must name a positive whole number of days. A mistyped value
/// silently minting a never-expiring link would be the wrong default, so it
/// is refused instead.
fn parse_expiry(raw: Option<&str>) -> std::result::Result<Option<OffsetDateTime>, Refusal> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let days: i64 = raw.parse().map_err(|_| Refusal::Forbidden)?;
    if days <= 0 {
        return Err(Refusal::Forbidden);
    }
    let seconds = days.checked_mul(86_400).ok_or(Refusal::Forbidden)?;
    OffsetDateTime::now_utc()
        .checked_add(time::Duration::seconds(seconds))
        .map(Some)
        .ok_or(Refusal::Forbidden)
}

/// The target the caller must own: present, untrashed, and theirs. Anything
/// else is not-found — a stranger learns nothing about whose it is.
async fn owned_target(
    store: &dyn Store,
    user: &User,
    kind: ShareKind,
    target_id: &str,
) -> std::result::Result<(), Refusal> {
    let owned = match kind {
        ShareKind::File => store
            .file(target_id)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .is_some_and(|file| file.owner_id == user.id && file.deleted_at.is_none()),
        ShareKind::Folder => store
            .folder(target_id)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .is_some_and(|folder| folder.owner_id == user.id && folder.deleted_at.is_none()),
    };
    if owned { Ok(()) } else { Err(Refusal::NotFound) }
}
/// The target the caller owns, trashed or not: unsharing a trashed target
/// still revokes the grant, and the grant row outlives the trash either way.
/// Anything else is not-found — a stranger learns nothing about whose it is.
async fn owned_target_for_unshare(
    store: &dyn Store,
    user: &User,
    kind: ShareKind,
    target_id: &str,
) -> std::result::Result<(), Refusal> {
    let owned = match kind {
        ShareKind::File => store
            .file(target_id)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .is_some_and(|file| file.owner_id == user.id),
        ShareKind::Folder => store
            .folder(target_id)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .is_some_and(|folder| folder.owner_id == user.id),
    };
    if owned { Ok(()) } else { Err(Refusal::NotFound) }
}

#[derive(Deserialize)]
struct CreateLinkForm {
    kind: String,
    target_id: String,
    #[serde(default)]
    can_download: Option<String>,
    #[serde(default)]
    expires_in_days: Option<String>,
}

/// Mints a bearer link. The token is shown once — in the redirect's
/// `?created=` pair, which the drive and settings pages render as the
/// copy-once banner — and never again; only its hash is stored.
#[route(POST "/api/share/link/create")]
async fn create_link(cx: &Cx, Form(input): Form<CreateLinkForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "/settings", "create", Some(refusal)),
    };
    let Some(kind) = parse_kind(&input.kind) else {
        return redirect_back(cx, "/settings", "create", Some(Refusal::NotFound));
    };
    if let Err(refusal) = owned_target(app(cx).store.as_ref(), &user, kind, &input.target_id).await
    {
        return redirect_back(cx, "/settings", "create", Some(refusal));
    }
    let expires_at = match parse_expiry(input.expires_in_days.as_deref()) {
        Ok(expires_at) => expires_at,
        Err(refusal) => return redirect_back(cx, "/settings", "create", Some(refusal)),
    };
    let created = app(cx)
        .store
        .create_share_link(
            &user.id,
            kind,
            &input.target_id,
            parse_flag(input.can_download.as_deref(), true),
            expires_at,
        )
        .await;
    match created {
        Ok(link) => {
            let back = back_to(cx, "/settings");
            let separator = if back.contains('?') { '&' } else { '?' };
            let location = format!("{back}{separator}created={}", link.token);
            Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]))
        }
        Err(error) => redirect_back(cx, "/settings", "create", Some(refusal_of(error))),
    }
}

#[derive(Deserialize)]
struct RevokeLinkForm {
    id: String,
}

/// Kills a bearer link. Only its creator may: anyone else is answered as if
/// it never existed. Revoking twice is not an error.
#[route(POST "/api/share/link/revoke")]
async fn revoke_link(cx: &Cx, Form(input): Form<RevokeLinkForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "/settings", "revoke", Some(refusal)),
    };
    let store = app(cx).store;
    let mine = store
        .share_links(&user.id)
        .await
        .map_err(|_| Refusal::Unavailable);
    let Ok(links) = mine else {
        return redirect_back(cx, "/settings", "revoke", Some(Refusal::Unavailable));
    };
    if !links
        .iter()
        .any(|link| link.id == input.id && link.created_by == user.id)
    {
        return redirect_back(cx, "/settings", "revoke", Some(Refusal::NotFound));
    }
    match store.revoke_share_link(&input.id).await {
        Ok(()) => redirect_back(cx, "/settings", "revoke", None),
        Err(error) => redirect_back(cx, "/settings", "revoke", Some(refusal_of(error))),
    }
}

/// Whether `at` is the link's target or sits under it: every step up must be
/// live, owned by the link's owner, and end at the target. A break anywhere
/// means the browse is outside the share.
async fn under_target(store: &dyn Store, owner_id: &str, target_id: &str, at: &str) -> bool {
    let mut current = at.to_string();
    loop {
        if current == target_id {
            return true;
        }
        let Ok(Some(folder)) = store.folder(&current).await else {
            return false;
        };
        if folder.owner_id != owner_id || folder.deleted_at.is_some() {
            return false;
        }
        match folder.parent_id {
            Some(parent) if parent != current => current = parent,
            _ => return false,
        }
    }
}
#[derive(Deserialize)]
struct ShareUserForm {
    kind: String,
    target_id: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    can_download: Option<String>,
}

/// Shares one file or folder with one named person. The address is folded to
/// lowercase before lookup; an unknown address is not-found.
#[route(POST "/api/share/user/add")]
async fn add_share(cx: &Cx, Form(input): Form<ShareUserForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "/settings", "add", Some(refusal)),
    };
    let Some(kind) = parse_kind(&input.kind) else {
        return redirect_back(cx, "/settings", "add", Some(Refusal::NotFound));
    };
    if let Err(refusal) = owned_target(app(cx).store.as_ref(), &user, kind, &input.target_id).await
    {
        return redirect_back(cx, "/settings", "add", Some(refusal));
    }
    let store = app(cx).store;
    let grantee = store
        .user_by_email(&input.email.trim().to_lowercase())
        .await
        .map_err(|_| Refusal::Unavailable);
    let Ok(Some(grantee)) = grantee else {
        return redirect_back(cx, "/settings", "add", Some(Refusal::NotFound));
    };
    match store
        .add_share_user(
            &user.id,
            kind,
            &input.target_id,
            &grantee.id,
            parse_flag(input.can_download.as_deref(), true),
        )
        .await
    {
        Ok(()) => redirect_back(cx, "/settings", "add", None),
        Err(error) => redirect_back(cx, "/settings", "add", Some(refusal_of(error))),
    }
}

/// Unshares. Removing what was never shared is not an error; only the
/// target's owner may unshare, trashed or not.
#[route(POST "/api/share/user/remove")]
async fn remove_share(cx: &Cx, Form(input): Form<ShareUserForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "/settings", "remove", Some(refusal)),
    };
    let Some(kind) = parse_kind(&input.kind) else {
        return redirect_back(cx, "/settings", "remove", Some(Refusal::NotFound));
    };
    if let Err(refusal) = owned_target_for_unshare(app(cx).store.as_ref(), &user, kind, &input.target_id).await
    {
        return redirect_back(cx, "/settings", "remove", Some(refusal));
    }
    let store = app(cx).store;
    let grantee = store
        .user_by_email(&input.email.trim().to_lowercase())
        .await
        .map_err(|_| Refusal::Unavailable);
    let Ok(Some(grantee)) = grantee else {
        return redirect_back(cx, "/settings", "remove", Some(Refusal::NotFound));
    };
    match store
        .remove_share_user(kind, &input.target_id, &grantee.id)
        .await
    {
        Ok(()) => redirect_back(cx, "/settings", "remove", None),
        Err(error) => redirect_back(cx, "/settings", "remove", Some(refusal_of(error))),
    }
}

/// The public viewer's query: which folder is browsed, which file is named,
/// whether the bytes (rather than the card) are wanted, and whether the
/// stored webp preview is wanted — the preview a view-only link grants.
struct SharedQuery {
    folder: Option<String>,
    file: Option<String>,
    dl: bool,
    thumb: bool,
}

/// The query off the request's own URI. Unparseable pairs are ignored —
/// a hand-edited query browses nothing rather than failing the page.
fn shared_query(cx: &Cx) -> SharedQuery {
    let query = uri(cx).query().unwrap_or("");
    SharedQuery {
        folder: query_value(query, "folder"),
        file: query_value(query, "file"),
        dl: has_flag(query, "dl"),
        thumb: has_flag(query, "thumb"),
    }
}

/// Whether the query names the bare flag, as `?dl` or `?dl=1`.
fn has_flag(query: &str, key: &str) -> bool {
    query
        .split('&')
        .any(|pair| pair == key || pair.starts_with(&format!("{key}=")))
}

/// The dead card: a spent, expired, revoked or never-real token, a trashed
/// target, or a download the link may not open. One answer for all of them —
/// a stranger learns nothing about which tokens exist.
async fn dead_link(cx: &Cx) -> Result {
    let language = lang(cx).await;
    view! {
        cx =>
        <main class="scaffold-note">
            (wordmark(cx).await?)
            <p>(Refusal::ShareRevoked.message_in(language))</p>
            <p><a href="/">(t(language, Key::BackToDrive))</a></p>
        </main>
    }
}

/// The public viewer. No auth: the token in the path is the whole
/// credential. A file target renders its card (and its bytes on `?dl=1`
/// when the link may download); a folder target renders the listing of the
/// browsed folder, downloads gated per file on the same flag.
#[route(GET "/s/{token}")]
async fn shared_link(cx: &Cx) -> topcoat::Result<topcoat::router::response::Response> {
    let token: &str = path_param::<Token>(cx);
    let store = app(cx).store;
    let now = OffsetDateTime::now_utc();
    let link = store
        .resolve_share_link(&hash_share_token(token), now)
        .await
        .ok()
        .flatten();
    let Some(link) = link else {
        return dead_link(cx).await.into_response(cx);
    };
    let query = shared_query(cx);
    match link.kind {
        ShareKind::File => {
            let file = store
                .file(&link.target_id)
                .await
                .map_err(|_| Refusal::Unavailable);
            let Ok(Some(file)) = file else {
                return dead_link(cx).await.into_response(cx);
            };
            if file.deleted_at.is_some() {
                return dead_link(cx).await.into_response(cx);
            }
            if query.dl {
                return download_bytes(cx, store.as_ref(), &file.id, &file.name, &file.mime, link.can_download).await;
            }
            if query.thumb {
                return public_thumb(cx, store.as_ref(), &file.id).await;
            }
            file_card(cx, &link, &file).await
        }
        ShareKind::Folder => {
            let root = store
                .folder(&link.target_id)
                .await
                .map_err(|_| Refusal::Unavailable);
            let Ok(Some(root)) = root else {
                return dead_link(cx).await.into_response(cx);
            };
            if root.deleted_at.is_some() {
                return dead_link(cx).await.into_response(cx);
            }
            let at = query.folder.as_deref().unwrap_or(&root.id).to_string();
            if !under_target(store.as_ref(), &root.owner_id, &root.id, &at).await {
                return dead_link(cx).await.into_response(cx);
            }
            if query.dl {
                let Some(file_id) = query.file.as_deref() else {
                    return dead_link(cx).await.into_response(cx);
                };
                let file = store
                    .file(file_id)
                    .await
                    .map_err(|_| Refusal::Unavailable);
                let Ok(Some(file)) = file else {
                    return dead_link(cx).await.into_response(cx);
                };
                if file.deleted_at.is_some()
                    || file.owner_id != root.owner_id
                    || file.folder_id.as_deref() != Some(at.as_str())
                {
                    return dead_link(cx).await.into_response(cx);
                }
                return download_bytes(cx, store.as_ref(), &file.id, &file.name, &file.mime, link.can_download).await;
            }
            if query.thumb {
                let Some(file_id) = query.file.as_deref() else {
                    return dead_link(cx).await.into_response(cx);
                };
                let file = store
                    .file(file_id)
                    .await
                    .map_err(|_| Refusal::Unavailable);
                let Ok(Some(file)) = file else {
                    return dead_link(cx).await.into_response(cx);
                };
                if file.deleted_at.is_some()
                    || file.owner_id != root.owner_id
                    || file.folder_id.as_deref() != Some(at.as_str())
                {
                    return dead_link(cx).await.into_response(cx);
                }
                return public_thumb(cx, store.as_ref(), &file.id).await;
            }
            folder_card(cx, store.as_ref(), &link, &root, &at, token).await
        }
    }
}

/// The bytes behind a public link. A view-only link answers the dead card
/// here — the same answer as a spent token — so the surface never says which
/// tokens exist.
async fn download_bytes(
    cx: &Cx,
    store: &dyn Store,
    file_id: &str,
    name: &str,
    mime: &str,
    can_download: bool,
) -> topcoat::Result<topcoat::router::response::Response> {
    use topcoat::router::response::IntoResponse;
    use topcoat::router::{HeaderMap, HeaderValue};
    if !can_download {
        return dead_link(cx).await.into_response(cx);
    }
    let bytes = store
        .file_bytes(file_id)
        .await
        .map_err(|_| Refusal::Unavailable);
    let Ok(Some(bytes)) = bytes else {
        return dead_link(cx).await.into_response(cx);
    };
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    let disposition = format!("attachment; filename=\"{}\"", safe_name(name));
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok((StatusCode::OK, headers, bytes).into_response(cx)?)
}

/// The stored webp preview behind a public link. Unlike the bytes, the
/// preview is what a view-only link grants, so this answers regardless of
/// `can_download` — and anything unservable is the dead card, like every
/// other answer on this surface. Headers twin the `files.rs` thumbnail route.
async fn public_thumb(
    cx: &Cx,
    store: &dyn Store,
    file_id: &str,
) -> topcoat::Result<topcoat::router::response::Response> {
    use topcoat::router::response::IntoResponse;
    use topcoat::router::{HeaderMap, HeaderValue};
    let Ok(Some(bytes)) = store.thumb_bytes(file_id).await else {
        return dead_link(cx).await.into_response(cx);
    };
    let etag = format!("\"{:x}\"", crate::files::fnv1a(&bytes));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/webp"),
    );
    let if_none_match = request_headers(cx)
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    if if_none_match == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, headers, Vec::new()).into_response(cx)?);
    }
    Ok((StatusCode::OK, headers, bytes).into_response(cx)?)
}

/// A filename safe for a header: quotes and backslashes stripped, never
/// empty.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// A shared file's card: its name, size and type, an image preview off the
/// public thumbnail route, and the download while the link allows it.
async fn file_card(
    cx: &Cx,
    link: &in_core::store::ShareLink,
    file: &in_core::store::File,
) -> topcoat::Result<topcoat::router::response::Response> {
    let language = lang(cx).await;
    let preview = file.mime.starts_with("image/");
    let token_path = current_path(cx);
    view! {
        cx =>
        <main class="scaffold-note">
            (wordmark(cx).await?)
            <h1 class="settings-title">(file.name.clone())</h1>
            <p class="field-note">(format!("{} · {}", file.mime.clone(), crate::settings::human_bytes(file.size_bytes)))</p>
            if preview {
                <img src=(format!("{token_path}?thumb=1")) alt=(file.name.clone())>
            } else {
                <p class="field-note">(t(language, Key::PreviewUnavailable))</p>
            }
            if link.can_download {
                <p><a class="primary" href=(format!("{token_path}?dl=1"))>(t(language, Key::Download))</a></p>
            } else {
                <p class="field-note">(t(language, Key::ViewOnly))</p>
            }
        </main>
    }
    .into_response(cx)
}

/// A shared folder's listing: subfolders then files, downloads gated on the
/// link's flag. Subfolders browse in place under the same token.
async fn folder_card(
    cx: &Cx,
    store: &dyn Store,
    link: &in_core::store::ShareLink,
    root: &in_core::store::Folder,
    at: &str,
    token: &str,
) -> topcoat::Result<topcoat::router::response::Response> {
    let language = lang(cx).await;
    let here = store
        .folder(at)
        .await?;
    let Some(here) = here else {
        return dead_link(cx).await.into_response(cx);
    };
    let listing = store
        .list_children(&root.owner_id, Some(at))
        .await?;
    let base = format!("/s/{token}");
    view! {
        cx =>
        (wordmark(cx).await?)
        <main class="settings-stage">
            <h1 class="settings-title">(here.name.clone())</h1>
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::FoldersHeading))</h2>
                    <span class="chip">(format!("{}", listing.folders.len()))</span>
                </div>
                <div class="panel-body">
                    if listing.folders.is_empty() {
                        <p class="field-note">(t(language, Key::EmptyFolder))</p>
                    }
                    for folder in &listing.folders {
                        <p><a href=(format!("{base}?folder={}", folder.id))>(folder.name.clone())</a></p>
                    }
                </div>
            </section>
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::FilesHeading))</h2>
                    <span class="chip">(format!("{}", listing.files.len()))</span>
                </div>
                <div class="panel-body">
                    if listing.files.is_empty() {
                        <p class="field-note">(t(language, Key::EmptyFolder))</p>
                    }
                    for file in &listing.files {
                        <p>
                            <span>(file.name.clone())</span>
                            (format!(" · {}", crate::settings::human_bytes(file.size_bytes)))
                            if link.can_download {
                                <a href=(format!("{base}?folder={at}&file={}&dl=1", file.id))>(t(language, Key::Download))</a>
                            } else {
                                <span class="field-note">(t(language, Key::ViewOnly))</span>
                            }
                        </p>
                    }
                </div>
            </section>
        </main>
    }
    .into_response(cx)
}

/// The request's own path, without its query: the download links the public
/// cards render point back at the same token.
fn current_path(cx: &Cx) -> String {
    let path = uri(cx).path();
    if path.is_empty() { "/".to_string() } else { path.to_string() }
}

/// Everything others shared with the reader, files and folders in their own
/// groups, newest grant first. The reader's own library never appears here.
#[page("/shared")]
async fn shared(cx: &Cx) -> Result {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            let language = lang(cx).await;
            return view! {
                cx =>
                <main class="scaffold-note">
                    <p>(refusal.message_in(language))</p>
                    <p><a href="/">(t(language, Key::BackToDrive))</a></p>
                </main>
            };
        }
    };
    let language = lang(cx).await;
    let store = app(cx).store;
    let items = store
        .shares_for_user(&user.id)
        .await?;
    let mut owners: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for item in &items {
        if !owners.contains_key(&item.owner_id) {
            let name = store
                .user(&item.owner_id)
                .await
                .ok()
                .flatten()
                .map(|owner| owner.display_name)
                .unwrap_or_default();
            owners.insert(item.owner_id.clone(), name);
        }
    }
    let folder_count = items.iter().filter(|item| item.kind == ShareKind::Folder).count();
    let file_count = items.iter().filter(|item| item.kind == ShareKind::File).count();
    view! {
        cx =>
        (topbar(cx, NavPage::Shared, &user, language).await?)
        <main class="settings-stage">
            <h1 class="settings-title">(t(language, Key::SharedWithMe))</h1>
            if items.is_empty() {
                <p class="field-note">(t(language, Key::NoSharedItems))</p>
            }
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::FoldersHeading))</h2>
                    <span class="chip">(format!("{folder_count}"))</span>
                </div>
                <div class="panel-body">
                    for item in items.iter().filter(|item| item.kind == ShareKind::Folder) {
                        <p>
                            <a href=(format!("/drive?folder={}", item.target_id))>(item.name.clone())</a>
                            (owner_chip(item, &owners))
                            (access_chip(language, item.can_download))
                        </p>
                    }
                </div>
            </section>
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::FilesHeading))</h2>
                    <span class="chip">(format!("{file_count}"))</span>
                </div>
                <div class="panel-body">
                    for item in items.iter().filter(|item| item.kind == ShareKind::File) {
                        <p>
                            <a href=(format!("/file/{}", item.target_id))>(item.name.clone())</a>
                            (owner_chip(item, &owners))
                            (access_chip(language, item.can_download))
                        </p>
                    }
                </div>
            </section>
        </main>
    }
}

/// `by <name>`, when the owner's name is known.
fn owner_chip(
    item: &in_core::store::SharedItem,
    owners: &std::collections::HashMap<String, String>,
) -> String {
    match owners.get(&item.owner_id) {
        Some(name) if !name.is_empty() => format!(" · {name}"),
        _ => String::new(),
    }
}

/// What the grant opens: the download, or the view alone.
fn access_chip(language: Lang, can_download: bool) -> &'static str {
    if can_download {
        t(language, Key::CanDownload)
    } else {
        t(language, Key::ViewOnly)
    }
}
