//! The drive browser.
//!
//! `GET /drive?folder=<id>` renders one directory — breadcrumbs back to the
//! root, subfolders then files — and the four owner-only folder mutations
//! ride beside it: `POST /api/folder/create|rename|move|delete`. Trashed
//! items never appear here; they wait in `trash.rs`. A folder whose ancestor
//! is trashed is unreachable, and a move that would nest a folder inside
//! itself is refused. Anything naming another owner's tree answers 404,
//! never 403.
//!
//! `GET /drive?q=<text>` keeps the same page and swaps the folder list
//! for library-wide name hits — the filterbar's search box, submitted as a
//! plain GET form. The old `GET /search?q=` address 303s here, so old links
//! keep working.

use in_core::store::{Folder, Store, StoreError};
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

/// How many hits each search list carries at most.
const LIMIT: u32 = 50;

#[query_params]
struct DriveQuery {
    folder: Option<String>,
    q: Option<String>,
    edit: Option<String>,
    sort: Option<String>,
    share: Option<String>,
    kind: Option<String>,
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
/// The drive listing's sort key: `None` is the standing order —
/// folders-then-files as the store listed them — anything else re-orders
/// each group in place. Unknown values fall back to the standing order,
/// never to a refusal: a hand-edited query names nothing fancy, not an
/// error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Uploaded,
    Size,
    Downloads,
}

impl SortKey {
    fn as_str(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Uploaded => "uploaded",
            SortKey::Size => "size",
            SortKey::Downloads => "downloads",
        }
    }
}

/// The sort direction. Absent or unknown falls back per key — ascending
/// for names, descending for the recency/count keys — the way iz reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    fn as_str(self) -> &'static str {
        match self {
            SortDir::Asc => "asc",
            SortDir::Desc => "desc",
        }
    }
}

/// The kind filter: everything, folders only, files only.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KindFilter {
    All,
    Folders,
    Files,
}

impl KindFilter {
    fn as_str(self) -> &'static str {
        match self {
            KindFilter::All => "all",
            KindFilter::Folders => "folders",
            KindFilter::Files => "files",
        }
    }
}

/// The sort off the query: `key:direction` in one value ("size:desc"), the
/// way the dropdown's options name it. A bare key reads with its natural
/// direction (names ascend, measures descend); anything else is no sort.
fn parse_sort(raw: Option<&str>) -> Option<(SortKey, SortDir)> {
    let (key, dir) = raw
        .unwrap_or("")
        .split_once(':')
        .unwrap_or((raw.unwrap_or(""), ""));
    let key = match key {
        "name" => SortKey::Name,
        "uploaded" => SortKey::Uploaded,
        "size" => SortKey::Size,
        "downloads" => SortKey::Downloads,
        _ => return None,
    };
    let dir = match dir {
        "asc" => SortDir::Asc,
        "desc" => SortDir::Desc,
        _ => match key {
            SortKey::Name => SortDir::Asc,
            _ => SortDir::Desc,
        },
    };
    Some((key, dir))
}

fn parse_kind(raw: Option<&str>) -> KindFilter {
    match raw.unwrap_or("") {
        "folders" => KindFilter::Folders,
        "files" => KindFilter::Files,
        _ => KindFilter::All,
    }
}

/// Order one folder group in place. Folders carry no size or download
/// count, so those keys fall back to the name order rather than inventing
/// one; the direction still applies.
fn sort_folders(folders: &mut [Folder], key: SortKey, dir: SortDir) {
    folders.sort_by(|a, b| {
        let order = match key {
            SortKey::Name | SortKey::Size | SortKey::Downloads => {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            }
            SortKey::Uploaded => a.created_at.cmp(&b.created_at),
        }
        .then_with(|| a.id.cmp(&b.id));
        match dir {
            SortDir::Asc => order,
            SortDir::Desc => order.reverse(),
        }
    });
}

