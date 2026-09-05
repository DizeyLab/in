//! File bytes, the viewer page, and small uploads.
//!
//! `GET /file/{id}` streams one file its owner may see — or a grantee may,
//! when the grant says so — honoring `?dl=1` (download disposition) and
//! `Range` (206 partial content), stamped `Cache-Control: private,
//! immutable`. `GET /thumb/{id}` serves the webp thumbnail under the same
//! visibility. `GET /view/{id}` renders the viewer page: the inline viewer
//! for the file's kind (`viewer_kind`), a download link while the reader
//! may download, and its details. `POST /files` (multipart) takes files
//! through the same pipeline the chunked protocol uses — each part capped
//! at the instance upload ceiling first — the store sanitises the name,
//! sniffs the mime, checks the quota
//! and attempts the thumbnail — and `POST /api/file/rename|move|delete`
//! mutate the row. Bytes for another owner's file answer 404, not 403.

use in_core::store::{File, ShareKind, Store, StoreError, ThumbState};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::multipart::Multipart;
use topcoat::router::content::{Form, Json};
use topcoat::router::error::not_found as page_not_found;
use topcoat::router::request::headers as request_headers;
use topcoat::router::{
    HeaderMap, HeaderName, HeaderValue, StatusCode, header, page, path_param, query_params, route,
};
use topcoat::view::view;

use crate::i18n::{Key, lang, t};
use crate::layout::{NavPage, topbar};
use crate::server::{Refusal, app, back_to, require_user};

path_param!(id);

/// A [`StoreError`] in the files' own words. Cross-owner answers
/// [`Refusal::NotFound`], same as a missing id.
fn store_refusal(error: StoreError) -> Refusal {
    match error {
        StoreError::QuotaExceeded => Refusal::QuotaExceeded,
        StoreError::NotFound | StoreError::CrossOwner => Refusal::NotFound,
        StoreError::UploadExpired => Refusal::UploadExpired,
        StoreError::BadChunk => Refusal::BadChunk,
        _ => Refusal::Unavailable,
    }
}

/// A 303 with no body.
fn redirect_to(location: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, value);
    }
    (StatusCode::SEE_OTHER, headers, Vec::new())
}

/// Back to the folder the upload was posted from, carrying the refusal on
/// the query the way a browser without script reads it.
fn back_to_folder(
    folder_id: Option<&str>,
    refusal: Option<Refusal>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut location = match folder_id.filter(|id| !id.is_empty()) {
        Some(id) => format!("/drive?folder={id}"),
        None => "/drive".to_string(),
    };
    if let Some(refusal) = refusal {
        let sep = if location.contains('?') { '&' } else { '?' };
        location.push_str(&format!("{sep}refusal={}&on=upload", refusal.code()));
    }
    redirect_to(&location)
}

fn not_found() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// Whether the browser renders this stored mime on its own rather than only
/// offering to save it. Trusts the stored mime alone; nothing here sniffs
/// bytes a second time. `image/heic` is excluded: no engine decodes it, so
/// it stays a download link rather than a broken image.
fn renders_inline(mime: &str) -> bool {
    if mime == "image/heic" {
        return false;
    }
    mime.starts_with("image/")
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
        || mime == "application/pdf"
        || mime == "text/plain"
}

/// Whether `inline` disposition is safe for this mime: an uploaded HTML or
/// SVG file stays `attachment` either way, so it is offered to save rather
/// than run on In's origin.
fn inline_ok(mime: &str) -> bool {
    renders_inline(mime) && mime != "image/svg+xml" && mime != "text/html"
}

/// The `Content-Disposition` for one download. The ASCII fallback keeps only
/// characters no quoting scheme could turn into a delimiter or a control
/// character; `filename*` carries the real name, percent-encoded.
fn disposition_of(file_name: &str, inline: bool) -> String {
    let kind = if inline { "inline" } else { "attachment" };
    let ascii: String = file_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for &byte in file_name.as_bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-') {
            encoded.push(c);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// A single-range `Range: bytes=...` request resolved against `total` bytes,
/// as `(start, end)` inclusive. `None` for anything this does not parse as
/// exactly one `bytes=` range (a multi-range header included) — the caller
/// falls back to a full `200`, which is always a valid answer to a `Range`
/// request. `Err(())` for an unsatisfiable range, so the caller can answer
/// `416` instead of serving nonsense bytes.
fn parse_range(header: &str, total: u64) -> Option<std::result::Result<(u64, u64), ()>> {
    let spec = header.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        // A suffix range: the last N bytes.
        let suffix: u64 = end.parse().ok()?;
        if suffix == 0 || total == 0 {
            return Some(Err(()));
        }
        let start = total.saturating_sub(suffix);
        return Some(Ok((start, total - 1)));
    }
    let start: u64 = start.parse().ok()?;
    if end.is_empty() {
        if start >= total {
            return Some(Err(()));
        }
        return Some(Ok((start, total - 1)));
    }
    let end: u64 = end.parse().ok()?;
    if start > end || start >= total {
        return Some(Err(()));
    }
    Some(Ok((start, end.min(total - 1))))
}

