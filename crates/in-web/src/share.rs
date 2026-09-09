//! Sharing: public links, per-user grants, and the shared-with-me page.
//!
//! `POST /api/share/link/create|revoke|remint` mints, kills, and re-mints
//! bearer links over a
//! file or folder (`can_download`, optional expiry in days; only the token
//! hash is stored). `POST /api/share/user/add|remove` grants and revokes
//! named users the same way. `GET /s/{token}` is public — no auth —
//! rendering the viewer page in the document shell, the download only when
//! `can_download` allows, the stored webp preview on `?thumb=1` regardless
//! of the flag, and the card's `<video>`/`<audio>` player feed on
//! `?media=1` for the video and audio kinds; a spent, expired or revoked
//! token answers the dead card, never a stack.
//! `GET /shared` lists what others shared with the reader.
//!
//! Every mutation answers the way `board.rs` in iz does: a 303 back to the
//! page the form was posted from, the refusal (if any) on the redirect's
//! query. The one exception is link creation, whose plaintext token is
//! sealed with the app key onto the `share_token:{id}` setting row and
//! shown in the redirect's `?created=<token>` pair, so a browser without
//! script can copy it; the drive and settings pages render the copy-once
//! banner off that pair, and re-derive the full address for the live-link
//! rows from the sealed one.

use in_core::store::{File, ShareKind, Store, StoreError, ThumbState, User};
use in_core::{hash_link_password, hash_share_token, link_password_matches, link_unlock_proof};
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use time::OffsetDateTime;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::cookie::{Cookies, cookie, cookies};
use topcoat::router::content::Form;
use topcoat::router::request::{headers as request_headers, uri};
use topcoat::router::response::IntoResponse;
use topcoat::router::{HeaderName, StatusCode, header, page, path_param, query_params, route};
use topcoat::view::view;

use crate::files::{ViewerKind, entry_chip, media_player, media_player_script, viewer_kind};
use crate::i18n::{Key, Lang, lang, t};
use crate::layout::{NavPage, document_shell, topbar, wordmark};
use crate::server::{Refusal, app, back_to, require_user, share_link_url, share_origin};

path_param!(token);
path_param!(kind);
path_param!(id);

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

/// The link password off the form: absent, blank, or all whitespace is no
/// password — the field is optional, and a blank string nobody typed is
/// never a secret worth keeping.
fn form_password(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|password| !password.is_empty())
        .map(str::to_string)
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
    if owned {
        Ok(())
    } else {
        Err(Refusal::NotFound)
    }
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
    if owned {
        Ok(())
    } else {
        Err(Refusal::NotFound)
    }
}

#[derive(Deserialize)]
struct CreateLinkForm {
    kind: String,
    target_id: String,
    #[serde(default)]
    can_download: Option<String>,
    #[serde(default)]
    expires_in_days: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// Mints a bearer link. The token rides the redirect's `?created=` pair —
/// which the drive and settings pages render as the copy-once banner — and
/// is sealed with the app key onto the `share_token:{id}` setting row, so
/// the full address can be re-shown later; the row itself keeps only the
/// token's hash.
#[route(POST "/api/share/link/create")]
async fn create_link(cx: &Cx, Form(input): Form<CreateLinkForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            return redirect_back(cx, "/settings?section=links", "create", Some(refusal));
        }
    };
    let Some(kind) = parse_kind(&input.kind) else {
        return redirect_back(
            cx,
            "/settings?section=links",
            "create",
            Some(Refusal::NotFound),
        );
    };
    if let Err(refusal) = owned_target(app(cx).store.as_ref(), &user, kind, &input.target_id).await
    {
        return redirect_back(cx, "/settings?section=links", "create", Some(refusal));
    }
    let expires_at = match parse_expiry(input.expires_in_days.as_deref()) {
        Ok(expires_at) => expires_at,
        Err(refusal) => {
            return redirect_back(cx, "/settings?section=links", "create", Some(refusal));
        }
    };
    // An optional password on the link: a trimmed empty field is no
    // password at all. Argon2id is CPU-slow on purpose, so the hash runs
    // off the async path — a hash this cheap would be a hash worth
    // guessing.
    let password_hash = match form_password(input.password.as_deref()) {
        Some(password) => {
            match tokio::task::spawn_blocking(move || hash_link_password(&password)).await {
                Ok(Ok(hash)) => Some(hash),
                _ => {
                    return redirect_back(
                        cx,
                        "/settings?section=links",
                        "create",
                        Some(Refusal::Unavailable),
                    );
                }
            }
        }
        None => None,
    };
    let created = app(cx)
        .store
        .create_share_link(
            &user.id,
            kind,
            &input.target_id,
            // Absent means the checkbox came unchecked: view-only. A checked
            // box posts `1`; nothing posted must never mint a download.
            parse_flag(input.can_download.as_deref(), false),
            expires_at,
            password_hash,
        )
        .await;
    match created {
        Ok(minted) => {
            // The token is sealed onto its own setting row — the same
            // `in.key` that seals the session cookies — so the share modal
            // and the settings panel can re-show the full address later. A
            // failed seal is lived with, not refused: refusing now would
            // leave a live link whose token the one-time banner never
            // showed, the worse half of that bargain.
            let sealed = in_core::store::secret::seal(&app(cx).link_key, &minted.token);
            let _ = app(cx)
                .store
                .set_setting(&format!("share_token:{}", minted.link.id), &sealed)
                .await;
            let back = back_to(cx, "/settings?section=links");
            let separator = if back.contains('?') { '&' } else { '?' };
            let location = format!("{back}{separator}created={}", minted.token);
            Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]))
        }
        Err(error) => redirect_back(
            cx,
            "/settings?section=links",
            "create",
            Some(refusal_of(error)),
        ),
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
        Err(refusal) => {
            return redirect_back(cx, "/settings?section=links", "revoke", Some(refusal));
        }
    };
    let store = app(cx).store;
    let mine = store
        .share_links(&user.id)
        .await
        .map_err(|_| Refusal::Unavailable);
    let Ok(links) = mine else {
        return redirect_back(
            cx,
            "/settings?section=links",
            "revoke",
            Some(Refusal::Unavailable),
        );
    };
    if !links
        .iter()
        .any(|link| link.id == input.id && link.created_by == user.id)
    {
        return redirect_back(
            cx,
            "/settings?section=links",
            "revoke",
            Some(Refusal::NotFound),
        );
    }
    match store.revoke_share_link(&input.id).await {
        Ok(()) => {
            // The sealed address dies with the link: deleting the row is
            // part of revoking, not cleanup after it, so a failed delete
            // reports the revoke as refused. A retry finishes the job —
            // revoking twice is not an error.
            match store
                .delete_setting(&format!("share_token:{}", input.id))
                .await
            {
                Ok(()) => redirect_back(cx, "/settings?section=links", "revoke", None),
                Err(error) => redirect_back(
                    cx,
                    "/settings?section=links",
                    "revoke",
                    Some(refusal_of(error)),
                ),
            }
        }
        Err(error) => redirect_back(
            cx,
            "/settings?section=links",
            "revoke",
            Some(refusal_of(error)),
        ),
    }
}

