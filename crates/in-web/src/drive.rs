//! The drive browser.
//!
//! `GET /drive?folder=<id>` renders one directory — breadcrumbs back to the
//! root, subfolders then files — and the four owner-only folder mutations
//! ride beside it: `POST /api/folder/create|rename|move|delete`. Trashed
//! items never appear here; they wait in `trash.rs`. A folder whose ancestor
//! is trashed is unreachable, and a move that would nest a folder inside
//! itself is refused. Anything naming another owner's tree answers 404,
//! never 403.

use in_core::store::{Folder, Store, StoreError, ThumbState};
use topcoat::router::request::uri;
use topcoat::router::{HeaderName, HeaderValue, StatusCode, header, page, query_params, route};

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::{Form, Json};
use topcoat::router::error::not_found;
use topcoat::view::view;

use crate::i18n::{Key, lang, t};
use crate::layout::{NavPage, topbar};
use crate::server::{Refusal, app, back_to, refusal_of, require_user};

/// A [`StoreError`] in the drive's own words. Cross-owner reads and writes
/// alike answer [`Refusal::NotFound`]: telling a stranger which ids exist is
/// the leak the distinction exists to prevent.
fn store_refusal(error: StoreError) -> Refusal {
    match error {
        StoreError::NameTaken => Refusal::NameTaken,
        StoreError::QuotaExceeded => Refusal::QuotaExceeded,
        StoreError::NotFound | StoreError::CrossOwner => Refusal::NotFound,
        // A move nesting a folder inside its own descendant.
        StoreError::Cycle => Refusal::Forbidden,
        _ => Refusal::Unavailable,
    }
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

#[query_params]
struct DriveQuery {
    folder: Option<String>,
}

/// The folder the query names, if it names one this account may open: owned,
/// live, and with no trashed ancestor. Anything else is not found rather
/// than forbidden.
async fn current_folder(
    store: &dyn Store,
    user_id: &str,
    folder_id: Option<&str>,
) -> Result<Option<Folder>, topcoat::Error> {
    let Some(id) = folder_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let Ok(Some(folder)) = store.folder(id).await else {
        return Err(not_found().into());
    };
    if folder.owner_id != user_id || folder.deleted_at.is_some() {
        return Err(not_found().into());
    }
    // Unreachable while any ancestor is trashed: the drive shows no path
    // back into it.
    let mut parent = folder.parent_id.clone();
    while let Some(id) = parent {
        match store.folder(&id).await {
            Ok(Some(next)) if next.owner_id == user_id && next.deleted_at.is_none() => {
                parent = next.parent_id.clone();
            }
            _ => return Err(not_found().into()),
        }
    }
    Ok(Some(folder))
}

/// The breadcrumb path, root first, for the open folder.
async fn breadcrumbs(store: &dyn Store, folder: Option<&Folder>) -> Vec<Folder> {
    let mut chain = Vec::new();
    let mut next = folder.cloned();
    while let Some(current) = next {
        let parent = match &current.parent_id {
            Some(id) => store.folder(id).await.ok().flatten(),
            None => None,
        };
        chain.push(current);
        next = parent;
    }
    chain.reverse();
    chain
}

/// Every live folder the account holds, with its root-first display path —
/// the move destination picker. Small trees only; the drive is not a
/// filesystem with ten thousand directories.
async fn folder_tree(store: &dyn Store, user_id: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // (parent id, path prefix)
    let mut queue: Vec<(Option<String>, String)> = vec![(None, String::new())];
    while let Some((parent, prefix)) = queue.pop() {
        let Ok(listing) = store.list_children(user_id, parent.as_deref()).await else {
            continue;
        };
        for folder in listing.folders {
            let path = if prefix.is_empty() {
                folder.name.clone()
            } else {
                format!("{prefix} / {}", folder.name)
            };
            queue.push((Some(folder.id.clone()), path.clone()));
            out.push((folder.id, path));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < 4 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
fn page_refusal(cx: &Cx) -> Option<Refusal> {
    ["create", "rename", "move", "delete", "upload"]
        .iter()
        .find_map(|call| refusal_of(cx, call))
}
/// The just-minted token off the redirect's `?created=` pair, if present.
fn created_token(query: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == "created" && !value.is_empty()).then(|| value.to_string())
    })
}

/// `GET /drive?folder=<id>`: one directory.
#[page("/drive")]
async fn drive(cx: &Cx) -> Result {
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

    let wanted = query_params::<DriveQuery>(cx)
        .ok()
        .and_then(|query| query.folder.clone());
    let current = current_folder(store.as_ref(), &user.id, wanted.as_deref()).await?;
    let crumbs = breadcrumbs(store.as_ref(), current.as_ref()).await;
    let listing = match store
        .list_children(&user.id, current.as_ref().map(|folder| folder.id.as_str()))
        .await
    {
        Ok(listing) => listing,
        Err(_) => return Err(not_found().into()),
    };
    let destinations = folder_tree(store.as_ref(), &user.id).await;
    let refusal = page_refusal(cx);
    // A link minted from this surface redirects back here with the plaintext
    // token on `?created=` — rendered once, like the settings page does.
    let created = created_token(uri(cx).query().unwrap_or(""));
    let origin = app(cx).config.listen_url();

    let current_id = current
        .as_ref()
        .map(|folder| folder.id.clone())
        .unwrap_or_default();
    let current_name = current
        .as_ref()
        .map(|folder| folder.name.clone())
        .unwrap_or_else(|| t(language, Key::Drive).to_string());

    view! {
        cx =>
        (topbar(cx, NavPage::Drive, &user, language).await?)
        <main class="settings-stage">
            <div class="filterbar">
                <nav class="detail-crumbs" aria-label=(current_name.clone())>
                    <a class="detail-crumb" href="/drive">(t(language, Key::Drive))</a>
                    for crumb in crumbs.iter() {
                        <span class="detail-crumb-sep">"/"</span>
                        <a class="detail-crumb" href=(format!("/drive?folder={}", crumb.id))>(crumb.name.clone())</a>
                    }
                </nav>
                <div class="spacer"></div>
                <form class="tag-form" method="post" action="/api/folder/create">
                    <input type="hidden" name="parent_id" value=(current_id.clone())>
                    <input class="field-input" type="text" name="name" maxlength="255"
                        placeholder=(t(language, Key::FolderName)) required="" aria-label=(t(language, Key::FolderName))>
                    <button class="primary" type="submit">(t(language, Key::CreateFolder))</button>
                </form>
            </div>
            if let Some(refusal) = refusal {
                <p class="field-error">(refusal.message_in(language))</p>
            }
            if let Some(token) = created {
                <section class="panel">
                    <h2 class="panel-title">(t(language, Key::LinkCreated))</h2>
                    <div class="panel-body">
                        <p class="field-note">(t(language, Key::CopyLinkOnce))</p>
                        <p class="member-link-value">(format!("{origin}/s/{token}"))</p>
                    </div>
                </section>
            }
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::FoldersHeading))</h2>
                    <span class="chip">(format!("{}", listing.folders.len()))</span>
                </div>
                <div class="panel-body">
                    if listing.folders.is_empty() {
                        <p class="detail-quiet">(t(language, Key::EmptyFolder))</p>
                    }
                    for folder in listing.folders.iter() {
                        <div class="dep-row">
                            <a class="dep-link" href=(format!("/drive?folder={}", folder.id))>(folder.name.clone())</a>
                            <div class="spacer"></div>
                            <form class="pop-row-form" method="post" action="/api/folder/rename">
                                <input type="hidden" name="id" value=(folder.id.clone())>
                                <input class="field-input" type="text" name="name" maxlength="255"
                                    value=(folder.name.clone()) required="" aria-label=(t(language, Key::RenameFolder))>
                                <button class="quiet" type="submit">(t(language, Key::Rename))</button>
                            </form>
                            <form class="pop-row-form" method="post" action="/api/folder/move">
                                <input type="hidden" name="id" value=(folder.id.clone())>
                                <select class="field-input" name="parent_id" aria-label=(t(language, Key::MoveFolder))>
                                    <option value="">(t(language, Key::Drive))</option>
                                    for dest in destinations.iter() {
                                        if dest.0 != folder.id {
                                            <option value=(dest.0.clone())>(dest.1.clone())</option>
                                        }
                                    }
                                </select>
                                <button class="quiet" type="submit">(t(language, Key::Move))</button>
                            </form>
                            <form class="detail-delete-form" method="post" action="/api/folder/delete">
                                <input type="hidden" name="id" value=(folder.id.clone())>
                                <button class="quiet" type="submit">(t(language, Key::Delete))</button>
                            </form>
                        </div>
                    }
                </div>
            </section>
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::FilesHeading))</h2>
                    <span class="chip">(format!("{}", listing.files.len()))</span>
                </div>
                <div class="panel-body">
                    <form id="upload-form" class="file-upload" method="post" action="/files"
                        enctype="multipart/form-data" data-hard="" data-failed-label=(t(language, Key::UploadFailed))>
                        <input type="hidden" name="folder_id" value=(current_id.clone())>
                        <label class="file-upload-box">
                            <span class="file-upload-name">(t(language, Key::ChooseFiles))</span>
                            <input class="file-upload-input" type="file" name="file" multiple="">
                        </label>
                        <span class="field-note">(t(language, Key::DropFilesHere))</span>
                    </form>
                    <div class="file-list">
                        for file in listing.files.iter() {
                            <div class="dep-row">
                                if file.thumb_state == ThumbState::Ready {
                                    <img class="file-chip" src=(format!("/thumb/{}", file.id)) alt="">
                                } else {
                                    <span class="file-chip" aria-hidden="true">"▦"</span>
                                }
                                <a class="dep-link" href=(format!("/file/{}", file.id))>(file.name.clone())</a>
                                <span class="file-chip-size">(human_size(file.size_bytes))</span>
                                <div class="spacer"></div>
                                <form class="pop-row-form" method="post" action="/api/file/rename">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <input class="field-input" type="text" name="name" maxlength="255"
                                        value=(file.name.clone()) required="" aria-label=(t(language, Key::RenameFile))>
                                    <button class="quiet" type="submit">(t(language, Key::Rename))</button>
                                </form>
                                <form class="pop-row-form" method="post" action="/api/file/move">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <select class="field-input" name="folder_id" aria-label=(t(language, Key::MoveFile))>
                                        <option value="">(t(language, Key::Drive))</option>
                                        for dest in destinations.iter() {
                                            <option value=(dest.0.clone())>(dest.1.clone())</option>
                                        }
                                    </select>
                                    <button class="quiet" type="submit">(t(language, Key::Move))</button>
                                </form>
                                <form class="detail-delete-form" method="post" action="/api/file/delete">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <button class="quiet" type="submit">(t(language, Key::Delete))</button>
                                </form>
                            </div>
                        }
                    </div>
                </div>
            </section>
        </main>
        (crate::dropdown::dropdown_script(cx).await?)
        (upload_script(cx).await?)
    }
}