/// A cheap, non-cryptographic hash — good enough for an `ETag` on bytes only
/// this server ever writes.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[query_params]
struct DownloadQuery {
    dl: Option<String>,
}

/// The file row this account may open: its owner may, and anyone holding a
/// live grant onto it may. Trashed files open for nobody here — the trash
/// has its own screen. `None` folds "no such file" and "not shared" into
/// one answer.
async fn visible_file(
    store: &dyn Store,
    user_id: &str,
    file_id: &str,
) -> Option<in_core::store::File> {
    let file = store.file(file_id).await.ok()??;
    if file.deleted_at.is_some() {
        return None;
    }
    if file.owner_id == user_id {
        return Some(file);
    }
    if store
        .can_see(ShareKind::File, file_id, user_id)
        .await
        .unwrap_or(false)
    {
        return Some(file);
    }
    None
}

/// Serves one file's bytes, or the same not-found a stranger would see for
/// a file that does not exist — never a `403`, which would confirm the id
/// belongs to somebody else's drive. Only `?dl=1` counts a download: the
/// viewer reads the same bytes inline, and looking is not downloading.
#[route(GET "/file/{id}")]
async fn download(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let id: &str = path_param::<Id>(cx);

    let user = match require_user(cx).await {
        Ok(user) => user,
        // A byte route has no page to carry a refusal on: back to the
        // front door, where the sign-in card says what to do.
        Err(_) => return Ok(redirect_to("/")),
    };

    let store = app(cx).store.clone();
    let Some(file) = visible_file(store.as_ref(), &user.id, id).await else {
        return Ok(not_found());
    };
    // A view-only grant opens the file's page but not its bytes.
    if file.owner_id != user.id
        && !store
            .can_download(ShareKind::File, id, &user.id)
            .await
            .unwrap_or(false)
    {
        return Ok(not_found());
    }
    let Ok(Some(bytes)) = store.file_bytes(id).await else {
        return Ok(not_found());
    };

    let forced = query_params::<DownloadQuery>(cx)
        .ok()
        .and_then(|query| query.dl.clone())
        .is_some();
    let inline = !forced && inline_ok(&file.mime);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&file.mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition_of(&file.name, inline))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    // Safari refuses to play a `<video>`/`<audio>` element without a `206`
    // reply to its own `Range` probe; every other engine loses instant seek
    // without one. Sent on every response, not only media — harmless either
    // way and one less type to special-case.
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // A file's bytes are write-once — nothing rewrites a row — so the id in
    // the URL is already its own version. A year of `immutable` costs
    // nothing because the URL can never outlive its bytes; `private` keeps
    // a shared proxy from handing one drive's files to another.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );

    let total = bytes.len() as u64;
    let range = request_headers(cx)
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    match range.and_then(|range| parse_range(range, total)) {
        Some(Ok((start, end))) => {
            // Only a real download counts — `?dl=1` — never the viewer's
            // inline bytes. A resumed download counts once, on its first
            // chunk; later chunks are the same download going on.
            if forced && start == 0 {
                let _ = store.record_download(id).await;
            }
            let slice = bytes[start as usize..=end as usize].to_vec();
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap(),
            );
            Ok((StatusCode::PARTIAL_CONTENT, headers, slice))
        }
        Some(Err(())) => {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).unwrap(),
            );
            Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers, Vec::new()))
        }
        None => {
            if forced {
                let _ = store.record_download(id).await;
            }
            Ok((StatusCode::OK, headers, bytes))
        }
    }
}

