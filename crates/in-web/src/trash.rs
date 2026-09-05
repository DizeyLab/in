//! Trash: the soft-deleted shelf and its three mutations.
//!
//! `GET /trash` lists the reader's trashed files and folders. Trashed bytes
//! stay on disk and keep counting toward the quota until the purge. Folder
//! trash and restore cascade to descendants under one timestamp, and an item
//! whose ancestor is still trashed cannot be restored.
//! `POST /api/trash/restore|purge|empty` bring back, destroy one row (plus
//! its bytes), or destroy them all. Purge is the only delete that touches
//! the filesystem. Folder purge has no store method yet (hubbed to Main via
//! the orchestrator) and is refused until it lands.

use in_core::store::{File, Folder, ShareKind, Store, ThumbState, User};
use serde::Deserialize;
use time::OffsetDateTime;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::{HeaderName, StatusCode, header, page, query_params, route};
use topcoat::view::view;

use crate::files::entry_chip;
use crate::i18n::{Key, Lang, lang, t};
use crate::layout::{NavPage, topbar};
use crate::server::{Refusal, app, back_to, require_user};
use crate::share::{refusal_banner, refusal_of};

/// The reader's own trashed row, or nothing a mutation may touch.
/// Cross-owner ids answer as not-found, never forbidden.
async fn trashed_row(
    store: &dyn Store,
    user: &User,
    kind: ShareKind,
    id: &str,
) -> std::result::Result<(), Refusal> {
    let owned = match kind {
        ShareKind::File => store
            .file(id)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .is_some_and(|file| file.owner_id == user.id && file.deleted_at.is_some()),
        ShareKind::Folder => store
            .folder(id)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .is_some_and(|folder| folder.owner_id == user.id && folder.deleted_at.is_some()),
    };
    if owned {
        Ok(())
    } else {
        Err(Refusal::NotFound)
    }
}

type Redirect = Result<(StatusCode, [(HeaderName, String); 1])>;

/// Back to the trash, the refusal (if any) on the query.
fn redirect_back(cx: &Cx, call: &str, refusal: Option<Refusal>) -> Redirect {
    let back = back_to(cx, "/trash");
    let separator = if back.contains('?') { '&' } else { '?' };
    let location = match refusal {
        Some(refusal) => format!("{back}{separator}refusal={}&on={call}", refusal.code()),
        None => back,
    };
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]))
}

#[derive(Deserialize)]
struct TrashTargetForm {
    kind: String,
    id: String,
}

/// Brings one trashed row back. Refused while an ancestor folder is still
/// trashed (as forbidden — there is no code of its own yet), or while a
/// live sibling wears its name.
#[route(POST "/api/trash/restore")]
async fn restore(cx: &Cx, Form(input): Form<TrashTargetForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "restore", Some(refusal)),
    };
    let kind = match input.kind.as_str() {
        "file" => ShareKind::File,
        "folder" => ShareKind::Folder,
        _ => return redirect_back(cx, "restore", Some(Refusal::NotFound)),
    };
    if let Err(refusal) = trashed_row(app(cx).store.as_ref(), &user, kind, &input.id).await {
        return redirect_back(cx, "restore", Some(refusal));
    }
    let store = app(cx).store;
    let outcome = match kind {
        ShareKind::File => store.restore_file(&input.id).await,
        ShareKind::Folder => store.restore_folder(&input.id).await,
    };
    match outcome {
        Ok(()) => redirect_back(cx, "restore", None),
        Err(error) => redirect_back(cx, "restore", Some(refusal_of(error))),
    }
}