/// Carries a legacy link forward: a fresh mint over the same target, with
/// the old row's `can_download`, its password, and a still-future expiry —
/// an elapsed one drops, since a minted expiry opens for no one. Only its
/// creator may; a foreign or unknown link is answered as if it never
/// existed. The old row and its sealed setting die with the mint, the
/// revoke path's exact semantics: the address remint replaces was already
/// unrecoverable.
#[route(POST "/api/share/link/remint")]
async fn remint_link(cx: &Cx, Form(input): Form<RevokeLinkForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            return redirect_back(cx, "/settings?section=links", "remint", Some(refusal));
        }
    };
    let store = app(cx).store;
    let mine = store
        .share_links(&user.id)
        .await
        .map_err(|_| Refusal::Unavailable);
    let Ok(links) = mine else {
        return redirect_back(
            cx,
            "/settings?section=links",
            "remint",
            Some(Refusal::Unavailable),
        );
    };
    let Some(old) = links
        .iter()
        .find(|link| link.id == input.id && link.created_by == user.id)
    else {
        return redirect_back(
            cx,
            "/settings?section=links",
            "remint",
            Some(Refusal::NotFound),
        );
    };
    // A still-future expiry carries over; an elapsed one would mint a link
    // that is dead on arrival.
    let expires_at = old.expires_at.filter(|at| *at > OffsetDateTime::now_utc());
    let created = store
        .create_share_link(
            &user.id,
            old.kind.clone(),
            &old.target_id,
            old.can_download,
            expires_at,
            old.password_hash.clone(),
        )
        .await;
    match created {
        Ok(minted) => match store.revoke_share_link(&input.id).await {
            Ok(()) => {
                // The sealed address dies with the link — part of the
                // remint, not cleanup after it, so a failed delete reports
                // the remint as refused.
                match store
                    .delete_setting(&format!("share_token:{}", input.id))
                    .await
                {
                    Ok(()) => {
                        // The fresh token gets the seal, exactly as a
                        // creation seals its own; a failed seal is lived
                        // with, because the redirect below shows the full
                        // address regardless.
                        let sealed = in_core::store::secret::seal(&app(cx).link_key, &minted.token);
                        let _ = app(cx)
                            .store
                            .set_setting(&format!("share_token:{}", minted.link.id), &sealed)
                            .await;
                        let back = back_to(cx, "/settings?section=links");
                        let separator = if back.contains('?') { '&' } else { '?' };
                        let location = format!("{back}{separator}created={}", minted.token);
                        Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]))
                    }
                    Err(error) => redirect_back(
                        cx,
                        "/settings?section=links",
                        "remint",
                        Some(refusal_of(error)),
                    ),
                }
            }
            Err(error) => redirect_back(
                cx,
                "/settings?section=links",
                "remint",
                Some(refusal_of(error)),
            ),
        },
        Err(error) => redirect_back(
            cx,
            "/settings?section=links",
            "remint",
            Some(refusal_of(error)),
        ),
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
/// The share form. The picker posts one `email` field per person, and
/// serde_urlencoded hands a map visitor one pair at a time — a `Vec` field
/// cannot see the repeats — so the impl below collects them itself. The
/// typed-address fallback posts a single `email`, which lands the same way.
struct ShareUserForm {
    kind: String,
    target_id: String,
    email: Vec<String>,
    can_download: Option<String>,
}

impl<'de> Deserialize<'de> for ShareUserForm {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FormVisitor;
        impl<'de> Visitor<'de> for FormVisitor {
            type Value = ShareUserForm;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the share form")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut form = ShareUserForm {
                    kind: String::new(),
                    target_id: String::new(),
                    email: Vec::new(),
                    can_download: None,
                };
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" => form.kind = map.next_value()?,
                        "target_id" => form.target_id = map.next_value()?,
                        // The repeats are the point: every checked row is
                        // one pair, collected in post order.
                        "email" => form.email.push(map.next_value()?),
                        // Absent means the checkbox came unchecked.
                        "can_download" => form.can_download = map.next_value()?,
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(form)
            }
        }
        deserializer.deserialize_map(FormVisitor)
    }
}

/// Shares one file or folder with the people the form names: one `email`
/// field per person, so the picker's multi-select posts them all in one
/// submit. Each address is folded to lowercase before lookup; an unknown
/// address is not-found.
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
    // Absent means the checkbox came unchecked: view-only, like the
    // sibling link creator above. One setting covers the whole submit.
    let can_download = parse_flag(input.can_download.as_deref(), false);
    // The picker's filter doubles as the typed-address fallback, so a
    // checkbox-only submit carries one blank `email` pair beside the checked
    // rows. Blank names nobody: only real addresses are looked up. Nothing
    // but blanks is still nobody — refused like nothing checked.
    let emails: Vec<&str> = input
        .email
        .iter()
        .map(|email| email.trim())
        .filter(|email| !email.is_empty())
        .collect();
    if emails.is_empty() {
        return redirect_back(cx, "/settings", "add", Some(Refusal::NotFound));
    }
    for email in emails {
        let grantee = store
            .user_by_email(&email.to_lowercase())
            .await
            .map_err(|_| Refusal::Unavailable);
        let Ok(Some(grantee)) = grantee else {
            return redirect_back(cx, "/settings", "add", Some(Refusal::NotFound));
        };
        // Re-sharing updates the same row — the store upserts — so a name
        // the list already carries cannot refuse here.
        if let Err(error) = store
            .add_share_user(&user.id, kind, &input.target_id, &grantee.id, can_download)
            .await
        {
            return redirect_back(cx, "/settings", "add", Some(refusal_of(error)));
        }
    }
    redirect_back(cx, "/settings", "add", None)
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
    if let Err(refusal) =
        owned_target_for_unshare(app(cx).store.as_ref(), &user, kind, &input.target_id).await
    {
        return redirect_back(cx, "/settings", "remove", Some(refusal));
    }
    let store = app(cx).store;
    // The remove form posts exactly one address.
    let email = input.email.first().map(String::as_str).unwrap_or("");
    let grantee = store
        .user_by_email(&email.trim().to_lowercase())
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
/// whether the bytes (rather than the card) are wanted, whether the stored
/// webp preview is wanted — the preview a view-only link grants — and
/// whether the inline media stream the card's player draws from is wanted.
struct SharedQuery {
    folder: Option<String>,
    file: Option<String>,
    dl: bool,
    thumb: bool,
    media: bool,
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
        media: has_flag(query, "media"),
    }
}

/// Whether the query names the bare flag, as `?dl` or `?dl=1`.
fn has_flag(query: &str, key: &str) -> bool {
    query
        .split('&')
        .any(|pair| pair == key || pair.starts_with(&format!("{key}=")))
}

/// A public page in the document shell. `#[page]` pairs its layouts at
/// build time, but this route's query asks for raw bytes (`?dl=1`,
/// `?thumb=1`, `?media=1`) that a page-shaped answer cannot carry, so the
/// `/s` surface stays a `#[route]` and wraps its views itself — in
/// `document_shell`, the same shell every `#[page]` under `/` wears, never
/// a hand-copied head.
async fn public_page(
    cx: &Cx,
    page: Result,
) -> topcoat::Result<topcoat::router::response::Response> {
    Ok(document_shell(cx, page).await?.into_response(cx)?)
}

