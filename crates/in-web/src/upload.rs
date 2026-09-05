//! The chunked-upload protocol.
//!
//! Four owner-only JSON calls, all UTF-8: `POST /api/upload/start`
//! `{folder_id, name, size_bytes}` answers `{id, chunk_size}` with the
//! server-fixed 8 MiB chunk size (`Refusal::QuotaExceeded` when the quota
//! says no, `Refusal::UploadTooLarge` past the instance upload ceiling);
//! `PUT /api/upload/{id}/{index}` takes one raw chunk (32 MiB cap, layered
//! in `main.rs` — the real per-file ceiling lives in the start handler);
//! `POST /api/upload/{id}/finish` assembles the
//! staged chunks, sniffs the mime, moves the bytes to `files/<new file id>`
//! and answers with the created file id; `POST /api/upload/{id}/abort`
//! drops the session and its staged bytes. Quota is enforced at start *and*
//! at finish. The UI picks this path at 8 MiB and over; below that it posts
//! straight to `POST /files`.
//!
//! Answers are `200` JSON throughout — `{ok: ...}` or `{err: <refusal>}` —
//! because the caller is script, not a form: there is no page to redirect
//! back to, and the progress bar reads the refusal straight off the body.

use in_core::store::StoreError;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Json;
use topcoat::router::request::{Bytes, uri};
use topcoat::router::{path_param, route};

use crate::server::{Refusal, app, require_user};

path_param!(id);

/// What the script side reads: the value, or the refusal it failed with.
/// Serialises as `{"ok":...}` / `{"err":...}` — a hand-rolled enum because
/// serde's own `Result` would say `Ok`/`Err`, which the UI script does not
/// read.
enum Answer<T> {
    Ok(T),
    Err(Refusal),
}

impl<T: serde::Serialize> serde::Serialize for Answer<T> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Answer::Ok(value) => map.serialize_entry("ok", value)?,
            Answer::Err(refusal) => map.serialize_entry("err", refusal)?,
        }
        map.end()
    }
}

/// A [`StoreError`] in the upload's own words. Another owner's session id
/// answers [`Refusal::NotFound`]: the id space is not theirs to probe.
fn store_refusal(error: StoreError) -> Refusal {
    match error {
        StoreError::QuotaExceeded => Refusal::QuotaExceeded,
        StoreError::UploadExpired => Refusal::UploadExpired,
        StoreError::BadChunk => Refusal::BadChunk,
        StoreError::NotFound | StoreError::CrossOwner => Refusal::NotFound,
        StoreError::NameTaken => Refusal::NameTaken,
        _ => Refusal::Unavailable,
    }
}

/// The owned, active session the call names, or the refusal its answer
/// carries. Another owner's session is not found rather than forbidden.
async fn owned_session(
    cx: &Cx,
    id: &str,
) -> std::result::Result<in_core::store::UploadSession, Refusal> {
    let user = require_user(cx).await?;
    let store = app(cx).store.clone();
    match store.upload_session(id).await {
        Ok(Some(session)) if session.owner_id == user.id => Ok(session),
        Ok(_) => Err(Refusal::NotFound),
        Err(_) => Err(Refusal::Unavailable),
    }
}

#[derive(serde::Deserialize)]
struct StartIn {
    folder_id: Option<String>,
    name: String,
    size_bytes: u64,
}

#[derive(serde::Serialize)]
struct StartOut {
    id: String,
    chunk_size: u64,
}