/// Serves one file's webp thumbnail under the same visibility as the file
/// itself — a listing shows thumbnails wherever it shows names, and names
/// need only `can_see`.
#[route(GET "/thumb/{id}")]
async fn thumb(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let id: &str = path_param::<Id>(cx);

    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(_) => return Ok(not_found()),
    };

    let store = app(cx).store.clone();
    let Some(_) = visible_file(store.as_ref(), &user.id, id).await else {
        return Ok(not_found());
    };
    let Ok(Some(bytes)) = store.thumb_bytes(id).await else {
        return Ok(not_found());
    };

    let etag = format!("\"{:x}\"", fnv1a(&bytes));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")),
    );
    // The bytes are content-addressed by the file id and never rewritten —
    // a changed picture is a changed file — so a year of `immutable` never
    // shows a stale thumbnail. `private` because the route is gated and a
    // shared proxy must not answer a stranger from another drive's entry.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"));

    let if_none_match = request_headers(cx)
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    if if_none_match == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, headers, Vec::new()));
    }
    Ok((StatusCode::OK, headers, bytes))
}

/// What the viewer page shows inline: images, video and audio through their
/// native elements, PDFs through the browser's reader, plain text through a
/// sandboxed frame. Anything else — archives, office documents, unknown
/// bytes — is download-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewerKind {
    Image,
    Video,
    Audio,
    Pdf,
    Text,
}

/// The inline viewer for one stored mime, if it has one. Trusts the stored
/// mime alone, the way [`renders_inline`] does; `image/heic` stays
/// download-only because no engine decodes it, and only `text/plain` opens
/// as text — anything juicier stays a download, the way [`inline_ok`]
/// keeps it off this origin.
pub(crate) fn viewer_kind(mime: &str) -> Option<ViewerKind> {
    if mime == "image/heic" {
        None
    } else if mime.starts_with("image/") {
        Some(ViewerKind::Image)
    } else if mime.starts_with("video/") {
        Some(ViewerKind::Video)
    } else if mime.starts_with("audio/") {
        Some(ViewerKind::Audio)
    } else if mime == "application/pdf" {
        Some(ViewerKind::Pdf)
    } else if mime == "text/plain" {
        Some(ViewerKind::Text)
    } else {
        None
    }
}

/// One file's list icon: the webp thumbnail where one is ready, else a
/// mime-class glyph. The glyphs are decorative — the file's name beside
/// them carries the meaning — so they hide from assistive technology.
///
/// Lists embed this beside the name: `(crate::files::entry_chip(cx,
/// file).await?)`.
pub(crate) async fn entry_chip(cx: &Cx, file: &File) -> Result {
    if file.thumb_state == ThumbState::Ready {
        return view! {
            cx =>
            <img class="file-chip" src=(format!("/thumb/{}", file.id)) alt="">
        };
    }
    let (class, glyph) = chip_mark(&file.mime);
    view! {
        cx =>
        <span class=(format!("file-chip file-chip-{class}")) aria-hidden="true">(glyph)</span>
    }
}

/// The chip's modifier class and its glyph, by stored mime: one distinct
/// mark per class — image, video, audio, pdf, text, archive, generic.
fn chip_mark(mime: &str) -> (&'static str, &'static str) {
    if mime.starts_with("image/") {
        ("image", "◫")
    } else if mime.starts_with("video/") {
        ("video", "▶")
    } else if mime.starts_with("audio/") {
        ("audio", "♫")
    } else if mime == "application/pdf" {
        ("pdf", "☰")
    } else if mime.starts_with("text/") {
        ("text", "¶")
    } else if is_archive_mime(mime) {
        ("archive", "⊞")
    } else {
        ("generic", "▦")
    }
}

/// The stored mimes that are bundles of other files: plain zips, the OOXML
/// and OpenDocument documents (all zips underneath), gzip blobs and the
/// legacy OLE container.
fn is_archive_mime(mime: &str) -> bool {
    mime == "application/zip"
        || mime == "application/gzip"
        || mime == "application/x-ole-storage"
        || mime == "application/vnd.ms-excel"
        || mime.contains("openxml")
        || mime.contains("opendocument")
}