/// The upload control's client half: files under 8 MiB ride the form's own
/// multipart post; files at or over it are split into the chunked protocol's
/// calls with a progress bar.
///
/// The form wears `data-hard` so the soft-nav's multipart replay leaves it
/// alone — two uploaders racing the same bytes would double-insert, and the
/// progress the soft-nav draws knows nothing of chunks. Without script the
/// form posts plainly and the 303 lands back on the folder.
async fn upload_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
            if (window.__inUpload) { return; } \
            window.__inUpload = true; \
            var LIMIT = 8388608; \
            function bar(box, label) { \
                var wrap = document.createElement('div'); \
                wrap.className = 'upload-progress'; \
                var fill = document.createElement('div'); \
                fill.className = 'upload-progress-fill'; \
                wrap.appendChild(fill); \
                box.appendChild(window.__inAdded(wrap)); \
                if (label) { wrap.setAttribute('aria-label', label); } \
                return fill; \
            } \
            function fail(box, message) { \
                var note = document.createElement('p'); \
                note.className = 'field-error'; \
                note.textContent = message; \
                box.appendChild(window.__inAdded(note)); \
            } \
            async function startChunked(folder, name, size) { \
                var r = await fetch('/api/upload/start', { \
                    method: 'POST', \
                    headers: { 'content-type': 'application/json', accept: 'application/json' }, \
                    body: JSON.stringify({ folder_id: folder || null, name: name, size_bytes: size }) \
                }); \
                var answer = await r.json(); \
                if (!answer.ok) { throw new Error(answer.err || 'start'); } \
                return answer.ok; \
            } \
            async function sendBig(file, folder, fill) { \
                var session = await startChunked(folder, file.name, file.size); \
                var index = 0; \
                for (var off = 0; off < file.size; off += LIMIT, index++) { \
                    var piece = file.slice(off, Math.min(file.size, off + LIMIT)); \
                    var r = await fetch('/api/upload/' + session.id + '/' + index, { \
                        method: 'PUT', \
                        headers: { 'content-type': 'application/octet-stream' }, \
                        body: piece \
                    }); \
                    if (!r.ok) { throw new Error('chunk'); } \
                    fill.style.width = Math.round(((off + piece.size) / file.size) * 100) + '%'; \
                } \
                var done = await fetch('/api/upload/' + session.id + '/finish', { method: 'POST' }); \
                var fin = await done.json(); \
                if (!fin.ok) { throw new Error((fin.err) || 'finish'); } \
            } \
            document.addEventListener('submit', function (e) { \
                var form = e.target; \
                if (!form || form.id !== 'upload-form') { return; } \
                e.preventDefault(); \
                var input = form.querySelector('.file-upload-input'); \
                var box = form.querySelector('.file-upload-box'); \
                var folder = form.querySelector('input[name=folder_id]').value; \
                var files = input && input.files ? Array.prototype.slice.call(input.files) : []; \
                if (!files.length) { return; } \
                if (form.__inBusy) { return; } \
                form.__inBusy = true; \
                if (input) { input.disabled = true; window.__inOwn(input, [], ['disabled']); } \
                var small = files.filter(function (f) { return f.size < LIMIT; }); \
                var big = files.filter(function (f) { return f.size >= LIMIT; }); \
                var failLabel = form.getAttribute('data-failed-label') || 'upload failed'; \
                var landing = window.location.href; \
                (async function () { \
                    try { \
                        if (small.length) { \
                            var data = new FormData(); \
                            data.append('folder_id', folder); \
                            small.forEach(function (f) { data.append('file', f, f.name); }); \
                            var fill = bar(box); \
                            var r = await fetch(form.getAttribute('action'), { \
                                method: 'POST', headers: { accept: 'text/html' }, body: data \
                            }); \
                            fill.style.width = '100%'; \
                            landing = r.url || landing; \
                            if (!r.ok) { fail(box, failLabel); } \
                        } \
                        for (var i = 0; i < big.length; i++) { \
                            var fill2 = bar(box, big[i].name); \
                            await sendBig(big[i], folder, fill2); \
                        } \
                    } catch (err) { fail(box, failLabel); } \
                    input.value = ''; \
                    form.__inBusy = false; \
                    if (input) { input.disabled = false; } \
                    if (window.__inGo) { window.__inGo(landing); } \
                    else { window.location.href = landing; } \
                })(); \
            }); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