/// Order one file group in place.
fn sort_files(files: &mut [in_core::store::File], key: SortKey, dir: SortDir) {
    files.sort_by(|a, b| {
        let order = match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Uploaded => a.created_at.cmp(&b.created_at),
            SortKey::Size => a.size_bytes.cmp(&b.size_bytes),
            SortKey::Downloads => a.download_count.cmp(&b.download_count),
        }
        .then_with(|| a.id.cmp(&b.id));
        match dir {
            SortDir::Asc => order,
            SortDir::Desc => order.reverse(),
        }
    });
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

    let params = query_params::<DriveQuery>(cx).ok();
    let wanted = params.as_ref().and_then(|query| query.folder.clone());
    // The filterbar's search box, trimmed: anything else on the query is
    // ignored, and an empty box is the plain folder view, never the whole
    // library.
    let asked = params
        .as_ref()
        .and_then(|query| query.q.clone())
        .map(|query| query.trim().to_string())
        .filter(|query| !query.is_empty());
    // The toolbar's listing controls: unknown values fall back to the
    // standing order (folders-then-files as listed), ascending names, and
    // the unfiltered kind — a hand-edited query re-orders nothing, it
    // never refuses.
    let sorting = params
        .as_ref()
        .and_then(|query| parse_sort(query.sort.as_deref()));
    let kind = params
        .as_ref()
        .map(|query| parse_kind(query.kind.as_deref()))
        .unwrap_or(KindFilter::All);
    let mut hits = match asked.as_deref() {
        Some(query) => Some(store.search(&user.id, query, LIMIT).await?),
        None => None,
    };
    let box_text = asked.clone().unwrap_or_default();
    let current = current_folder(store.as_ref(), &user.id, wanted.as_deref()).await?;
    let crumbs = breadcrumbs(store.as_ref(), current.as_ref()).await;
    let mut listing = match store
        .list_children(&user.id, current.as_ref().map(|folder| folder.id.as_str()))
        .await
    {
        Ok(listing) => listing,
        Err(_) => return Err(not_found().into()),
    };
    // Sort and filter in the page layer, after the fetch — no store API
    // change. Each group keeps its own order; folders-then-files stands.
    if let Some((key, dir)) = sorting {
        sort_folders(&mut listing.folders, key, dir);
        sort_files(&mut listing.files, key, dir);
    }
    match kind {
        KindFilter::All => {}
        KindFilter::Folders => listing.files.clear(),
        KindFilter::Files => listing.folders.clear(),
    }
    if let Some(found) = hits.as_mut() {
        if let Some((key, dir)) = sorting {
            sort_folders(&mut found.folders, key, dir);
            sort_files(&mut found.files, key, dir);
        }
        match kind {
            KindFilter::All => {}
            KindFilter::Folders => found.files.clear(),
            KindFilter::Files => found.folders.clear(),
        }
    }
    let destinations = folder_tree(store.as_ref(), &user.id).await;
    let refusal = page_refusal(cx);
    // Which folder row (if any) renders as its rename form: iz's `?edit=`
    // idiom — a server-side row swap, no client state to hold.
    let edit_id = params.as_ref().and_then(|query| query.edit.clone());
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
    // The folder view without `?edit=`: the rename form's cancel target and
    // the base the edit links extend. The listing controls ride along so a
    // rename round-trip keeps the order and filter the reader chose; every
    // value here is an id or a fixed vocabulary word, so no encoding is
    // owed.
    let mut here_bits: Vec<String> = Vec::new();
    if !current_id.is_empty() {
        here_bits.push(format!("folder={current_id}"));
    }
    if let Some((key, dir)) = sorting {
        here_bits.push(format!("sort={}:{}", key.as_str(), dir.as_str()));
    }
    if kind != KindFilter::All {
        here_bits.push(format!("kind={}", kind.as_str()));
    }
    let here = if here_bits.is_empty() {
        "/drive".to_string()
    } else {
        format!("/drive?{}", here_bits.join("&"))
    };
    // The edit links extend `here` with the matching separator.
    let edit_sep = if here.contains('?') { "&" } else { "?" };
    // The value the sort dropdown shows as selected: the parsed pair, or the
    // visual default when the standing order is on.
    let sort_value = sorting
        .map(|(key, dir)| format!("{}:{}", key.as_str(), dir.as_str()))
        .unwrap_or_else(|| "name:asc".to_string());
    // The share dialog: `?share=kind:id` renders the entry's modal over the
    // page (its forms come back here through Referer, so it stays open); the
    // close link is this view without the pair. A pair that names nothing
    // owned and live opens nothing.
    let share_pair = params.as_ref().and_then(|query| query.share.as_deref());
    let share_dialog = match share_pair.and_then(|pair| pair.split_once(':')) {
        Some((kind, id)) => {
            crate::share::share_modal(cx, kind, id, &here, created.clone(), &origin).await?
        }
        None => None,
    };
    // The copy-once banner belongs to the modal when it is open.
    let created_panel = if share_pair.is_some() {
        None
    } else {
        created.clone()
    };

    view! {
        cx =>
        (topbar(cx, NavPage::Drive, &user, language).await?)
        <main class="settings-stage stage-wide">
            <div class="filterbar">
                if let Some(query) = asked.as_deref() {
                    <p class="detail-quiet">(t(language, Key::SearchResults)) (format!(" “{query}”"))</p>
                } else {
                    if current.is_some() {
                        <nav class="detail-crumbs" aria-label=(current_name.clone())>
                            <a class="detail-crumb" href="/drive">(t(language, Key::Drive))</a>
                            for crumb in crumbs.iter() {
                                <span class="detail-crumb-sep">"/"</span>
                                <a class="detail-crumb" href=(format!("/drive?folder={}", crumb.id))>(crumb.name.clone())</a>
                            }
                        </nav>
                    }
                }
                <form class="field-box field-box-search" method="get" action="/drive">
                    <span class="field-text">(t(language, Key::NavSearch))</span>
                    <input
                        class="dd-search"
                        type="search"
                        name="q"
                        value=(box_text.clone())
                        placeholder=(t(language, Key::SearchPlaceholder))
                        aria-label=(t(language, Key::SearchPlaceholder))
                    >
                    <input type="hidden" name="folder" value=(current_id.clone())>
    if sorting.is_some() {
        <input type="hidden" name="sort" value=(sort_value.clone())>
    }
    if kind != KindFilter::All {
        <input type="hidden" name="kind" value=(kind.as_str())>
    }
                </form>
                <div class="spacer"></div>
                <form class="field-box field-box-sort" method="get" action="/drive">
                    <span class="field-text">(t(language, Key::Sort))</span>
    <select class="status-select" name="sort" data-autosubmit="" data-nosearch="" aria-label=(t(language, Key::Sort))>
        <option value="name:asc" selected=(sort_value == "name:asc")>(t(language, Key::SortNameAZ))</option>
        <option value="name:desc" selected=(sort_value == "name:desc")>(t(language, Key::SortNameZA))</option>
        <option value="uploaded:desc" selected=(sort_value == "uploaded:desc")>(t(language, Key::SortNewest))</option>
        <option value="uploaded:asc" selected=(sort_value == "uploaded:asc")>(t(language, Key::SortOldest))</option>
        <option value="size:desc" selected=(sort_value == "size:desc")>(t(language, Key::SortLargest))</option>
        <option value="size:asc" selected=(sort_value == "size:asc")>(t(language, Key::SortSmallest))</option>
        <option value="downloads:desc" selected=(sort_value == "downloads:desc")>(t(language, Key::SortMostDownloads))</option>
        <option value="downloads:asc" selected=(sort_value == "downloads:asc")>(t(language, Key::SortLeastDownloads))</option>
    </select>
    <input type="hidden" name="folder" value=(current_id.clone())>
    <input type="hidden" name="q" value=(box_text.clone())>
    if kind != KindFilter::All {
        <input type="hidden" name="kind" value=(kind.as_str())>
    }
    <button class="quiet" type="submit" aria-label=(t(language, Key::Sort))>"→"</button>
    </form>
                <form class="field-box field-box-sort" method="get" action="/drive">
                    <span class="field-text">(t(language, Key::Kind))</span>
                    <select class="status-select" name="kind" data-autosubmit="" aria-label=(t(language, Key::Kind))>
                        <option value="all" selected=(kind == KindFilter::All)>(t(language, Key::KindAll))</option>
                        <option value="folders" selected=(kind == KindFilter::Folders)>(t(language, Key::KindFolders))</option>
                        <option value="files" selected=(kind == KindFilter::Files)>(t(language, Key::KindFiles))</option>
                    </select>
                    <input type="hidden" name="folder" value=(current_id.clone())>
                    <input type="hidden" name="q" value=(box_text.clone())>
                    if sorting.is_some() {
                        <input type="hidden" name="sort" value=(sort_value.clone())>
                    }
                    <button class="quiet" type="submit" aria-label=(t(language, Key::Kind))>"→"</button>
                </form>
                <details class="user-menu drive-add">
                    <summary class="quiet drive-add-trigger" aria-label=(t(language, Key::NewFolder))>"+ "</summary>
                    <div class="user-menu-panel">
                        <form class="user-menu-item-form" method="post" action="/api/folder/create">
                            <input type="hidden" name="parent_id" value=(current_id.clone())>
                            <button class="user-menu-item" type="submit">(t(language, Key::NewFolder))</button>
                        </form>
                        <label class="user-menu-item" for="drive-upload-input">(t(language, Key::UploadFiles))</label>
                    </div>
                </details>
                </div>
            if let Some(refusal) = refusal {
                <p class="field-error">(refusal.message_in(language))</p>
            }
    if let Some(token) = created_panel {
                <section class="panel">
                    <h2 class="panel-title">(t(language, Key::LinkCreated))</h2>
                    <div class="panel-body">
                        <p class="field-note">(t(language, Key::CopyLinkOnce))</p>
                        <p class="member-link-value">(format!("{origin}/s/{token}"))</p>
                    </div>
                </section>
            }
            // The upload control sits outside the list so the + menu's
            // "Upload files" label and the page-wide drop handler find it on
            // every drive view, folder or search. The input stays hidden via
            // the file-upload-input rule; live progress rows go to the
            // document shell's `#in-status` corner stack, which every page
            // carries, so they stay visible across soft navigations.
            <form id="upload-form" class="file-upload" method="post" action="/files"
                enctype="multipart/form-data" data-hard=""
                data-failed-label=(t(language, Key::UploadFailed))
                data-complete-label=(t(language, Key::UploadComplete))
                data-cancel-label=(t(language, Key::CancelUpload))
                data-canceled-label=(t(language, Key::UploadCanceled))>
                <input type="hidden" name="folder_id" value=(current_id.clone())>
                <input id="drive-upload-input" class="file-upload-input" type="file" name="file" multiple="">
            </form>
            // The full-page drop affordance: hidden until a file drag is
            // over the window, pointer-transparent so it never blocks the
            // drop itself.
            <div id="drop-overlay" class="drop-overlay" aria-hidden="true">
                <span class="drop-overlay-text">(t(language, Key::DropFilesToUpload))</span>
            </div>
            if let Some(hits) = hits {
                if hits.folders.is_empty() && hits.files.is_empty() {
                    <p class="detail-quiet">(t(language, Key::NoResults))</p>
                } else {
                <section class="panel">
                    <div class="panel-body">
                        for folder in &hits.folders {
                            <div class="dep-row">
                                <a class="dep-link" href=(format!("/drive?folder={}", folder.id))>(folder.name.clone())</a>
                            </div>
                        }
                        for file in &hits.files {
                            <div class="dep-row">
                                <a class="dep-link" href=(format!("/view/{}", file.id))>(file.name.clone())</a>
                            </div>
                        }
                    </div>
                </section>
                }
            } else {
            <section class="panel">
                <div class="panel-body">
                    if listing.folders.is_empty() && listing.files.is_empty() {
                        <p class="detail-quiet">(t(language, Key::EmptyFolder))</p>
                    }
                    for folder in listing.folders.iter() {
                        <div class="dep-row">
                            if edit_id.as_deref() == Some(folder.id.as_str()) {
                                <form class="dep-edit-form" method="post" action="/api/folder/rename">
                                    <input type="hidden" name="id" value=(folder.id.clone())>
                                    <input class="field-input" type="text" name="name" maxlength="255"
                                        value=(folder.name.clone()) required="" data-edit-focus=""
                                        aria-label=(t(language, Key::RenameFolder))>
                                    <div class="spacer"></div>
                                    <button class="quiet" type="submit">(t(language, Key::Rename))</button>
                                    <a class="quiet" href=(here.clone())>(t(language, Key::Cancel))</a>
                                </form>
                            } else {
                            <a class="dep-link" href=(format!("/drive?folder={}", folder.id))>
                                <span class="dep-title">(folder.name.clone())</span>
                            </a>
                            <span class="dep-note">(t(language, Key::UploadedLabel))" "(folder.created_at.date().to_string())</span>
                            <div class="spacer"></div>
                            <details class="user-menu entry-options">
                                <summary class="quiet entry-options-trigger">(t(language, Key::Options))" ▾"</summary>
                                <div class="user-menu-panel">
                                    <a class="user-menu-item"
                                        href=(format!("{here}{edit_sep}edit={}", folder.id))>(t(language, Key::Rename))</a>
                                    <a class="user-menu-item"
                                        href=(format!("{here}{edit_sep}share=folder:{}", folder.id))>(t(language, Key::Share))</a>
                                    <form class="user-menu-item-form" method="post" action="/api/folder/move">
                                        <input type="hidden" name="id" value=(folder.id.clone())>
                                        <div class="user-menu-item-move">
                                            <select class="field-input" name="parent_id" aria-label=(t(language, Key::MoveFolder))>
                                                <option value="">(t(language, Key::Drive))</option>
                                                for dest in destinations.iter() {
                                                    if dest.0 != folder.id {
                                                        <option value=(dest.0.clone())>(dest.1.clone())</option>
                                                    }
                                                }
                                            </select>
                                            <button class="quiet" type="submit">(t(language, Key::Move))</button>
                                        </div>
                                    </form>
                                    <form class="user-menu-item-form" method="post" action="/api/folder/delete">
                                        <input type="hidden" name="id" value=(folder.id.clone())>
                                        <button class="user-menu-item quiet quiet-danger" type="submit">(t(language, Key::Delete))</button>
                                    </form>
                                </div>
                            </details>
                            }
                        </div>
                    }
                    for file in listing.files.iter() {
                        <div class="dep-row">
                            (crate::files::entry_chip(cx, file).await?)
                            if edit_id.as_deref() == Some(file.id.as_str()) {
                                <form class="dep-edit-form" method="post" action="/api/file/rename">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <input class="field-input" type="text" name="name" maxlength="255"
                                        value=(file.name.clone()) required="" data-edit-focus=""
                                        aria-label=(t(language, Key::RenameFile))>
                                    <div class="spacer"></div>
                                    <button class="quiet" type="submit">(t(language, Key::Rename))</button>
                                    <a class="quiet" href=(here.clone())>(t(language, Key::Cancel))</a>
                                </form>
                            } else {
                            <a class="dep-link" href=(format!("/view/{}", file.id))>
                                <span class="dep-title">(file.name.clone())</span>
                            </a>
                            <span class="file-chip-size">(human_size(file.size_bytes))</span>
                            <span class="file-chip-note">(t(language, Key::UploadedLabel))" "(file.created_at.date().to_string())</span>
                            <span class="file-chip-note">(t(language, Key::DownloadsLabel))" "(file.download_count.to_string())</span>
                            <div class="spacer"></div>
                            <details class="user-menu entry-options">
                                <summary class="quiet entry-options-trigger">(t(language, Key::Options))" ▾"</summary>
                                <div class="user-menu-panel">
                                    <a class="user-menu-item"
                                        href=(format!("{here}{edit_sep}edit={}", file.id))>(t(language, Key::Rename))</a>
                                    <a class="user-menu-item"
                                        href=(format!("{here}{edit_sep}share=file:{}", file.id))>(t(language, Key::Share))</a>
                                    <form class="user-menu-item-form" method="post" action="/api/file/move">
                                        <input type="hidden" name="id" value=(file.id.clone())>
                                        <div class="user-menu-item-move">
                                            <select class="field-input" name="folder_id" aria-label=(t(language, Key::MoveFile))>
                                                <option value="">(t(language, Key::Drive))</option>
                                                for dest in destinations.iter() {
                                                    <option value=(dest.0.clone())>(dest.1.clone())</option>
                                                }
                                            </select>
                                            <button class="quiet" type="submit">(t(language, Key::Move))</button>
                                        </div>
                                    </form>
                                    <form class="user-menu-item-form" method="post" action="/api/file/delete">
                                        <input type="hidden" name="id" value=(file.id.clone())>
                                        <button class="user-menu-item quiet quiet-danger" type="submit">(t(language, Key::Delete))</button>
                                    </form>
                                </div>
                            </details>
                            }
                        </div>
                    }
                </div>
            </section>
            }
        </main>
        if let Some(dialog) = share_dialog {
            (dialog)
        }
        (crate::dropdown::dropdown_script(cx).await?)
        (upload_script(cx).await?)
        (options_menu_script(cx).await?)
    }
}