/// `GET /view/{id}`: one file's viewer page — its name, the inline viewer
/// for its kind, a download link and its details. Authorization mirrors
/// trashed files open for nobody. A view-only grant opens the page but not
/// the bytes: the download link shows only while the reader may download,
/// and the inline viewer gives way to the unavailable note, since every
/// viewer element reads the same gated bytes. Bytes for another owner's
/// file answer 404, not 403.
#[page("/view/{id}")]
async fn view_file(cx: &Cx) -> Result {
    let id: &str = path_param::<Id>(cx);

    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(_) => {
            let location = (header::LOCATION, HeaderValue::from_static("/"));
            return view! {
                cx =>
                (StatusCode::SEE_OTHER)
                (location)
            };
        }
    };
    let language = lang(cx).await;

    let store = app(cx).store.clone();
    let Some(file) = visible_file(store.as_ref(), &user.id, id).await else {
        return Err(page_not_found().into());
    };
    let may_download = file.owner_id == user.id
        || store
            .can_download(ShareKind::File, &file.id, &user.id)
            .await
            .unwrap_or(false);

    let src = format!("/file/{}", file.id);
    let download_href = format!("/file/{}?dl=1", file.id);
    let meta = format!(
        "{} · {}",
        file.mime,
        crate::settings::human_bytes(file.size_bytes)
    );
    let uploaded = file.created_at.date().to_string();
    view! {
        cx =>
        (topbar(cx, NavPage::Drive, &user, language).await?)
        <main class="settings-stage stage-wide">
            <div class="viewer-head">
                <a class="quiet" href="/drive">(t(language, Key::BackToDrive))</a>
                <div class="spacer"></div>
                if may_download {
                    <a class="primary" href=(download_href)>(t(language, Key::Download))</a>
                }
            </div>
            <h1 class="settings-title viewer-title">(entry_chip(cx, &file).await?) (file.name.clone())</h1>
            <p class="field-note">(meta)</p>
            <div class="viewer-stage">
                if may_download {
                    match viewer_kind(&file.mime) {
                        Some(ViewerKind::Image) => <img class="viewer-media" src=(src.clone()) alt=(file.name.clone())>,
                        Some(ViewerKind::Video) => <video class="viewer-media" src=(src.clone()) controls="" preload="metadata"></video>,
                        Some(ViewerKind::Audio) => <div class="viewer-audio-wrap"><audio class="viewer-audio" src=(src.clone()) controls="" preload="metadata"></audio></div>,
                        Some(ViewerKind::Pdf) => <object class="viewer-media viewer-pdf" data=(src.clone()) type="application/pdf">
                            <p class="field-note">(t(language, Key::PreviewUnavailable))</p>
                        </object>,
                        Some(ViewerKind::Text) => <iframe class="viewer-media viewer-text" src=(src.clone()) sandbox="" title=(file.name.clone())></iframe>,
                        None => <div class="viewer-fallback">
                            <p class="field-note">(t(language, Key::PreviewUnavailable))</p>
                        </div>
                    }
                } else {
                    // A view-only grant opens the page but not the bytes, so
                    // the viewer elements — every one reads /file/{id} —
                    // would only frame a 404. The note instead.
                    <div class="viewer-fallback">
                        <p class="field-note">(t(language, Key::PreviewUnavailable))</p>
                    </div>
                }
            </div>
            <section class="panel viewer-details">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::FileDetails))</h2>
                </div>
                <div class="panel-body">
                    <div class="field">
                        <span class="field-label">(t(language, Key::SizeColumn))</span>
                        <span class="field-static">(crate::settings::human_bytes(file.size_bytes))</span>
                    </div>
                    <div class="field">
                        <span class="field-label">(t(language, Key::UploadedLabel))</span>
                        <span class="field-static">(uploaded)</span>
                    </div>
                    <div class="field">
                        <span class="field-label">(t(language, Key::DownloadsLabel))</span>
                        <span class="field-static">(file.download_count.to_string())</span>
                    </div>
                </div>
            </section>
        </main>
    }
}