#[derive(serde::Deserialize)]
struct CreateFolderForm {
    #[serde(default)]
    parent_id: String,
    name: String,
}

/// `POST /api/folder/create`: one folder under the open one, or the root.
#[route(POST "/api/folder/create")]
async fn create_folder(cx: &Cx, Form(input): Form<CreateFolderForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = app(cx).store.clone();
    let parent = if input.parent_id.is_empty() {
        None
    } else {
        Some(input.parent_id.as_str())
    };
    if let Some(id) = parent {
        match store.folder(id).await {
            Ok(Some(folder)) if folder.owner_id == user.id && folder.deleted_at.is_none() => {}
            _ => return redirect(cx, Some(Refusal::NotFound)),
        }
    }
    match store.create_folder(&user.id, parent, &input.name).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}

#[derive(serde::Deserialize)]
struct FolderIdForm {
    id: String,
}

#[derive(serde::Deserialize)]
struct RenameFolderForm {
    id: String,
    name: String,
}

#[derive(serde::Deserialize)]
struct MoveFolderForm {
    id: String,
    #[serde(default)]
    parent_id: String,
}

/// `POST /api/folder/rename`: a new label on an owned folder.
#[route(POST "/api/folder/rename")]
async fn rename_folder(cx: &Cx, Form(input): Form<RenameFolderForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = app(cx).store.clone();
    match store.folder(&input.id).await {
        Ok(Some(folder)) if folder.owner_id == user.id && folder.deleted_at.is_none() => {}
        _ => return redirect(cx, Some(Refusal::NotFound)),
    }
    match store.rename_folder(&input.id, &input.name).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}