/// Destroys one trashed file for good: the row goes and the bytes and
/// thumbnail follow. A live file is never purged — trash it first. Folder
/// purge waits on a store method (see the module docs) and is refused.
#[route(POST "/api/trash/purge")]
async fn purge(cx: &Cx, Form(input): Form<TrashTargetForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "purge", Some(refusal)),
    };
    let kind = match input.kind.as_str() {
        "file" => ShareKind::File,
        "folder" => ShareKind::Folder,
        _ => return redirect_back(cx, "purge", Some(Refusal::NotFound)),
    };
    if let Err(refusal) = trashed_row(app(cx).store.as_ref(), &user, kind, &input.id).await {
        return redirect_back(cx, "purge", Some(refusal));
    }
    match kind {
        ShareKind::File => match app(cx).store.purge_file(&input.id).await {
            Ok(true) => redirect_back(cx, "purge", None),
            Ok(false) => redirect_back(cx, "purge", Some(Refusal::NotFound)),
            Err(error) => redirect_back(cx, "purge", Some(refusal_of(error))),
        },
        ShareKind::Folder => match app(cx).store.purge_folder(&input.id).await {
            Ok(_) => redirect_back(cx, "purge", None),
            Err(error) => redirect_back(cx, "purge", Some(refusal_of(error))),
        },
    }
}

/// Destroys everything the reader trashed, files before folders.
#[route(POST "/api/trash/empty")]
async fn empty(cx: &Cx) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "empty", Some(refusal)),
    };
    match app(cx).store.empty_trash(&user.id).await {
        Ok(_) => redirect_back(cx, "empty", None),
        Err(error) => redirect_back(cx, "empty", Some(refusal_of(error))),
    }
}

/// A trashed file's chip: always the mime-class glyph, never the thumbnail
/// image — `/thumb/{id}` 404s trashed rows, so a Ready row would render a
/// broken image. The clone only clears the thumbnail flag for the render.
async fn trash_chip(cx: &Cx, file: &File) -> Result {
    let mut unthumb = file.clone();
    unthumb.thumb_state = ThumbState::None;
    entry_chip(cx, &unthumb).await
}

#[query_params]
struct TrashQuery {
    sort: Option<String>,
    kind: Option<String>,
    q: Option<String>,
}

/// The trash's sort, off the query as `key:direction` ("deleted:asc"): name,
/// deleted, uploaded or size; names ascend by default, measures descend.
/// Anything else is the default — newest trash first.
fn valid_trash_sort(raw: Option<&str>) -> (&'static str, bool) {
    let (key, dir) = raw
        .unwrap_or("")
        .split_once(':')
        .unwrap_or((raw.unwrap_or(""), ""));
    let key = match key {
        "name" => "name",
        "deleted" => "deleted",
        "uploaded" => "uploaded",
        "size" => "size",
        _ => "deleted",
    };
    let descending = match dir {
        "asc" => false,
        "desc" => true,
        _ => key != "name",
    };
    (key, descending)
}

/// The kind filter, off the query: all, folders or files. Anything else
/// shows everything.
fn valid_kind(raw: Option<&str>) -> &'static str {
    match raw {
        Some("folders") => "folders",
        Some("files") => "files",
        _ => "all",
    }
}

/// One trashed row, folder or file, for the unified list.
enum TrashEntry<'a> {
    Folder(&'a Folder),
    File(&'a File),
}

impl TrashEntry<'_> {
    fn id(&self) -> &str {
        match self {
            TrashEntry::Folder(folder) => &folder.id,
            TrashEntry::File(file) => &file.id,
        }
    }

    fn name(&self) -> &str {
        match self {
            TrashEntry::Folder(folder) => &folder.name,
            TrashEntry::File(file) => &file.name,
        }
    }

    fn uploaded(&self) -> OffsetDateTime {
        match self {
            TrashEntry::Folder(folder) => folder.created_at,
            TrashEntry::File(file) => file.created_at,
        }
    }

    /// The trash timestamp. Always present on trashed rows; the upload
    /// date covers a row that lost it.
    fn deleted(&self) -> OffsetDateTime {
        let stamped = match self {
            TrashEntry::Folder(folder) => folder.deleted_at,
            TrashEntry::File(file) => file.deleted_at,
        };
        stamped.unwrap_or_else(|| self.uploaded())
    }

    /// Folders carry no size of their own and sort as zero.
    fn size(&self) -> u64 {
        match self {
            TrashEntry::Folder(_) => 0,
            TrashEntry::File(file) => file.size_bytes,
        }
    }
}