/// The dead card: a spent, expired, revoked or never-real token, a trashed
/// target, or a download the link may not open. One answer for all of them —
/// a stranger learns nothing about which tokens exist.
async fn dead_link(cx: &Cx) -> topcoat::Result<topcoat::router::response::Response> {
    let language = lang(cx).await;
    let page = view! {
        cx =>
        <main class="scaffold-note">
            (wordmark(cx).await?)
            <p>(Refusal::ShareRevoked.message_in(language))</p>
            <p><a href="/">(t(language, Key::BackToDrive))</a></p>
        </main>
    };
    public_page(cx, page).await
}
/// The public viewer. No auth: the token in the path is the whole
/// credential. A file target renders its card (and its bytes on `?dl=1`
/// when the link may download); a folder target renders the listing of the
/// browsed folder, downloads gated per file on the same flag. The card's
/// player streams `?media=1` regardless of the flag — preview is what a
/// view-only link grants — for the video and audio kinds the signed-in
/// viewer plays inline.
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
        return dead_link(cx).await;
    };
    // A password-protected link answers nothing — card, bytes or thumbnail —
    // until this browser carries the unlock proof. One check up front gates
    // every branch below, which is the point: a gate the download route
    // forgot would not be a gate.
    if let Some(password_hash) = &link.password_hash {
        let proof = link_unlock_proof(token, password_hash);
        let presented = cookies(cx)
            .get(&link_cookie_name(&link.id))
            .map(|cookie| cookie.value().to_string());
        if presented.as_deref() != Some(proof.as_str()) {
            return password_gate(cx).await;
        }
    }
    let query = shared_query(cx);
    match link.kind {
        ShareKind::File => {
            let file = store
                .file(&link.target_id)
                .await
                .map_err(|_| Refusal::Unavailable);
            let Ok(Some(file)) = file else {
                return dead_link(cx).await;
            };
            if file.deleted_at.is_some() {
                return dead_link(cx).await;
            }
            if query.dl {
                return download_bytes(
                    cx,
                    store.as_ref(),
                    &file.id,
                    file.size_bytes,
                    &file.name,
                    &file.mime,
                    link.can_download,
                )
                .await;
            }
            if query.media {
                return media_bytes(
                    cx,
                    store.as_ref(),
                    &file.id,
                    file.size_bytes,
                    &file.name,
                    &file.mime,
                )
                .await;
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
                return dead_link(cx).await;
            };
            if root.deleted_at.is_some() {
                return dead_link(cx).await;
            }
            let at = query.folder.as_deref().unwrap_or(&root.id).to_string();
            if !under_target(store.as_ref(), &root.owner_id, &root.id, &at).await {
                return dead_link(cx).await;
            }
            if query.dl {
                let Some(file_id) = query.file.as_deref() else {
                    return dead_link(cx).await;
                };
                let file = store.file(file_id).await.map_err(|_| Refusal::Unavailable);
                let Ok(Some(file)) = file else {
                    return dead_link(cx).await;
                };
                if file.deleted_at.is_some()
                    || file.owner_id != root.owner_id
                    || file.folder_id.as_deref() != Some(at.as_str())
                {
                    return dead_link(cx).await;
                }
                return download_bytes(
                    cx,
                    store.as_ref(),
                    &file.id,
                    file.size_bytes,
                    &file.name,
                    &file.mime,
                    link.can_download,
                )
                .await;
            }
            if query.media {
                let Some(file_id) = query.file.as_deref() else {
                    return dead_link(cx).await;
                };
                let file = store.file(file_id).await.map_err(|_| Refusal::Unavailable);
                let Ok(Some(file)) = file else {
                    return dead_link(cx).await;
                };
                if file.deleted_at.is_some()
                    || file.owner_id != root.owner_id
                    || file.folder_id.as_deref() != Some(at.as_str())
                {
                    return dead_link(cx).await;
                }
                return media_bytes(
                    cx,
                    store.as_ref(),
                    &file.id,
                    file.size_bytes,
                    &file.name,
                    &file.mime,
                )
                .await;
            }
            if query.thumb {
                let Some(file_id) = query.file.as_deref() else {
                    return dead_link(cx).await;
                };
                let file = store.file(file_id).await.map_err(|_| Refusal::Unavailable);
                let Ok(Some(file)) = file else {
                    return dead_link(cx).await;
                };
                if file.deleted_at.is_some()
                    || file.owner_id != root.owner_id
                    || file.folder_id.as_deref() != Some(at.as_str())
                {
                    return dead_link(cx).await;
                }
                return public_thumb(cx, store.as_ref(), &file.id).await;
            }
            folder_card(cx, store.as_ref(), &link, &root, &at, token).await
        }
    }
}

/// The cookie a unlocked browser carries for one link, named for the link's
/// own id: two protected links are two gates, not one.
fn link_cookie_name(link_id: &str) -> String {
    format!("in_link_{link_id}")
}

/// The gate a password-protected link shows until the browser carries the
/// proof. Same public chrome as the cards — no hint of what waits behind
/// the gate, not even the file's name.
async fn password_gate(cx: &Cx) -> topcoat::Result<topcoat::router::response::Response> {
    let language = lang(cx).await;
    let action = current_path(cx);
    let wrong = has_flag(uri(cx).query().unwrap_or(""), "wrong");
    let page = view! {
        cx =>
        <main class="scaffold-note">
            (wordmark(cx).await?)
            <h1 class="settings-title">(t(language, Key::PasswordProtected))</h1>
            <p class="field-note">(t(language, Key::PasswordPrompt))</p>
            if wrong {
                <p class="field-error" role="alert">(t(language, Key::WrongPassword))</p>
            }
            <form class="pop-row-form" method="post" action=(action)>
                <input class="field-input" type="password" name="password" required="" autocomplete="off" placeholder=(t(language, Key::PasswordLabel)) aria-label=(t(language, Key::PasswordLabel))>
                <button class="quiet" type="submit">(t(language, Key::Unlock))</button>
            </form>
        </main>
    };
    public_page(cx, page).await
}

#[derive(Deserialize)]
struct UnlockForm {
    #[serde(default)]
    password: Option<String>,
}

/// Unlocks a password-protected link: the right password mints the proof
/// cookie and the visitor is sent on to the page as if the gate had never
/// been there; anything else is the gate again, the error riding `?wrong=1`.
/// The whole surface answers a 303 either way, so the response says nothing
/// about whether the guess was close.
#[route(POST "/s/{token}")]
async fn unlock_link(
    cx: &Cx,
    Form(input): Form<UnlockForm>,
) -> topcoat::Result<topcoat::router::response::Response> {
    let token: &str = path_param::<Token>(cx);
    let store = app(cx).store;
    let now = OffsetDateTime::now_utc();
    let link = store
        .resolve_share_link(&hash_share_token(token), now)
        .await
        .ok()
        .flatten();
    let Some(link) = link else {
        return dead_link(cx).await;
    };
    let Some(password_hash) = link.password_hash else {
        // Nothing to unlock: the form has no business here. Back to the page.
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("/s/{token}"))],
        )
            .into_response(cx);
    };
    let candidate = input.password.unwrap_or_default();
    // The proof needs the hash after the verify, so the verify gets a copy.
    let hash_for_verify = password_hash.clone();
    let matches =
        tokio::task::spawn_blocking(move || link_password_matches(&hash_for_verify, &candidate))
            .await
            .unwrap_or(false);
    if matches {
        let proof = link_unlock_proof(token, &password_hash);
        // Secure mirrors the session cookie: on when the identity provider
        // itself is reached over https, so a plain-http rehearsal stays
        // plain.
        let secure = app(cx).config.oidc.issuer.starts_with("https://");
        let name = link_cookie_name(&link.id);
        cookies(cx).add(cookie! {
            name = proof;
            Path = "/s";
            HttpOnly;
            SameSite = Lax;
            Secure = secure
        });
        (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("/s/{token}"))],
        )
            .into_response(cx)
    } else {
        (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("/s/{token}?wrong=1"))],
        )
            .into_response(cx)
    }
}