/// `GET /search`: the old address — a 303 to the drive's search box, keeping
/// the query so old links land on their results. Nothing renders here.
#[page("/search")]
async fn search_redirect(cx: &Cx) -> Result {
    let target = match query_value(uri(cx).query().unwrap_or(""), "q") {
        Some(raw) if !raw.trim().is_empty() => format!("/drive?q={raw}"),
        _ => "/drive".to_string(),
    };
    let location = (
        header::LOCATION,
        HeaderValue::from_str(&target).unwrap_or(HeaderValue::from_static("/drive")),
    );
    view! {
        cx =>
        (StatusCode::SEE_OTHER)
        (location)
    }
}

/// The value of one query pair, if present. A hand-edited query names
/// nothing rather than failing the redirect.
fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_string())
    })
}

/// The upload control's client half: files under 8 MiB ride one multipart
/// post, files at or over it are split into the chunked protocol's calls —
/// and every upload, small or big, draws the same live progress row (name,
/// bar, percent, cancel) into the persistent `#in-status` corner stack,
/// which the document shell renders on every page. Cancel aborts the small
/// path's XHR, or stops the chunked loop and posts the session's abort
/// endpoint — the abort handle rides the `window.__inUploads` mirror entry,
/// so the row's ✕ keeps working across soft navigations — then the row
/// drops and the canceled card announces through `window.__inNotify`.
///
/// The form wears `data-hard` so the soft-nav's multipart replay leaves it
/// alone — two uploaders racing the same bytes would double-insert, and the
/// progress the soft-nav draws knows nothing of chunks. Without script the
/// form posts plainly and the 303 lands back on the folder.
///
/// Failures never append inline notes anymore: they go through
/// `window.__inNotify` into the persistent stack
/// ([`crate::layout::status_script`]), which survives the swaps that used to
/// wipe them. Completions announce there too, then the page navigates to the
/// landing the post answered.
///
/// The same block carries the `?edit=` row's focus: the rename input the
/// server renders open gets focused with its name selected on arrival, so a
/// quick-created folder is ready to name. Guarded lookups — most visits
/// render no such input.
///
/// And the page-wide drop: dragging files anywhere over a drive view raises
/// the `#drop-overlay` affordance (a dragenter/dragleave counter, hidden on
/// drop or on leaving), and dropping feeds the upload input, whose change
/// the soft-nav's listener turns into the submit below. The overlay wears
/// `pointer-events: none` so it never blocks the drop itself.
///
/// The Rust `\`-continuations strip newlines, so the emitted script is one
/// long line: only `/* */` comments survive inside it — a `//` would eat
/// the rest of the script and kill every handler silently.
async fn upload_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
            if (window.__inUpload) { return; } \
            window.__inUpload = true; \
            var LIMIT = 8388608; \
            function upBox() { return document.getElementById('in-status'); } \
            function upSeq() { window.__inUpSeq = (window.__inUpSeq || 0) + 1; return window.__inUpSeq; } \
            /* Rows are found by walking `[data-upload]` and comparing the \
               attribute in script: a selector with an interpolated value would \
               need double quotes inside this Rust string, so it is avoided. */ \
            function findRow(box, id) { \
                var rows = box.querySelectorAll('[data-upload]'); \
                for (var i = 0; i < rows.length; i++) { \
                    if (rows[i].getAttribute('data-upload') == String(id)) { return rows[i]; } \
                } \
                return null; \
            } \
            /* The in-flight mirror: progress rows are client-made (`__inAdded`, \
               so the live morph keeps them) and mirrored here, so the full-replace \
               path — which rebuilds the body on every soft navigation — re-renders \
               them from this array on `in:wire` instead of losing sight of the \
               upload. Progress ticks only ever write the mirror, then paint the \
               live row if it is still connected; a tick landing mid-swap paints \
               on the next wire. Full page loads cannot preserve an in-flight bar \
               — the completion card still announces on landing. */ \
            function renderUploads() { \
                var box = upBox(); \
                if (!box) { return; } \
                if (!window.__inUploads) { window.__inUploads = []; } \
                var seen = {}; \
                window.__inUploads.forEach(function (u) { \
                    seen[u.id] = true; \
                    var row = findRow(box, u.id); \
                    if (!row) { \
                        row = document.createElement('div'); \
                        row.className = 'upload-progress-row'; \
                        row.setAttribute('data-upload', String(u.id)); \
                        if (u.name) { row.setAttribute('aria-label', u.name); } \
                        var name = document.createElement('span'); \
                        name.className = 'upload-progress-name'; \
                        name.textContent = u.name || ''; \
                        var track = document.createElement('div'); \
                        track.className = 'upload-progress'; \
                        var fill = document.createElement('div'); \
                        fill.className = 'upload-progress-fill'; \
                        track.appendChild(fill); \
                        var pct = document.createElement('span'); \
                        pct.className = 'upload-progress-pct'; \
                        var shut = document.createElement('button'); \
                        shut.type = 'button'; \
                        shut.className = 'upload-progress-cancel'; \
                        shut.textContent = '✕'; \
                        if (u.cancelLabel) { shut.setAttribute('aria-label', u.cancelLabel); } \
                        shut.addEventListener('click', function () { \
                            shut.disabled = true; \
                            if (u.cancel) { u.cancel(); } \
                        }); \
                        row.appendChild(name); \
                        row.appendChild(track); \
                        row.appendChild(pct); \
                        row.appendChild(shut); \
                        box.appendChild(window.__inAdded(row)); \
                    } \
                    var text = Math.min(100, Math.round(u.frac * 100)) + '%'; \
                    var fillNow = row.querySelector('.upload-progress-fill'); \
                    var pctNow = row.querySelector('.upload-progress-pct'); \
                    if (fillNow) { fillNow.style.width = text; } \
                    if (pctNow) { pctNow.textContent = text; } \
                }); \
                box.querySelectorAll('[data-upload]').forEach(function (row) { \
                    if (!seen[row.getAttribute('data-upload')]) { row.remove(); } \
                }); \
            } \
            function bar(label) { \
                if (!window.__inUploads) { window.__inUploads = []; } \
                var form = document.getElementById('upload-form'); \
                var cancelLabel = (form && form.getAttribute('data-cancel-label')) || ''; \
                var u = { id: upSeq(), name: label || '', frac: 0, cancelLabel: cancelLabel }; \
                /* A cancel landing before the transport arms itself (the \
                   start round-trip) still counts: the flag below stops the \
                   loop at the next boundary, and sendBig aborts a session \
                   minted after the click. */ \
                u.cancel = function () { u.cancelled = true; }; \
                window.__inUploads.push(u); \
                renderUploads(); \
                return u; \
            } \
            function setProgress(u, frac) { \
                u.frac = frac; \
                var box = upBox(); \
                var row = box && findRow(box, u.id); \
                if (!row) { renderUploads(); return; } \
                var text = Math.min(100, Math.round(frac * 100)) + '%'; \
                var fill = row.querySelector('.upload-progress-fill'); \
                var pct = row.querySelector('.upload-progress-pct'); \
                if (fill) { fill.style.width = text; } \
                if (pct) { pct.textContent = text; } \
            } \
            function dropRow(u) { \
                if (window.__inUploads) { \
                    window.__inUploads = window.__inUploads.filter(function (x) { return x.id !== u.id; }); \
                } \
                var box = upBox(); \
                var row = box && findRow(box, u.id); \
                if (row) { row.remove(); } \
            } \
            document.addEventListener('in:wire', renderUploads); \
            renderUploads(); \
            function notify(kind, message) { \
                if (window.__inNotify) { window.__inNotify(kind, message); } \
            } \
            /* Cancel is not a failure: the rejection below carries a flag the \
               submit handler reads, so a deliberate cancel announces the \
               canceled card instead of the failed one. */ \
            function canceledErr() { var e = new Error('canceled'); e.canceled = true; return e; } \
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
            async function sendBig(file, folder, onProgress, u) { \
                var session = await startChunked(folder, file.name, file.size); \
                /* The session id lives on the mirror entry, so the row's \
                   cancel control still reaches the abort endpoint after a \
                   soft navigation rebuilt the page around it. A cancel that \
                   landed during the start flight aborts the fresh session \
                   straight away instead of leaking it. */ \
                if (u) { \
                    if (u.cancelled) { \
                        fetch('/api/upload/' + session.id + '/abort', { method: 'POST' }); \
                        throw canceledErr(); \
                    } \
                    u.sessionId = session.id; \
                    u.cancel = function () { \
                        u.cancelled = true; \
                        try { u.abortChunk(); } catch (err) {} \
                        fetch('/api/upload/' + session.id + '/abort', { method: 'POST' }); \
                    }; \
                } \
                var total = Math.max(1, file.size); \
                var index = 0; \
                var flight = null; \
                if (u) { \
                    u.abortChunk = function () { if (flight) { try { flight.abort(); } catch (err) {} } }; \
                } \
                for (var off = 0; off < file.size; off += LIMIT, index++) { \
                    if (u && u.cancelled) { throw canceledErr(); } \
                    var piece = file.slice(off, Math.min(file.size, off + LIMIT)); \
                    flight = new AbortController(); \
                    var r; \
                    try { \
                        r = await fetch('/api/upload/' + session.id + '/' + index, { \
                            method: 'PUT', \
                            headers: { 'content-type': 'application/octet-stream' }, \
                            body: piece, \
                            signal: flight.signal \
                        }); \
                    } catch (err) { \
                        if (u && u.cancelled) { throw canceledErr(); } \
                        throw err; \
                    } \
                    if (!r.ok) { throw new Error('chunk'); } \
                    onProgress((off + piece.size) / total); \
                } \
                if (u && u.cancelled) { throw canceledErr(); } \
                var done = await fetch('/api/upload/' + session.id + '/finish', { method: 'POST' }); \
                var fin = await done.json(); \
                if (!fin.ok) { throw new Error((fin.err) || 'finish'); } \
                onProgress(1); \
            } \
            function sendSmall(action, folder, files, onProgress, u) { \
                return new Promise(function (resolve, reject) { \
                    var data = new FormData(); \
                    data.append('folder_id', folder); \
                    files.forEach(function (f) { data.append('file', f, f.name); }); \
                    var x = new XMLHttpRequest(); \
                    if (u) { \
                        u.cancel = function () { \
                            u.cancelled = true; \
                            try { x.abort(); } catch (err) {} \
                        }; \
                    } \
                    x.open('POST', action); \
                    x.setRequestHeader('accept', 'text/html'); \
                    x.upload.onprogress = function (ev) { \
                        if (ev.lengthComputable && ev.total > 0) { onProgress(ev.loaded / ev.total); } \
                    }; \
                    x.onload = function () { \
                        onProgress(1); \
                        if (x.status >= 200 && x.status < 300) { resolve(x.responseURL || ''); } \
                        else { reject(new Error('small')); } \
                    }; \
                    x.onerror = function () { reject(new Error('small')); }; \
                    x.onabort = function () { reject(canceledErr()); }; \
                    x.send(data); \
                }); \
            } \
            document.addEventListener('submit', function (e) { \
                var form = e.target; \
                if (!form || form.id !== 'upload-form') { return; } \
                e.preventDefault(); \
                var input = form.querySelector('.file-upload-input'); \
                var folder = form.querySelector('input[name=folder_id]').value; \
                var files = input && input.files ? Array.prototype.slice.call(input.files) : []; \
                if (!files.length) { return; } \
                if (form.__inBusy) { return; } \
                form.__inBusy = true; \
                if (input) { input.disabled = true; window.__inOwn(input, [], ['disabled']); } \
                var small = files.filter(function (f) { return f.size < LIMIT; }); \
                var big = files.filter(function (f) { return f.size >= LIMIT; }); \
                var failLabel = form.getAttribute('data-failed-label') || 'upload failed'; \
                var doneLabel = form.getAttribute('data-complete-label') || ''; \
                var canceledLabel = form.getAttribute('data-canceled-label') || ''; \
                var landing = window.location.href; \
                function settle() { \
                    input.value = ''; \
                    form.__inBusy = false; \
                    if (input) { input.disabled = false; } \
                } \
                (async function () { \
                    var rows = []; \
                    try { \
                        if (small.length) { \
                            var title = small.length === 1 ? small[0].name : (small.length + ' files'); \
                            var ui = bar(title); \
                            rows.push(ui); \
                            var url = await sendSmall(form.getAttribute('action'), folder, small, function (frac) { setProgress(ui, frac); }, ui); \
                            if (url) { landing = url; } \
                        } \
                        for (var i = 0; i < big.length; i++) { \
                            await (function (file) { \
                                var ui2 = bar(file.name); \
                                rows.push(ui2); \
                                return sendBig(file, folder, function (frac) { setProgress(ui2, frac); }, ui2); \
                            })(big[i]); \
                        } \
                    } catch (err) { \
                        rows.forEach(dropRow); \
                        if (err && err.canceled) { \
                            if (canceledLabel) { notify('ok', canceledLabel); } \
                        } else { \
                            var why = err && err.message ? err.message : String(err); \
                            notify('error', failLabel + ' (' + why + ')'); \
                        } \
                        settle(); \
                        return; \
                    } \
                    /* A refused upload answers 303 into a refusal banner page, which the XHR follows to a 200: the banner carries the message, so only trumpet success when no refusal rode the redirect. */ \
                    if (doneLabel && !/[?&]refusal=/.test(landing)) { notify('ok', doneLabel); } \
                    settle(); \
                    if (window.__inGo) { window.__inGo(landing); } \
                    else { window.location.href = landing; } \
                })(); \
            }); \
            (function () { \
                if (window.__inEditFocus) { return; } \
                window.__inEditFocus = true; \
                function focusIt() { \
                    var input = document.querySelector('input[data-edit-focus]'); \
                    if (input && input.focus && input.select) { input.focus(); input.select(); } \
                } \
                focusIt(); \
                document.addEventListener('in:wire', focusIt); \
            })(); \
            (function () { \
                if (window.__inDrop) { return; } \
                window.__inDrop = true; \
                var depth = 0; \
                function uploadForm() { return document.getElementById('upload-form'); } \
                function overlay() { return document.getElementById('drop-overlay'); } \
                function hasFiles(e) { \
                    return !!(e.dataTransfer && e.dataTransfer.types && Array.prototype.indexOf.call(e.dataTransfer.types, 'Files') !== -1); \
                } \
                function show(on) { \
                    var ov = overlay(); \
                    if (ov) { ov.classList.toggle('drop-overlay-show', !!on); } \
                } \
                document.addEventListener('dragenter', function (e) { \
                    if (!uploadForm() || !hasFiles(e)) { return; } \
                    e.preventDefault(); \
                    depth++; \
                    show(true); \
                }); \
                document.addEventListener('dragover', function (e) { \
                    if (uploadForm() && hasFiles(e)) { e.preventDefault(); } \
                }); \
                document.addEventListener('dragleave', function (e) { \
                    if (!uploadForm()) { return; } \
                    depth = Math.max(0, depth - 1); \
                    if (depth === 0) { show(false); } \
                }); \
                document.addEventListener('drop', function (e) { \
                    depth = 0; \
                    show(false); \
                    var form = uploadForm(); \
                    if (!form || !e.dataTransfer || !e.dataTransfer.files || !e.dataTransfer.files.length) { return; } \
                    e.preventDefault(); \
                    var input = form.querySelector('.file-upload-input'); \
                    if (!input) { return; } \
                    input.files = e.dataTransfer.files; \
                    input.dispatchEvent(new Event('change', { bubbles: true })); \
                }); \
            })(); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