/// The muted line under each trashed name: the trash date, the upload date,
/// and the size for files.
fn trash_details(language: Lang, row: &TrashEntry) -> String {
    let mut out = format!(
        "{} {} · {} {}",
        t(language, Key::DeletedLabel),
        row.deleted().date(),
        t(language, Key::UploadedLabel),
        row.uploaded().date()
    );
    if let TrashEntry::File(file) = row {
        out.push_str(&format!(
            " · {}",
            crate::settings::human_bytes(file.size_bytes)
        ));
    }
    out
}

/// The reader's trash: folders and files in one list, each row carrying its
/// way back and its way gone, plus the button that destroys them all.
#[page("/trash")]
async fn trash(cx: &Cx) -> Result {
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
    let params = query_params::<TrashQuery>(cx).ok();
    let sort = valid_trash_sort(params.as_ref().and_then(|query| query.sort.as_deref()));
    let kind = valid_kind(params.as_ref().and_then(|query| query.kind.as_deref()));
    let asked = params
        .as_ref()
        .and_then(|query| query.q.clone())
        .map(|query| query.trim().to_string())
        .filter(|query| !query.is_empty());
    let box_text = asked.clone().unwrap_or_default();
    let listing = app(cx).store.list_trash(&user.id).await?;
    let nothing_trashed = listing.folders.is_empty() && listing.files.is_empty();
    let mut rows: Vec<TrashEntry> = Vec::new();
    if kind == "all" || kind == "folders" {
        rows.extend(listing.folders.iter().map(TrashEntry::Folder));
    }
    if kind == "all" || kind == "files" {
        rows.extend(listing.files.iter().map(TrashEntry::File));
    }
    if let Some(needle) = asked.as_deref() {
        let needle = needle.to_lowercase();
        rows.retain(|row| row.name().to_lowercase().contains(&needle));
    }
    let (sort, descending) = sort;
    rows.sort_by(|a, b| {
        let order = match sort {
            "name" => a
                .name()
                .to_lowercase()
                .cmp(&b.name().to_lowercase())
                .then_with(|| a.id().cmp(b.id())),
            "uploaded" => a
                .uploaded()
                .cmp(&b.uploaded())
                .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase())),
            "size" => a
                .size()
                .cmp(&b.size())
                .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase())),
            _ => a
                .deleted()
                .cmp(&b.deleted())
                .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase())),
        };
        if descending { order.reverse() } else { order }
    });
    let sort_value = format!("{}:{}", sort, if descending { "desc" } else { "asc" });
    view! {
        cx =>
        (topbar(cx, NavPage::Trash, &user, language).await?)
        <main class="settings-stage stage-wide">
            <h1 class="settings-title">(t(language, Key::Trash))</h1>
            <div class="filterbar">
                <form class="field-box field-box-sort" method="get" action="/trash">
                    <span class="field-text">(t(language, Key::Sort))</span>
                    <select class="status-select" name="sort" data-autosubmit="" data-nosearch="" aria-label=(t(language, Key::Sort))>
                        <option value="deleted:desc" selected=(sort_value == "deleted:desc")>(t(language, Key::SortNewest))</option>
                        <option value="deleted:asc" selected=(sort_value == "deleted:asc")>(t(language, Key::SortOldest))</option>
                        <option value="name:asc" selected=(sort_value == "name:asc")>(t(language, Key::SortNameAZ))</option>
                        <option value="name:desc" selected=(sort_value == "name:desc")>(t(language, Key::SortNameZA))</option>
                        <option value="uploaded:desc" selected=(sort_value == "uploaded:desc")>(t(language, Key::SortNewest))</option>
                        <option value="uploaded:asc" selected=(sort_value == "uploaded:asc")>(t(language, Key::SortOldest))</option>
                        <option value="size:desc" selected=(sort_value == "size:desc")>(t(language, Key::SortLargest))</option>
                        <option value="size:asc" selected=(sort_value == "size:asc")>(t(language, Key::SortSmallest))</option>
                    </select>
                    <input type="hidden" name="kind" value=(kind.to_string())>
                    <input type="hidden" name="q" value=(box_text.clone())>
                </form>
                <form class="field-box field-box-sort" method="get" action="/trash">
                    <span class="field-text">(t(language, Key::Kind))</span>
                    <select class="status-select" name="kind" data-autosubmit="" aria-label=(t(language, Key::Kind))>
                        <option value="all" selected=(kind == "all")>(t(language, Key::KindAll))</option>
                        <option value="folders" selected=(kind == "folders")>(t(language, Key::KindFolders))</option>
                        <option value="files" selected=(kind == "files")>(t(language, Key::KindFiles))</option>
                    </select>
                    <input type="hidden" name="sort" value=(sort_value.clone())>
                    <input type="hidden" name="q" value=(box_text.clone())>
                </form>
                <form class="field-box field-box-search" method="get" action="/trash">
                    <span class="field-text">(t(language, Key::NavSearch))</span>
                    <input
                        class="dd-search"
                        type="search"
                        name="q"
                        value=(box_text.clone())
                        placeholder=(t(language, Key::SearchPlaceholder))
                        aria-label=(t(language, Key::SearchPlaceholder))
                    >
                    <input type="hidden" name="sort" value=(sort_value.clone())>
                    <input type="hidden" name="kind" value=(kind.to_string())>
                </form>
            </div>
            (refusal_banner(cx, language, &["restore", "purge", "empty"]).await?)
            if !nothing_trashed {
                <form method="post" action="/api/trash/empty">
                    <button class="quiet quiet-danger" type="submit">(t(language, Key::EmptyTrash))</button>
                </form>
            }
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(language, Key::Trash))</h2>
                    <span class="chip">(rows.len().to_string())</span>
                </div>
                <div class="panel-body">
                    if rows.is_empty() {
                        if nothing_trashed {
                            <p class="field-note">(t(language, Key::TrashEmpty))</p>
                        } else {
                            <p class="field-note">(t(language, Key::NoResults))</p>
                        }
                    }
                    for row in &rows {
                        match row {
                            TrashEntry::Folder(folder) => <div class="dep-row">
                                <span class="file-chip file-chip-folder" aria-hidden="true">"▤"</span>
                                <span class="member-name dep-title">(folder.name.clone())</span>
                                <span class="field-note">(trash_details(language, row))</span>
                                <div class="spacer"></div>
                                <form class="pop-row-form" method="post" action="/api/trash/restore">
                                    <input type="hidden" name="kind" value="folder">
                                    <input type="hidden" name="id" value=(folder.id.clone())>
                                    <button class="quiet" type="submit">(t(language, Key::Restore))</button>
                                </form>
                                <form class="pop-row-form" method="post" action="/api/trash/purge">
                                    <input type="hidden" name="kind" value="folder">
                                    <input type="hidden" name="id" value=(folder.id.clone())>
                                    <button class="quiet quiet-danger" type="submit">(t(language, Key::DeleteForever))</button>
                                </form>
                            </div>,
                            TrashEntry::File(file) => <div class="dep-row">
                                (trash_chip(cx, file).await?)
                                <span class="member-name dep-title">(file.name.clone())</span>
                                <span class="field-note">(trash_details(language, row))</span>
                                <div class="spacer"></div>
                                <form class="pop-row-form" method="post" action="/api/trash/restore">
                                    <input type="hidden" name="kind" value="file">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <button class="quiet" type="submit">(t(language, Key::Restore))</button>
                                </form>
                                <form class="pop-row-form" method="post" action="/api/trash/purge">
                                    <input type="hidden" name="kind" value="file">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <button class="quiet quiet-danger" type="submit">(t(language, Key::DeleteForever))</button>
                                </form>
                            </div>,
                        }
                    }
                </div>
            </section>
        </main>
        (crate::dropdown::dropdown_script(cx).await?)
    }
}