/// Takes the files the drive's upload form carries — one per `file` part,
/// and a `multiple` picker posts several. The route's body guard sits at 2
/// GiB (`main.rs`); each part is buffered whole because the store writes
/// it in one go, capped at the instance upload ceiling before it gets
/// there. Fields arrive in request order:
/// `folder_id` before the `file` parts, so the destination is known before
/// a byte of any file is kept.
#[route(POST "/files")]
async fn upload(
    cx: &Cx,
    mut multipart: Multipart,
) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(back_to_folder(None, Some(refusal))),
    };
    let store = app(cx).store.clone();

    let mut folder_id: Option<String> = None;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => {
                return Ok(back_to_folder(
                    folder_id.as_deref(),
                    Some(Refusal::Unavailable),
                ));
            }
        };
        match field.name() {
            Some("folder_id") => {
                folder_id = field.text().await.ok().filter(|value| !value.is_empty());
            }
            Some("file") => {
                let Some(name) = field.file_name().map(str::to_string) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                let mut field = field;
                let mut collected = Vec::new();
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => collected.extend_from_slice(&chunk),
                        Ok(None) => break,
                        Err(_) => {
                            return Ok(back_to_folder(
                                folder_id.as_deref(),
                                Some(Refusal::Unavailable),
                            ));
                        }
                    }
                }
                files.push((name, collected));
            }
            _ => {}
        }
    }

    // The destination must be an owned, live folder — or the root.
    if let Some(folder) = folder_id.as_deref() {
        match store.folder(folder).await {
            Ok(Some(row)) if row.owner_id == user.id && row.deleted_at.is_none() => {}
            _ => return Ok(back_to_folder(None, Some(Refusal::NotFound))),
        }
    }
    if files.is_empty() {
        return Ok(back_to_folder(folder_id.as_deref(), None));
    }

    let limit = crate::server::effective_upload_limit(cx).await;

    let mut refusal: Option<Refusal> = None;
    for (name, bytes) in files {
        // One file may not pass the instance ceiling; oversize files never
        // reach the store.
        if bytes.len() as u64 > limit {
            refusal = Some(Refusal::UploadTooLarge);
            break;
        }
        // The store sanitises the label, sniffs the mime off the bytes,
        // checks the quota, writes the file and attempts the thumbnail.
        if let Err(error) = store
            .insert_file(&user.id, folder_id.as_deref(), &name, &bytes)
            .await
        {
            refusal = Some(store_refusal(error));
            break;
        }
    }
    Ok(back_to_folder(folder_id.as_deref(), refusal))
}

/// A 303 back to the page the form was posted from, carrying the refusal as
/// the body for `carry_refusal_on_redirect` to copy onto the query.
type Redirect = Result<(StatusCode, [(HeaderName, String); 1], Json<Option<Refusal>>)>;

fn redirect(cx: &Cx, refusal: Option<Refusal>) -> Redirect {
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, back_to(cx, "/drive"))],
        Json(refusal),
    ))
}

/// The owned, live file the form names, or the refusal the caller returns
/// as-is. Another owner's file is not found rather than forbidden.
async fn owned_file(cx: &Cx, id: &str) -> std::result::Result<in_core::store::File, Redirect> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Err(redirect(cx, Some(refusal))),
    };
    let store = app(cx).store.clone();
    match store.file(id).await {
        Ok(Some(file)) if file.owner_id == user.id && file.deleted_at.is_none() => Ok(file),
        _ => Err(redirect(cx, Some(Refusal::NotFound))),
    }
}

#[derive(serde::Deserialize)]
struct FileIdForm {
    id: String,
}

#[derive(serde::Deserialize)]
struct RenameFileForm {
    id: String,
    name: String,
}

#[derive(serde::Deserialize)]
struct MoveFileForm {
    id: String,
    #[serde(default)]
    folder_id: String,
}

/// `POST /api/file/rename`: a new label on an owned file.
#[route(POST "/api/file/rename")]
async fn rename_file(cx: &Cx, Form(input): Form<RenameFileForm>) -> Redirect {
    let file = match owned_file(cx, &input.id).await {
        Ok(file) => file,
        Err(answer) => return answer,
    };
    let store = app(cx).store.clone();
    match store.rename_file(&file.id, &input.name).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}

/// `POST /api/file/move`: an owned file into an owned folder, or the root.
#[route(POST "/api/file/move")]
async fn move_file(cx: &Cx, Form(input): Form<MoveFileForm>) -> Redirect {
    let file = match owned_file(cx, &input.id).await {
        Ok(file) => file,
        Err(answer) => return answer,
    };
    let store = app(cx).store.clone();
    let folder = if input.folder_id.is_empty() {
        None
    } else {
        Some(input.folder_id.as_str())
    };
    if let Some(id) = folder {
        let user = match require_user(cx).await {
            Ok(user) => user,
            Err(refusal) => return redirect(cx, Some(refusal)),
        };
        match store.folder(id).await {
            Ok(Some(row)) if row.owner_id == user.id && row.deleted_at.is_none() => {}
            _ => return redirect(cx, Some(Refusal::NotFound)),
        }
    }
    match store.move_file(&file.id, folder).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}

/// `POST /api/file/delete`: trash an owned file. The bytes stay on disk —
/// and counting toward quota — until the trash purges the row.
#[route(POST "/api/file/delete")]
async fn delete_file(cx: &Cx, Form(input): Form<FileIdForm>) -> Redirect {
    let file = match owned_file(cx, &input.id).await {
        Ok(file) => file,
        Err(answer) => return answer,
    };
    let store = app(cx).store.clone();
    match store.delete_file(&file.id).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}