/// Opens an upload session after the quota pre-check: the declared total
/// must fit the remaining quota, or the browser learns before sending a
/// single chunk.
#[route(POST "/api/upload/start")]
async fn start(cx: &Cx, Json(input): Json<StartIn>) -> Result<Json<Answer<StartOut>>> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Json(Answer::Err(refusal))),
    };
    let store = app(cx).store.clone();
    let folder = input.folder_id.as_deref().filter(|id| !id.is_empty());
    if let Some(id) = folder {
        match store.folder(id).await {
            Ok(Some(row)) if row.owner_id == user.id && row.deleted_at.is_none() => {}
            Ok(_) => return Ok(Json(Answer::Err(Refusal::NotFound))),
            Err(_) => return Ok(Json(Answer::Err(Refusal::Unavailable))),
        }
    }
    // The instance ceiling is checked against the declared total before a
    // single chunk is sent: chunks can never add past the declared total
    // (the store refuses that), so this one check caps the whole session.
    if input.size_bytes > crate::server::effective_upload_limit(cx).await {
        return Ok(Json(Answer::Err(Refusal::UploadTooLarge)));
    }
    match store
        .create_upload_session(&user.id, folder, &input.name, input.size_bytes)
        .await
    {
        Ok(session) => Ok(Json(Answer::Ok(StartOut {
            id: session.id,
            chunk_size: session.chunk_size,
        }))),
        Err(error) => Ok(Json(Answer::Err(store_refusal(error)))),
    }
}

#[derive(serde::Serialize)]
struct ChunkOut {
    received_bytes: u64,
    size_bytes: u64,
}

/// Stages one raw chunk. The store refuses a wrong-sized or out-of-range
/// piece with [`StoreError::BadChunk`] and an expired session with
/// [`StoreError::UploadExpired`]; this layer only gates the owner.
#[route(PUT "/api/upload/{id}/{index}")]
async fn chunk(cx: &Cx, body: Bytes) -> Result<Json<Answer<ChunkOut>>> {
    let id: &str = path_param::<Id>(cx);
    let index: u64 = match uri(cx)
        .path()
        .rsplit('/')
        .next()
        .and_then(|raw| raw.parse().ok())
    {
        Some(index) => index,
        None => return Ok(Json(Answer::Err(Refusal::BadChunk))),
    };
    let session = match owned_session(cx, id).await {
        Ok(session) => session,
        Err(refusal) => return Ok(Json(Answer::Err(refusal))),
    };
    let store = app(cx).store.clone();
    match store.record_chunk(&session.id, index, &body).await {
        Ok(session) => Ok(Json(Answer::Ok(ChunkOut {
            received_bytes: session.received_bytes,
            size_bytes: session.size_bytes,
        }))),
        Err(error) => Ok(Json(Answer::Err(store_refusal(error)))),
    }
}

/// Assembles the staged chunks — rechecking the quota, sniffing the mime
/// off the bytes, attempting the thumbnail — and answers the created file
/// id. The chunks stay staged on refusal, so the browser may retry.
#[route(POST "/api/upload/{id}/finish")]
async fn finish(cx: &Cx) -> Result<Json<Answer<String>>> {
    let id: &str = path_param::<Id>(cx);
    let session = match owned_session(cx, id).await {
        Ok(session) => session,
        Err(refusal) => return Ok(Json(Answer::Err(refusal))),
    };
    let store = app(cx).store.clone();
    match store.finish_upload(&session.id).await {
        Ok(file) => Ok(Json(Answer::Ok(file.id))),
        Err(error) => Ok(Json(Answer::Err(store_refusal(error)))),
    }
}

/// Aborts a session: the row says `aborted` and the staged chunks go.
/// Aborting twice — or aborting nothing — is not an error.
#[route(POST "/api/upload/{id}/abort")]
async fn abort(cx: &Cx) -> Result<Json<Answer<()>>> {
    let id: &str = path_param::<Id>(cx);
    let session = match owned_session(cx, id).await {
        Ok(session) => session,
        Err(refusal) => return Ok(Json(Answer::Err(refusal))),
    };
    let store = app(cx).store.clone();
    match store.abort_upload(&session.id).await {
        Ok(_) => Ok(Json(Answer::Ok(()))),
        Err(error) => Ok(Json(Answer::Err(store_refusal(error)))),
    }
}
