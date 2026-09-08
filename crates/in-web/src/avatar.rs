//! Every provisioned member's face, proxied from im.
//!
//! `GET /avatar/{user_id}` answers with im's photo bytes for any member of
//! the workspace — one's own id or a coworker's: a person's face renders in
//! every signed-in topbar, so the gate is the session, not the id. im is
//! asked with the app's own credentials and always answers an image (a
//! member with no photo gets the default tile), so the only failures left
//! are im unreachable and an id no row names — the same empty answer a
//! browser without a photo shows initials for. A browser without a session
//! is refused outright.
//!
//! Caching is versioned, not content-hashed: the `ETag` is the row's
//! `photo_version` — im's own stamp, mirrored by the directory passes — and
//! a request whose `?v=` already matches the row is stamped
//! `private, max-age=31536000, immutable`, so a changed photo is a changed
//! URL and no browser pins an old face behind a year of cache. Any other
//! spelling of the address revalidates with `private, no-cache`.

use topcoat::context::Cx;
use topcoat::router::request::{headers as request_headers, uri};
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, path_param, route};
use crate::server::{app, require_user};

path_param!(user_id);

fn not_found() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// The `?v=` the request carries, if it carries one.
fn stamped_version(cx: &Cx) -> Option<&str> {
    uri(cx).query()?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == "v").then_some(value)
    })
}

/// `GET /avatar/{user_id}`: im's photo for any provisioned member, or the
/// same not-found an unknown id gets — never a `403`, which would confirm
/// which ids exist. `?v=` equal to the row's `photo_version` is answered
/// from a year of `immutable`; anything else revalidates.
#[route(GET "/avatar/{user_id}")]
async fn avatar(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let target: &str = path_param::<UserId>(cx);

    // The gate is the session: any signed-in browser may fetch any
    // provisioned member's face, so the identity itself is not needed here.
    if require_user(cx).await.is_err() {
        // An `<img>` has no page to carry a refusal on; 401 names the fix
        // the way the live channel's does.
        return Ok((StatusCode::UNAUTHORIZED, HeaderMap::new(), Vec::new()));
    }
    // The row, not the caller's session, decides the stamp: any provisioned
    // member's face is served, keyed by their own photo version. A store
    // error folds into the same not-found as an unknown id — an `<img>` has
    // no better way to say either.
    let Ok(Some(row)) = app(cx).store.user(target).await else {
        return Ok(not_found());
    };

    let etag = format!("\"p{}\"", row.photo_version);
    let fresh = stamped_version(cx) == Some(row.photo_version.to_string().as_str());
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"p0\"")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if fresh {
            "private, max-age=31536000, immutable"
        } else {
            "private, no-cache"
        }),
    );
    // The stamp the browser holds is the row's own word: a match means the
    // bytes behind it cannot have changed without the version moving, so im
    // is not asked at all.
    let if_none_match = request_headers(cx)
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    if if_none_match == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, headers, Vec::new()));
    }

    let Ok((bytes, mime)) = app(cx).directory.photo(&row.oidc_sub).await else {
        return Ok(not_found());
    };
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    Ok((StatusCode::OK, headers, bytes))
}
