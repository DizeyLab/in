//! The signed-in person's face, proxied from im.
//!
//! `GET /avatar/{user_id}` answers with im's photo bytes for the caller's own
//! id — anything else (another id, no photo, im unreachable) is the same 404,
//! and a browser without a session is refused outright. Served with the same
//! fnv1a ETag + immutable cache as im's own photo route, so the topbar's
//! `<img>` revalidates rather than refetches.

use topcoat::context::Cx;
use topcoat::router::request::headers as request_headers;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, path_param, route};
use crate::server::require_user;

path_param!(user_id);

fn not_found() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// `GET /avatar/{user_id}`: im's photo for the caller, or the same not-found
/// a stranger would see — never a `403`, which would confirm the id belongs
/// to somebody else's account.
#[route(GET "/avatar/{user_id}")]
async fn avatar(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let target: &str = path_param::<UserId>(cx);

    let user = match require_user(cx).await {
        Ok(user) => user,
        // An `<img>` has no page to carry a refusal on; 401 names the fix
        // the way the live channel's does.
        Err(_) => return Ok((StatusCode::UNAUTHORIZED, HeaderMap::new(), Vec::new())),
    };
    // v1: one's own face only — anyone else's id reads exactly like no
    // photo, so one account's face can never be probed through another's.
    if target != user.id {
        return Ok(not_found());
    }
    let Some((bytes, mime)) = in_client::photo_for(cx, &user.oidc_sub).await else {
        return Ok(not_found());
    };

    let etag = format!("\"{:x}\"", crate::files::fnv1a(&bytes));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")),
    );
    // The bytes are im's to version and this URL carries no stamp of its
    // own, so a changed photo may take up to a year to show — the price of
    // proxying without a stamp channel, and the same trade the thumbnail
    // route already makes. `private` because the route is gated and a shared
    // proxy must not answer a stranger from another account's entry.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );

    let if_none_match = request_headers(cx)
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    if if_none_match == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, headers, Vec::new()));
    }
    Ok((StatusCode::OK, headers, bytes))
}