/// The row Options menus' client half: a `<details class="entry-options">`
/// panel opens downward by house CSS. The flip decision is viewport-relative
/// — the same drop-up idea as `dropdown.rs`'s `place`, which `iz` shares:
/// the menu lifts via the `menu-up` variant class only when it spills past
/// the viewport bottom *and* fits better above. The stage is not a clipper:
/// on this page `.settings-stage` is content-height (the window scrolls), so
/// measuring against it lifted every menu near the list bottom and the
/// max-height clamp then gave the panel its own scrollbar; the stylesheet's
/// `:has` lift lets the open panel escape the stage's `overflow` instead.
/// On toggle open the panel is measured one frame later — the toggle fires
/// before the open layout settles, so a synchronous read scrolls menus that
/// fit. The scroll fallback fires only when the menu spills in both
/// directions; a menu that fits as-is never moves the page.
///
/// Open menus close the way the house dropdowns do: a second row's menu
/// opening closes the first (two stacked panels once read as one menu with
/// doubled items), and a click anywhere outside closes whichever is open —
/// except clicks on the Move picker's `.dd-panel`, which `dropdown.rs`
/// portals to `<body>` and which belongs to the open menu. Closing also
/// clears the inline clamp, so a reopened menu measures fresh.
///
/// The Rust `\`-continuations strip newlines, so the emitted script is one
/// long line: only `/* */` comments survive inside it.
async fn options_menu_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
            if (window.__inEntryOptions) { return; } \
            window.__inEntryOptions = true; \
            function closeOthers(except) { \
                document.querySelectorAll('details.entry-options[open]').forEach(function (d) { \
                    if (d !== except) { d.removeAttribute('open'); } \
                }); \
            } \
            /* `toggle` does not bubble, but a capture listener still sees it \
               on the way down; per-details listeners would need rewiring \
               after every soft swap, this one survives them. */ \
            document.addEventListener('toggle', function (e) { \
                var details = e.target; \
                if (!details || !details.classList || !details.classList.contains('entry-options')) { return; } \
                var panel = details.querySelector('.user-menu-panel'); \
                if (!details.hasAttribute('open')) { \
                    details.classList.remove('menu-up'); \
                    if (panel) { panel.style.maxHeight = ''; panel.style.overflowY = ''; } \
                    return; \
                } \
                if (!panel) { return; } \
                closeOthers(details); \
                /* The toggle fires before the open layout settles (panel \
                   transition, the `:has` overflow lift), so a synchronously \
                   read rect is stale and the scroll fallback fired on menus \
                   that fit — measure one frame later, post-layout. */ \
                window.requestAnimationFrame(function () { \
                    if (!details.hasAttribute('open')) { return; } \
                    details.classList.remove('menu-up'); \
                    var down = panel.getBoundingClientRect(); \
                    if (down.bottom <= window.innerHeight - 8) { return; } \
                    var trigger = details.querySelector('summary'); \
                    var anchor = trigger ? trigger.getBoundingClientRect() : details.getBoundingClientRect(); \
                    var above = anchor.top - 4; \
                    var below = window.innerHeight - anchor.bottom - 4; \
                    if (above >= down.height || above > below) { details.classList.add('menu-up'); } \
                    /* Never taller than the room on the chosen side: without \
                       the cap a drop-up reaches over the topbar. */ \
                    var up = details.classList.contains('menu-up'); \
                    panel.style.maxHeight = Math.max(96, (up ? above : below) - 4) + 'px'; \
                    panel.style.overflowY = 'auto'; \
                    var r = panel.getBoundingClientRect(); \
                    if (r.top < 0 && r.bottom > window.innerHeight) { panel.scrollIntoView({ block: 'nearest' }); } \
                }); \
            }, true); \
            document.addEventListener('click', function (e) { \
                var el = e.target; \
                /* The Move picker's panel is portaled to <body> by \
                   dropdown.rs — outside the details box but inside the \
                   menu's logic, so it must not close the menu it serves. */ \
                if (el && el.closest && (el.closest('details.entry-options') || el.closest('.dd-panel'))) { return; } \
                closeOthers(null); \
            }, true); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