/// `POST /api/folder/move`: an owned folder under another owned folder, or
/// the root. Into its own descendant is refused, not looped.
#[route(POST "/api/folder/move")]
async fn move_folder(cx: &Cx, Form(input): Form<MoveFolderForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = app(cx).store.clone();
    match store.folder(&input.id).await {
        Ok(Some(folder)) if folder.owner_id == user.id && folder.deleted_at.is_none() => {}
        _ => return redirect(cx, Some(Refusal::NotFound)),
    }
    let parent = if input.parent_id.is_empty() {
        None
    } else {
        Some(input.parent_id.as_str())
    };
    if let Some(id) = parent {
        match store.folder(id).await {
            Ok(Some(folder)) if folder.owner_id == user.id && folder.deleted_at.is_none() => {}
            _ => return redirect(cx, Some(Refusal::NotFound)),
        }
    }
    match store.move_folder(&input.id, parent).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}

/// `POST /api/folder/delete`: trash an owned folder — the whole subtree wears
/// the same timestamp, and the bytes wait in the trash for the purge.
#[route(POST "/api/folder/delete")]
async fn delete_folder(cx: &Cx, Form(input): Form<FolderIdForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = app(cx).store.clone();
    match store.folder(&input.id).await {
        Ok(Some(folder)) if folder.owner_id == user.id && folder.deleted_at.is_none() => {}
        _ => return redirect(cx, Some(Refusal::NotFound)),
    }
    match store.delete_folder(&input.id).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}