/// The bytes behind a public link. A view-only link answers the dead card
/// here — the same answer as a spent token — so the surface never says which
/// tokens exist. The body streams off disk through
/// [`crate::files::bytes_response`], the same shape the signed-in route
/// serves: a range reads only its span, and no fetch buffers the file whole.
async fn download_bytes(
    cx: &Cx,
    store: &dyn Store,
    file_id: &str,
    size_bytes: u64,
    name: &str,
    mime: &str,
    can_download: bool,
) -> topcoat::Result<topcoat::router::response::Response> {
    use topcoat::router::response::IntoResponse;
    use topcoat::router::{HeaderMap, HeaderValue};
    if !can_download {
        return dead_link(cx).await;
    }
    // A fetch counts once: a full fetch, or a range resuming from byte 0.
    // A mid-file chunk is the same view going on, not a new one — the way
    // the signed-in download route counts its first chunk only.
    let range = request_headers(cx)
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let counts = match range {
        None => true,
        Some(header) => range_starts_at_zero(header),
    };
    if counts {
        let _ = store.record_download(file_id).await;
    }
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
    let Some(parts) =
        crate::files::bytes_response(store, file_id, size_bytes, range, headers).await
    else {
        return dead_link(cx).await;
    };
    Ok(parts.into_response(cx)?)
}

/// The inline media stream behind a public link: the bytes the card's
/// `<video>`/`<audio>` player draws from, through
/// [`crate::files::bytes_response`] — a range reads only its span, the way
/// every byte route here serves. Only the kinds the signed-in viewer plays
/// through their native elements answer: preview is what a view-only link
/// grants, but a document's bytes (pdf, text) would leak through the same
/// inline fetch, so anything else — and anything unservable — is the dead
/// card, like every other answer on this surface. Never an attachment, and
/// never counted as a download: a view is not a download.
async fn media_bytes(
    cx: &Cx,
    store: &dyn Store,
    file_id: &str,
    size_bytes: u64,
    name: &str,
    mime: &str,
) -> topcoat::Result<topcoat::router::response::Response> {
    use topcoat::router::response::IntoResponse;
    use topcoat::router::{HeaderMap, HeaderValue};
    if !matches!(
        viewer_kind(mime),
        Some(ViewerKind::Video | ViewerKind::Audio)
    ) {
        return dead_link(cx).await;
    }
    let range = request_headers(cx)
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    // The stored bytes under the stored mime: looking is what a view-only
    // link grants, and this grants nothing beyond it — there is no download
    // shape on this route, and `?dl=1` still answers the original or the
    // dead card.
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    let disposition = format!("inline; filename=\"{}\"", safe_name(name));
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    // Safari refuses to play a media element without a `206` reply to its
    // own `Range` probe; the signed-in byte route declares the same.
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    let Some(parts) =
        crate::files::bytes_response(store, file_id, size_bytes, range, headers).await
    else {
        return dead_link(cx).await;
    };
    Ok(parts.into_response(cx)?)
}

