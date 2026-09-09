//! The machine API: the four routes a family service drives with its bearer
//! key — push a file under its own handle, fetch it back, take it away, and
//! read its quota. Every answer is 200 JSON in the house `{"ok": ...}` /
//! `{"err": <word>}` shape, except a missing or spent key, which answers
//! `401 {"err":"Unauthorized"}` — one word, because telling a stranger which
//! keys exist is the leak the shape exists to prevent.
//!
//! A human browser's session cookie means nothing on these routes, and a
//! bearer key means nothing on the human ones: [`crate::server::service_client`]
//! never reads a cookie, and [`crate::server::current_user`] never reads the
//! `Authorization` header.

use in_core::store::{Store, StoreError};
use serde::Serialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Json;
use topcoat::router::content::multipart::Multipart;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, path_param, route};

use crate::server::{Refusal, app, service_client};

path_param!(external_id);

/// The machine answer: one flat JSON object, whatever the call owes. The
/// status route carries its facts beside `ok`, so the shape is a map built
/// by hand, not serde's `Result`.
enum Reply {
    /// `401 {"err":"Unauthorized"}`.
    Unauthorized,
    /// `200 {"err": <word>}` — the word the caller's contract names.
    Err(&'static str),
    /// `200 {"ok": <value>}` — a file id, or `true`.
    Ok(serde_json::Value),
    /// `200 {"ok":true,"quota_bytes":N,"used_bytes":N}`.
    Status { quota_bytes: u64, used_bytes: u64 },
}

impl Serialize for Reply {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            Reply::Unauthorized => map.serialize_entry("err", "Unauthorized")?,
            Reply::Err(word) => map.serialize_entry("err", word)?,
            Reply::Ok(value) => map.serialize_entry("ok", value)?,
            Reply::Status {
                quota_bytes,
                used_bytes,
            } => {
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("quota_bytes", quota_bytes)?;
                map.serialize_entry("used_bytes", used_bytes)?;
            }
        }
        map.end()
    }
}

type ServiceReply = Result<(StatusCode, Json<Reply>)>;

fn unauthorized() -> ServiceReply {
    Ok((StatusCode::UNAUTHORIZED, Json(Reply::Unauthorized)))
}

fn refused(word: &'static str) -> ServiceReply {
    Ok((StatusCode::OK, Json(Reply::Err(word))))
}

fn served(value: serde_json::Value) -> ServiceReply {
    Ok((StatusCode::OK, Json(Reply::Ok(value))))
}

fn not_found_bytes() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// The byte-route 401: no body worth parsing, the status is the answer.
fn unauthorized_bytes() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::UNAUTHORIZED, HeaderMap::new(), Vec::new())
}

/// `POST /api/service/files` — one file under the service's own handle.
/// Multipart fields `external_id` and `name`, then the bytes as the `file`
/// part. A second push of a known `external_id` is the same file with new
/// bytes; the quota refusal is the one the contract names.
#[route(POST "/api/service/files")]
async fn service_upload(cx: &Cx, mut multipart: Multipart) -> ServiceReply {
    let Some(account) = service_client(cx).await else {
        return unauthorized();
    };
    let mut external_id: Option<String> = None;
    let mut name: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return refused("Unavailable"),
        };
        match field.name() {
            Some("external_id") => {
                external_id = field.text().await.ok().filter(|value| !value.is_empty());
            }
            Some("name") => {
                name = field.text().await.ok().filter(|value| !value.is_empty());
            }
            Some("file") => {
                let mut field = field;
                let mut collected = Vec::new();
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => collected.extend_from_slice(&chunk),
                        Ok(None) => break,
                        Err(_) => return refused("Unavailable"),
                    }
                }
                bytes = Some(collected);
            }
            _ => {}
        }
    }
    let (Some(external_id), Some(name), Some(bytes)) = (external_id, name, bytes) else {
        return refused("BadRequest");
    };
    let store = app(cx).store.clone();
    match store
        .insert_service_file(&account.id, &external_id, &name, &bytes)
        .await
    {
        Ok(id) => served(serde_json::Value::String(id)),
        Err(StoreError::QuotaExceeded) => refused("QuotaExceeded"),
        Err(_) => refused("Unavailable"),
    }
}