#[derive(serde::Deserialize)]
struct CreateFolderForm {
    #[serde(default)]
    parent_id: String,
    // Absent on the + menu's quick form, which posts no name at all: the
    // empty read below takes the generic-name path instead of failing the
    // post.
    #[serde(default)]
    name: String,
}

/// `POST /api/folder/create`: one folder under the open one, or the root.
///
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
    if input.name.trim().is_empty() {
        return quick_folder(cx, &store, &user.id, parent).await;
    }
    match store.create_folder(&user.id, parent, &input.name).await {
        Ok(_) => redirect(cx, None),
        Err(error) => redirect(cx, Some(store_refusal(error))),
    }
}

/// The + menu's quick create: a generic name, then a 303 to the folder view
/// with that row in its rename form. The store postfixes a colliding name
/// itself, so one attempt always answers.
async fn quick_folder(
    cx: &Cx,
    store: &std::sync::Arc<dyn Store>,
    owner_id: &str,
    parent: Option<&str>,
) -> Redirect {
    let base = t(lang(cx).await, Key::NewFolder).to_string();
    match store.create_folder(owner_id, parent, &base).await {
        Ok(folder) => {
            let location = match parent {
                Some(parent) => format!("/drive?folder={parent}&edit={}", folder.id),
                None => format!("/drive?edit={}", folder.id),
            };
            Ok((
                StatusCode::SEE_OTHER,
                [(header::LOCATION, location)],
                Json(None),
            ))
        }
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