/// Whether a `Range` header starts at byte 0: only those ranges (and the
/// header's absence) count a download. Anything else — a mid-file resume,
/// a suffix probe, several ranges, garbage — does not.
fn range_starts_at_zero(header: &str) -> bool {
    let Some(spec) = header.strip_prefix("bytes=") else {
        return false;
    };
    if spec.contains(',') {
        return false;
    }
    match spec.split_once('-') {
        Some(("0", _)) => true,
        _ => false,
    }
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
        return dead_link(cx).await;
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
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"));
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
/// public thumbnail route, the video/audio kinds through the house player
/// itself — [`crate::files::media_player`], the one markup + bar + script
/// the signed-in viewer renders, its classes (`media-player`, `media-el`,
/// `viewer-video`) verbatim, the stream off the public `?media=1` route —
/// and the download while the link allows it.
async fn file_card(
    cx: &Cx,
    link: &in_core::store::ShareLink,
    file: &in_core::store::File,
) -> topcoat::Result<topcoat::router::response::Response> {
    let language = lang(cx).await;
    let preview = file.mime.starts_with("image/");
    let token_path = current_path(cx);
    let media = viewer_kind(&file.mime);
    let media_src = format!("{token_path}?media=1");
    let page = view! {
        cx =>
        <main class="scaffold-note">
            (wordmark(cx).await?)
            <h1 class="settings-title">(file.name.clone())</h1>
            <p class="field-note">(format!("{} · {}", file.mime.clone(), crate::settings::human_bytes(file.size_bytes)))</p>
            if preview {
                <img src=(format!("{token_path}?thumb=1")) alt=(file.name.clone())>
            } else if matches!(media, Some(ViewerKind::Video)) {
                (media_player(cx, language, file.name.clone(), media_src.clone(), true).await?)
            } else if matches!(media, Some(ViewerKind::Audio)) {
                (media_player(cx, language, file.name.clone(), media_src.clone(), false).await?)
            } else {
                <p class="field-note">(t(language, Key::PreviewUnavailable))</p>
            }
            if link.can_download {
                <p><a class="primary" href=(format!("{token_path}?dl=1"))>(t(language, Key::Download))</a></p>
            } else {
                <p class="field-note">(t(language, Key::ViewOnly))</p>
            }
        </main>
        (media_player_script(cx).await?)
    };
    public_page(cx, page).await
}

/// One file's chip on the public card: the link's own thumbnail where one
/// is ready, else the same mime-class glyph the signed-in lists wear.
/// `/thumb/{id}` needs a session, so this points at the public `?thumb=1`
/// route behind the same token instead.
async fn public_chip(cx: &Cx, thumb_src: &str, file: &in_core::store::File) -> Result {
    if file.thumb_state == ThumbState::Ready {
        return view! {
            cx =>
            <img class="file-chip" src=(thumb_src.to_string()) alt="">
        };
    }
    entry_chip(cx, file).await
}

/// One shared file's chip: the thumbnail image while the target is live,
/// the mime-class glyph once it is trashed — `/thumb/{id}` 404s trashed
/// rows, so a Ready image would render broken if the target went into the
/// trash after the grant. The clone only clears the thumbnail flag for the
/// render.
async fn shared_chip(cx: &Cx, file: &in_core::store::File) -> Result {
    if file.deleted_at.is_some() {
        let mut unthumb = file.clone();
        unthumb.thumb_state = ThumbState::None;
        return entry_chip(cx, &unthumb).await;
    }
    entry_chip(cx, file).await
}

/// One row of the public folder listing, folder or file, for the unified
/// list.
enum PublicEntry<'a> {
    Folder(&'a in_core::store::Folder),
    File(&'a in_core::store::File),
}

impl PublicEntry<'_> {
    fn name(&self) -> &str {
        match self {
            PublicEntry::Folder(folder) => &folder.name,
            PublicEntry::File(file) => &file.name,
        }
    }
}

/// A shared folder's listing: folders and files in one list, downloads
/// gated on the link's flag. Subfolders browse in place under the same
/// token.
async fn folder_card(
    cx: &Cx,
    store: &dyn Store,
    link: &in_core::store::ShareLink,
    root: &in_core::store::Folder,
    at: &str,
    token: &str,
) -> topcoat::Result<topcoat::router::response::Response> {
    let language = lang(cx).await;
    let here = store.folder(at).await?;
    let Some(here) = here else {
        return dead_link(cx).await;
    };
    let listing = store.list_children(&root.owner_id, Some(at)).await?;
    let base = format!("/s/{token}");
    let mut rows: Vec<PublicEntry> = Vec::new();
    rows.extend(listing.folders.iter().map(PublicEntry::Folder));
    rows.extend(listing.files.iter().map(PublicEntry::File));
    rows.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    let page = view! {
        cx =>
        (wordmark(cx).await?)
        <main class="settings-stage stage-wide">
            <h1 class="settings-title">(here.name.clone())</h1>
            <section class="panel">
                <div class="panel-body">
                    if rows.is_empty() {
                        <p class="field-note">(t(language, Key::EmptyFolder))</p>
                    }
                    for row in &rows {
                        match row {
                            PublicEntry::Folder(folder) => <div class="dep-row">
                                <span class="file-chip file-chip-folder" aria-hidden="true">"▤"</span>
                                <a class="dep-link" href=(format!("{base}?folder={}", folder.id))><span class="dep-title">(folder.name.clone())</span></a>
                            </div>,
                            PublicEntry::File(file) => <div class="dep-row">
                                (public_chip(cx, &format!("{base}?folder={at}&file={}&thumb=1", file.id), file).await?)
                                <span class="member-name dep-title">(file.name.clone())</span>
                                <span class="field-note">(crate::settings::human_bytes(file.size_bytes))</span>
                                <div class="spacer"></div>
                                if link.can_download {
                                    <a class="quiet" href=(format!("{base}?folder={at}&file={}&dl=1", file.id))>(t(language, Key::Download))</a>
                                } else {
                                    <span class="field-note">(t(language, Key::ViewOnly))</span>
                                }
                            </div>,
                        }
                    }
                </div>
            </section>
        </main>
    };
    public_page(cx, page).await
}

/// The request's own path, without its query: the download links the public
/// cards render point back at the same token.
fn current_path(cx: &Cx) -> String {
    let path = uri(cx).path();
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

#[query_params]
struct SharedPageQuery {
    sort: Option<String>,
    kind: Option<String>,
    q: Option<String>,
}

/// The shared list's sort, off the query as `key:direction` ("owner:desc"):
/// name, uploaded, size or owner; name and owner ascend by default, the
/// measures descend. Anything else is the default — by name.
fn valid_shared_sort(raw: Option<&str>) -> (&'static str, bool) {
    let (key, dir) = raw
        .unwrap_or("")
        .split_once(':')
        .unwrap_or((raw.unwrap_or(""), ""));
    let key = match key {
        "name" => "name",
        "uploaded" => "uploaded",
        "size" => "size",
        "owner" => "owner",
        _ => "name",
    };
    let descending = match dir {
        "asc" => false,
        "desc" => true,
        _ => matches!(key, "uploaded" | "size"),
    };
    (key, descending)
}

/// The kind filter, off the query: all, folders or files. Anything else
/// shows everything.
fn valid_shared_kind(raw: Option<&str>) -> &'static str {
    match raw {
        Some("folders") => "folders",
        Some("files") => "files",
        _ => "all",
    }
}

/// One grant with the target's own dates and counters, for the unified
/// list. The file row stays aboard for the list chip; folders carry none.
struct SharedRow {
    item: in_core::store::SharedItem,
    owner_name: String,
    file: Option<in_core::store::File>,
    uploaded: OffsetDateTime,
}

impl SharedRow {
    fn size(&self) -> u64 {
        self.file.as_ref().map(|file| file.size_bytes).unwrap_or(0)
    }
}

/// Everything others shared with the reader, folders and files in one
/// list. The reader's own library never appears here.
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
    let params = query_params::<SharedPageQuery>(cx).ok();
    let sort = valid_shared_sort(params.as_ref().and_then(|query| query.sort.as_deref()));
    let kind = valid_shared_kind(params.as_ref().and_then(|query| query.kind.as_deref()));
    let asked = params
        .as_ref()
        .and_then(|query| query.q.clone())
        .map(|query| query.trim().to_string())
        .filter(|query| !query.is_empty());
    let box_text = asked.clone().unwrap_or_default();
    let store = app(cx).store;
    let items = store.shares_for_user(&user.id).await?;
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
    let mut rows: Vec<SharedRow> = Vec::new();
    for item in items {
        if kind == "folders" && item.kind != ShareKind::Folder {
            continue;
        }
        if kind == "files" && item.kind != ShareKind::File {
            continue;
        }
        if let Some(needle) = asked.as_deref() {
            if !item.name.to_lowercase().contains(&needle.to_lowercase()) {
                continue;
            }
        }
        let owner_name = owners.get(&item.owner_id).cloned().unwrap_or_default();
        // The grant carries no dates or counters of its own: the file row
        // brings the upload date, size and downloads, the folder row its
        // upload date. A target that went missing since the grant keeps
        // its row on the grant's own date.
        let (file, uploaded) = match item.kind {
            ShareKind::File => match store.file(&item.target_id).await.ok().flatten() {
                Some(file) => {
                    let uploaded = file.created_at;
                    (Some(file), uploaded)
                }
                None => (None, item.created_at),
            },
            ShareKind::Folder => {
                let uploaded = store
                    .folder(&item.target_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|folder| folder.created_at)
                    .unwrap_or(item.created_at);
                (None, uploaded)
            }
        };
        rows.push(SharedRow {
            item,
            owner_name,
            file,
            uploaded,
        });
    }
    let nothing_shared = rows.is_empty() && asked.is_none() && kind == "all";
    let (sort, descending) = sort;
    rows.sort_by(|a, b| {
        let order = match sort {
            "uploaded" => a
                .uploaded
                .cmp(&b.uploaded)
                .then_with(|| a.item.name.to_lowercase().cmp(&b.item.name.to_lowercase())),
            "size" => a
                .size()
                .cmp(&b.size())
                .then_with(|| a.item.name.to_lowercase().cmp(&b.item.name.to_lowercase())),
            "owner" => a
                .owner_name
                .to_lowercase()
                .cmp(&b.owner_name.to_lowercase())
                .then_with(|| a.item.name.to_lowercase().cmp(&b.item.name.to_lowercase())),
            _ => a
                .item
                .name
                .to_lowercase()
                .cmp(&b.item.name.to_lowercase())
                .then_with(|| a.item.target_id.cmp(&b.item.target_id)),
        };
        if descending { order.reverse() } else { order }
    });
    let sort_value = format!("{}:{}", sort, if descending { "desc" } else { "asc" });
    view! {
        cx =>
        (topbar(cx, NavPage::Shared, &user, language).await?)
        <main class="settings-stage stage-wide">
            <h1 class="settings-title">(t(language, Key::SharedWithMe))</h1>
            <div class="filterbar">
                <form class="field-box field-box-search" method="get" action="/shared">
                    <span class="field-text">(t(language, Key::NavSearch))</span>
                    <input
                        class="filter-search"
                        type="search"
                        name="q"
                        value=(box_text.clone())
                        placeholder=(t(language, Key::SearchPlaceholder))
                        aria-label=(t(language, Key::SearchPlaceholder))
                    >
                    <input type="hidden" name="sort" value=(sort.to_string())>
                    <input type="hidden" name="kind" value=(kind.to_string())>
                </form>
                <form class="field-box field-box-sort" method="get" action="/shared">
                    <span class="field-text">(t(language, Key::Sort))</span>
                    <select class="status-select" name="sort" data-autosubmit="" aria-label=(t(language, Key::Sort))>
                        <option value="name:asc" selected=(sort_value == "name:asc")>(t(language, Key::SortNameAZ))</option>
                        <option value="name:desc" selected=(sort_value == "name:desc")>(t(language, Key::SortNameZA))</option>
                        <option value="uploaded:desc" selected=(sort_value == "uploaded:desc")>(t(language, Key::SortNewest))</option>
                        <option value="uploaded:asc" selected=(sort_value == "uploaded:asc")>(t(language, Key::SortOldest))</option>
                        <option value="size:desc" selected=(sort_value == "size:desc")>(t(language, Key::SortLargest))</option>
                        <option value="size:asc" selected=(sort_value == "size:asc")>(t(language, Key::SortSmallest))</option>
                        <option value="owner:asc" selected=(sort_value == "owner:asc")>(t(language, Key::SortOwnerAZ))</option>
                        <option value="owner:desc" selected=(sort_value == "owner:desc")>(t(language, Key::SortOwnerZA))</option>
                    </select>
                    <input type="hidden" name="kind" value=(kind.to_string())>
                    <input type="hidden" name="q" value=(box_text.clone())>
                </form>
                <form class="field-box field-box-sort" method="get" action="/shared">
                    <span class="field-text">(t(language, Key::Kind))</span>
                    <select class="status-select" name="kind" data-autosubmit="" aria-label=(t(language, Key::Kind))>
                        <option value="all" selected=(kind == "all")>(t(language, Key::KindAll))</option>
                        <option value="folders" selected=(kind == "folders")>(t(language, Key::KindFolders))</option>
                        <option value="files" selected=(kind == "files")>(t(language, Key::KindFiles))</option>
                    </select>
                    <input type="hidden" name="sort" value=(sort.to_string())>
                    <input type="hidden" name="q" value=(box_text.clone())>
                </form>
            </div>
            <section class="panel drive-panel drive-owner">
                if rows.is_empty() {
                    <div class="drive-empty">
                        <span class="drive-empty-glyph" aria-hidden="true">"▤"</span>
                        <p class="drive-empty-text">(t(language, if nothing_shared { Key::NoSharedItems } else { Key::NoResults }))</p>
                    </div>
                } else {
                <div class="drive-head">
                    <span class="drive-cols">
                        <span class="drive-col-ico" aria-hidden="true"></span>
                        <span>(t(language, Key::NameColumn))</span>
                        <span class="drive-col-num drive-col-size">(t(language, Key::SizeColumn))</span>
                        <span class="drive-col-date">(t(language, Key::ModifiedColumn))</span>
                        <span class="drive-col-num drive-col-dl">(t(language, Key::OwnerColumn))</span>
                    </span>
                </div>
                <div class="drive-list">
                    for row in &rows {
                        // The grant's facts ride the whole row: the last cell
                        // names who opened it and how far the opening goes.
                        <div class="drive-row">
                            <a class="drive-open" href=(match row.item.kind {
                                ShareKind::Folder => format!("/drive?folder={}", row.item.target_id),
                                ShareKind::File => format!("/view/{}?from=shared", row.item.target_id),
                            })>
                                if row.item.kind == ShareKind::Folder {
                                    <span class="file-chip file-chip-folder" aria-hidden="true">"▤"</span>
                                } else {
                                    match &row.file {
                                        Some(file) => (shared_chip(cx, file).await?),
                                        None => <span class="file-chip file-chip-generic" aria-hidden="true">"▦"</span>,
                                    }
                                }
                                <span class="dep-title">(row.item.name.clone())</span>
                                if row.item.kind == ShareKind::File {
                                    <span class="drive-meta drive-size">(crate::drive::human_size(row.size()))</span>
                                } else {
                                    <span class="drive-meta" aria-hidden="true"></span>
                                }
                                <span class="drive-meta drive-date">(row.uploaded.date().to_string())</span>
                                <span class="drive-meta drive-dl">(format!("{} · {}", row.owner_name, access_chip(language, row.item.can_download)))</span>
                            </a>
                        </div>
                    }
                </div>
                }
            </section>
        </main>
        (crate::dropdown::dropdown_script(cx).await?)
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

/// The share modal: the drive row's Share item links to `?share=kind:id`
/// and the drive page renders this over itself — the Proton-style dialog
/// (add-people row, who-has-access list, public-link section), not a
/// separate page. `None` when the target is missing, trashed, or not the
/// reader's — a hand-edited `?share=` pair simply opens nothing.
///
/// Every form inside posts to the existing share routes and comes back
/// through `Referer`, so the modal survives them; the minted token's
/// copy-once banner rides the `?created=` pair into the modal itself.
pub(crate) async fn share_modal(
    cx: &Cx,
    kind_raw: &str,
    target_id: &str,
    close_href: &str,
    created: Option<String>,
) -> Result<Option<topcoat::view::View>> {
    let Some(kind) = parse_kind(kind_raw) else {
        return Ok(None);
    };
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(_) => return Ok(None),
    };
    let language = lang(cx).await;
    // The links this modal shows carry the public origin — the admin-set
    // `base_url` setting when one is stored, else the config chain: the
    // file's `base_url`, else the bound address.
    let origin = share_origin(cx).await;
    let store = app(cx).store.clone();
    // The owned, live target: present, untrashed, and theirs. Anything else
    // opens nothing — a stranger learns not even whose it is.
    let (name, file): (String, Option<File>) = match kind {
        ShareKind::File => {
            let Some(file) = store.file(target_id).await? else {
                return Ok(None);
            };
            if file.owner_id != user.id || file.deleted_at.is_some() {
                return Ok(None);
            }
            (file.name.clone(), Some(file))
        }
        ShareKind::Folder => {
            let Some(folder) = store.folder(target_id).await? else {
                return Ok(None);
            };
            if folder.owner_id != user.id || folder.deleted_at.is_some() {
                return Ok(None);
            }
            (folder.name.clone(), None)
        }
    };
    let links = store.share_links(&user.id).await?;
    let live: Vec<_> = links
        .iter()
        .filter(|link| {
            link.kind == kind && link.target_id == target_id && link.revoked_at.is_none()
        })
        .collect();
    let grants = match store.shares_for_target(&user.id, kind, target_id).await {
        Ok(grants) => grants,
        Err(StoreError::NotFound) | Err(StoreError::CrossOwner) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut people: Vec<(String, String, bool)> = Vec::new();
    for grant in &grants {
        if let Some(grantee) = store.user(&grant.user_id).await? {
            people.push((grantee.display_name, grantee.email, grant.can_download));
        }
    }
    // The shareable people come from im's directory: each entry mirrored
    // into a local user row here, so the address the form posts resolves
    // the way a typed one would — then the owner, anyone already on the
    // list above, and any locally-disabled account are taken back out
    // (im's directory not naming someone disabled here must not offer
    // them). A directory that does not answer, or answers nobody new,
    // leaves the typed-address field in place below.
    let default_quota = app(cx).config.default_quota_bytes;
    let mut candidates: Vec<User> = Vec::new();
    if let Ok(members) = app(cx).directory.directory().await {
        for member in members {
            let row = store
                .provision_user(
                    &member.sub,
                    &member.email,
                    &member.name,
                    Some(member.admin),
                    member.photo_version,
                    default_quota,
                )
                .await?;
            if !row.disabled
                && row.id != user.id
                && !people.iter().any(|(_, email, _)| *email == row.email)
            {
                candidates.push(row);
            }
        }
    }
    candidates.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    let refusal = query_value(uri(cx).query().unwrap_or(""), "refusal")
        .and_then(|code| Refusal::from_code(&code));
    Ok(Some(view! {
        cx =>
        <div class="modal-scrim">
            <div class="modal share-modal">
                <div class="viewer-head">
                    <h2 class="settings-title share-modal-title">
                        if kind == ShareKind::Folder {
                            <span class="file-chip file-chip-folder" aria-hidden="true">"▤"</span>
                        }
                        if let Some(file) = file.as_ref() {
                            (entry_chip(cx, file).await?)
                        }
                        (format!("{} “{}”", t(language, Key::Share), name.clone()))
                    </h2>
                    <div class="spacer"></div>
                    <a class="quiet" href=(close_href.to_string()) aria-label=(t(language, Key::Close))>(t(language, Key::Close))</a>
                </div>
                if let Some(refusal) = refusal {
                    <p class="field-error share-modal-note">(refusal.message_in(language))</p>
                }
                if let Some(token) = created {
                    <p class="field-note share-modal-note">(t(language, Key::CopyLinkOnce))</p>
                    <div class="share-link-row">
                        <p class="member-link-value share-link-url" aria-label=(t(language, Key::ShareLink))>(format!("{origin}/s/{token}"))</p>
                        <button class="quiet share-copy" type="button" data-copied-label=(t(language, Key::Copied))>(t(language, Key::CopyLink))</button>
                    </div>
                }
                <form class="pop-row-form share-add" method="post" action="/api/share/user/add">
                    <input type="hidden" name="kind" value=(kind.as_str())>
                    <input type="hidden" name="target_id" value=(target_id.to_string())>
                    if candidates.is_empty() {
                        <input class="field-input share-add-email" type="email" name="email" required="" placeholder=(t(language, Key::SharePlaceholder)) aria-label=(t(language, Key::EmailAddress))>
                    } else {
                        <div class="share-picker pop-panel" role="group" aria-label=(t(language, Key::SharePickPerson))>
                            // The filter doubles as the typed-address fallback: it
                            // carries `name="email"`, so typing an address and
                            // submitting without script still grants that person.
                            // With script it only filters — the script half benches
                            // it on submit whenever rows are checked, and the
                            // route skips the blank pair a checkbox-only post
                            // carries beside the checked values.
                            <input class="field-input share-pick-filter" type="email" name="email" placeholder=(t(language, Key::SharePickPerson)) aria-label=(t(language, Key::SharePickPerson))>
                            <label class="pop-row share-pick-row share-pick-all">
                                <input type="checkbox" class="share-all-toggle" aria-label=(t(language, Key::ShareEveryone))>
                                <svg class="glyph share-pick-glyph" width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.5 8.5l3.5 3.5 7.5-8"></path></svg>
                                <span class="pop-row-name">(t(language, Key::ShareEveryone))</span>
                            </label>
                            <div class="pop-list pop-list-scroll">
                                for person in &candidates {
                                    <label class="pop-row share-pick-row">
                                        <input type="checkbox" name="email" value=(person.email.clone())>
                                        <svg class="glyph share-pick-glyph" width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.5 8.5l3.5 3.5 7.5-8"></path></svg>
                                        <span class="pop-row-name">(person.display_name.clone())</span>
                                        <span class="share-pick-mail">(person.email.clone())</span>
                                    </label>
                                }
                                // Ships hidden: only the script ever shows it,
                                // when the filter matches no row.
                                <p class="field-note share-pick-none share-pick-off">(t(language, Key::ShareNoMatch))</p>
                            </div>
                        </div>
                    }
                    <select class="field-input share-add-access" name="can_download" aria-label=(t(language, Key::CanDownload))>
                        <option value="1">(t(language, Key::CanDownload))</option>
                        <option value="0">(t(language, Key::ViewOnly))</option>
                    </select>
                    <button class="quiet" type="submit">(t(language, Key::Share))</button>
                </form>
                <p class="share-section">(t(language, Key::WhoHasAccess))</p>
                <div class="member-row share-owner">
                    <span class="member-name">(format!("{} {}", user.display_name.clone(), t(language, Key::YouSuffix)))</span>
                    <span class="field-note">(user.email.clone())</span>
                    <div class="spacer"></div>
                    <span class="field-note">(t(language, Key::OwnerRole))</span>
                </div>
                for person in &people {
                    <div class="member-row">
                        <span class="member-name">(person.0.clone())</span>
                        <span class="field-note">(person.1.clone())</span>
                        <div class="spacer"></div>
                        <span class="field-note">(access_chip(language, person.2))</span>
                        <form class="pop-row-form" method="post" action="/api/share/user/remove">
                            <input type="hidden" name="kind" value=(kind.as_str())>
                            <input type="hidden" name="target_id" value=(target_id.to_string())>
                            <input type="hidden" name="email" value=(person.1.clone())>
                            <button class="quiet" type="submit">(t(language, Key::RemoveAccess))</button>
                        </form>
                    </div>
                }
                <p class="share-section">(t(language, Key::PublicLinkLabel))</p>
    if live.is_empty() {
        <form class="pop-row-form" method="post" action="/api/share/link/create">
            <input type="hidden" name="kind" value=(kind.as_str())>
            <input type="hidden" name="target_id" value=(target_id.to_string())>
            <span class="field-note">(t(language, Key::NotActive))</span>
            <div class="spacer"></div>
            <select class="field-input" name="can_download" aria-label=(t(language, Key::CanDownload))>
                <option value="1">(t(language, Key::CanDownload))</option>
                <option value="0">(t(language, Key::ViewOnly))</option>
            </select>
            <input class="field-input share-expiry" type="number" name="expires_in_days" min="1" step="1" placeholder=(t(language, Key::ExpiresInDays)) aria-label=(t(language, Key::ExpiresInDays))>
            <input class="field-input share-expiry" type="password" name="password" autocomplete="new-password" placeholder=(t(language, Key::PasswordOptional)) aria-label=(t(language, Key::PasswordOptional))>
            <button class="quiet" type="submit">(t(language, Key::CreateLink))</button>
        </form>
    }
    for link in live.iter().take(1) {
        // away. A link from before the sealing — or one whose key is gone —
        // has no address to re-show: the row carries the mint action and the
        // note below states the fact.
        let url = share_link_url(cx, *link).await;
        let legacy = url.is_none();
        <div class="member-row">
            <span class="member-name">(t(language, Key::AnyoneWithLink))</span>
            <div class="spacer"></div>
            <span class="field-note">(expiry_line(language, link.expires_at))</span>
            if link.password_hash.is_some() {
                <span class="field-note">"▪ "(t(language, Key::PasswordLabel))</span>
            }
            <span class="field-note">(access_chip(language, link.can_download))</span>
        </div>
        <div class="share-link-row">
            if let Some(url) = url {
                <p class="member-link-value share-link-url" aria-label=(t(language, Key::ShareLink))>(url)</p>
                <button class="quiet share-copy" type="button" data-copied-label=(t(language, Key::Copied))>(t(language, Key::CopyLink))</button>
            } else {
                <form class="pop-row-form" method="post" action="/api/share/link/remint">
                    <input type="hidden" name="id" value=(link.id.clone())>
                    <button class="quiet" type="submit">(t(language, Key::RemintLink))</button>
                </form>
            }
            <form class="pop-row-form" method="post" action="/api/share/link/revoke">
                <input type="hidden" name="id" value=(link.id.clone())>
                <button class="quiet quiet-danger" type="submit">(t(language, Key::RevokeLink))</button>
            </form>
        </div>
        if legacy {
            <p class="share-link-note">(t(language, Key::LegacyLinkNote))</p>
        }
    }
            </div>
        </div>
        (share_copy_script(cx).await?)
        (share_pick_script(cx).await?)
    }?))
}

/// The link row's copy client: one delegated listener, idempotent across
/// re-renders (`window.__inShareCopy` guards it), so the `in:wire` morph
/// needs no per-element re-init. A click on the copy button or on the
/// wrapping link text itself copies the full address — and only where a
/// copy button is present: the masked legacy rows carry none, so their
/// placeholder can never reach a clipboard.
pub(crate) async fn share_copy_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
            if (window.__inShareCopy) { return; } \
            window.__inShareCopy = true; \
            document.addEventListener('click', function (e) { \
                if (!e.target || !e.target.closest) { return; } \
                var hit = e.target.closest('.share-copy, .share-link-url'); \
                if (!hit) { return; } \
                var row = hit.closest('.share-link-row'); \
                if (!row) { return; } \
                var b = row.querySelector('.share-copy'); \
                var u = row.querySelector('.share-link-url'); \
                if (!b || !u) { return; } \
                if (document.createRange && window.getSelection) { \
                    var r = document.createRange(); r.selectNodeContents(u); \
                    var s = window.getSelection(); s.removeAllRanges(); s.addRange(r); \
                } \
                var done = function () { b.textContent = b.getAttribute('data-copied-label'); }; \
                if (navigator.clipboard && navigator.clipboard.writeText) { navigator.clipboard.writeText(u.textContent).then(done, done); } \
                else { try { document.execCommand('copy'); } catch (err) {} done(); } \
            }); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

/// The people picker's client half. The All row checks and unchecks every
/// candidate, and hand-checking rows keeps the All row honest. The filter
/// field narrows the rows live — substring, case-insensitive, against name
/// and address — and hides itself down to the Everyone row when no filter
/// word matches. The same field carries the no-script typed-address post,
/// so on submit it is benched whenever rows are checked: its half-typed
/// text must not ride beside them. The first keystroke also locks the
/// panel at its full, unfiltered height — the rows the filter hides would
/// otherwise shrink the modal under it, jumping everything below with
/// every letter — and a panel reborn through a morph re-render locks
/// anew. Delegated listeners only, idempotent across the modal's
/// re-renders (`window.__inSharePick` guards it), so the `in:wire` morph
/// needs no per-element re-init.
pub(crate) async fn share_pick_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
            if (window.__inSharePick) { return; } \
            window.__inSharePick = true; \
            function rows(box) { return Array.prototype.slice.call(box.querySelectorAll('.pop-list input[type=checkbox]')); } \
            function pickRows(box) { return Array.prototype.slice.call(box.querySelectorAll('.pop-list .share-pick-row')); } \
            document.addEventListener('submit', function (e) { \
                var form = e.target; \
                if (!form || !form.classList || !form.classList.contains('share-add')) { return; } \
                var box = form.querySelector('.share-picker'); \
                if (!box) { return; } \
                var filter = box.querySelector('.share-pick-filter'); \
                if (filter && box.querySelector('.pop-list input[type=checkbox]:checked')) { \
                    filter.disabled = true; \
                    setTimeout(function () { filter.disabled = false; }, 0); \
                } \
            }); \
            document.addEventListener('input', function (e) { \
                var t = e.target; \
                if (!t || !t.classList || !t.classList.contains('share-pick-filter')) { return; } \
                var box = t.closest('.share-picker'); \
                if (!box) { return; } \
                if (!box.style.minHeight) { box.style.minHeight = box.getBoundingClientRect().height + 'px'; } \
                var needle = t.value.trim().toLowerCase(); \
                var shown = 0; \
                pickRows(box).forEach(function (row) { \
                    var hit = !needle || row.textContent.toLowerCase().indexOf(needle) !== -1; \
                    row.classList.toggle('share-pick-off', !hit); \
                    if (hit) { shown += 1; } \
                }); \
                var word = needle === '' || needle.indexOf('everyone') !== -1 || needle.indexOf('herkes') !== -1 || needle.indexOf('all') !== -1; \
                var all = box.querySelector('.share-pick-all'); \
                if (all) { all.classList.toggle('share-pick-off', !word); } \
                var none = box.querySelector('.share-pick-none'); \
                if (none) { none.classList.toggle('share-pick-off', needle === '' || shown > 0 || word); } \
            }); \
            document.addEventListener('change', function (e) { \
                var t = e.target; \
                if (!t || !t.closest) { return; } \
                var box = t.closest('.share-picker'); \
                if (!box) { return; } \
                if (t.classList.contains('share-all-toggle')) { \
                    rows(box).forEach(function (cb) { cb.checked = t.checked; }); \
                } else { \
                    var all = box.querySelector('.share-all-toggle'); \
                    var list = rows(box); \
                    if (all) { all.checked = list.length > 0 && list.every(function (cb) { return cb.checked; }); } \
                } \
            }); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

/// When the link stops opening, or never on its own. Twins the settings
/// page's helper; the share page renders the same expiry line.
fn expiry_line(language: Lang, expires_at: Option<OffsetDateTime>) -> String {
    match expires_at {
        Some(at) => format!(
            "{} {}",
            t(language, Key::ExpiresLabel),
            at.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "?".to_string())
        ),
        None => t(language, Key::NeverExpires).to_string(),
    }
}