/// `GET /api/service/file/{external_id}` — the bytes, under the same
/// download headers the human drive serves: the stored name as attachment,
/// and a year of `immutable`, because the handle can never outlive its
/// bytes. A handle that names nothing answers the same 404 a stranger's
/// file id would.
#[route(GET "/api/service/file/{external_id}")]
async fn service_download(cx: &Cx) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let Some(account) = service_client(cx).await else {
        return Ok(unauthorized_bytes());
    };
    let external_id: &str = path_param::<ExternalId>(cx);
    let store = app(cx).store.clone();
    let Some(file) = store
        .service_file(&account.id, external_id)
        .await
        .ok()
        .flatten()
    else {
        return Ok(not_found_bytes());
    };
    let Some(bytes) = store.file_bytes(&file.id).await.ok().flatten() else {
        return Ok(not_found_bytes());
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{}\"",
            ascii_name(&file.name)
        ))
        .unwrap_or(HeaderValue::from_static("attachment")),
    );
    if let Ok(value) = HeaderValue::from_str(&file.mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok((StatusCode::OK, headers, bytes))
}

/// The ASCII half of a `Content-Disposition` filename: only characters no
/// quoting scheme could turn into a delimiter survive.
fn ascii_name(file_name: &str) -> String {
    file_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `DELETE /api/service/file/{external_id}` — the bytes are gone, the
/// account's usage follows. A machine client's delete is a delete.
#[route(DELETE "/api/service/file/{external_id}")]
async fn service_delete(cx: &Cx) -> ServiceReply {
    let Some(account) = service_client(cx).await else {
        return unauthorized();
    };
    let external_id: &str = path_param::<ExternalId>(cx);
    let store = app(cx).store.clone();
    match store.delete_service_file(&account.id, external_id).await {
        Ok(_) => served(serde_json::Value::Bool(true)),
        Err(_) => refused("Unavailable"),
    }
}

/// `GET /api/service/status` — the account's ceiling and what it holds, the
/// two numbers a service's admin page shows.
#[route(GET "/api/service/status")]
async fn service_status(cx: &Cx) -> ServiceReply {
    let Some(account) = service_client(cx).await else {
        return unauthorized();
    };
    let store = app(cx).store.clone();
    let Ok(Some(fresh)) = store.user(&account.id).await else {
        return refused("Unavailable");
    };
    Ok((
        StatusCode::OK,
        Json(Reply::Status {
            quota_bytes: fresh.quota_bytes,
            used_bytes: fresh.used_bytes,
        }),
    ))
}

/// Takes every service key's ceiling from a freshly fetched family: the
/// limit im states for the service's key, or the house default when im
/// states none. The family beat calls this after every successful fetch; a
/// failure is one log line and nothing else — the next beat re-asks.
pub async fn apply_family_limits(
    store: &std::sync::Arc<dyn Store>,
    family: &[in_client::ServiceJson],
    default_quota_bytes: u64,
) -> Result<(), String> {
    for account in store
        .list_service_accounts()
        .await
        .map_err(|e| e.to_string())?
    {
        let limit = family
            .iter()
            .find(|row| row.key == account.service)
            .and_then(|row| row.limit_bytes);
        store
            .apply_service_limit(&account.service, limit, default_quota_bytes)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The mintable services, for the panel's dropdown and the mint route's
/// gate: named by the family mirror, and not keyed yet.
pub(crate) async fn mintable_services(
    cx: &Cx,
) -> std::result::Result<Vec<in_client::ServiceJson>, Refusal> {
    let store = app(cx).store.clone();
    let family = store
        .get_setting("family")
        .await
        .map_err(Refusal::from)?
        .and_then(|json| serde_json::from_str::<Vec<in_client::ServiceJson>>(&json).ok())
        .unwrap_or_default();
    let keyed: std::collections::HashSet<String> = store
        .list_service_accounts()
        .await
        .map_err(Refusal::from)?
        .into_iter()
        .map(|account| account.service)
        .collect();
    Ok(family
        .into_iter()
        .filter(|row| !keyed.contains(&row.key))
        .collect())
}
