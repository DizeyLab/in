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

use in_core::store::{ShareKind, Store, User};
use serde::Deserialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::{HeaderName, StatusCode, header, page, route};
use topcoat::view::view;

use crate::i18n::{Key, lang, t};
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
    if owned { Ok(()) } else { Err(Refusal::NotFound) }
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

/// The reader's trash: every trashed folder and file, newest trash first,
/// each row carrying its way back and its way gone, plus the button that
/// destroys them all.
#[page("/trash")]
async fn trash(cx: &Cx) -> Result {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            let language = lang(cx);
            return view! {
                cx =>
                <main class="scaffold-note">
                    <p>(refusal.message_in(language))</p>
                    <p><a href="/">(t(language, Key::BackToDrive))</a></p>
                </main>
            };
        }
    };
    let language = lang(cx);
    let listing = app(cx)
        .store
        .list_trash(&user.id)
        .await?;
    view! {
        cx =>
        <main class="settings-shell">
            (topbar(cx, NavPage::Trash, &user, language).await?)
            <h1 class="settings-title">(t(language, Key::Trash))</h1>
            (refusal_banner(cx, language, &["restore", "purge", "empty"]).await?)
            if listing.folders.is_empty() && listing.files.is_empty() {
                <p class="field-note">(t(language, Key::TrashEmpty))</p>
            }
            if !listing.folders.is_empty() || !listing.files.is_empty() {
                <form method="post" action="/api/trash/empty">
                    <button class="quiet-danger" type="submit">(t(language, Key::EmptyTrash))</button>
                </form>
            }
            if !listing.folders.is_empty() {
                <section class="panel">
                    <h2 class="panel-title">(t(language, Key::FoldersHeading))</h2>
                    <div class="panel-body">
                        for folder in &listing.folders {
                            <div class="member-row">
                                <span class="member-name">(folder.name.clone())</span>
                                <form class="pop-row-form" method="post" action="/api/trash/restore">
                                    <input type="hidden" name="kind" value="folder">
                                    <input type="hidden" name="id" value=(folder.id.clone())>
                                    <button type="submit">(t(language, Key::Restore))</button>
                                </form>
                                <form class="pop-row-form" method="post" action="/api/trash/purge">
                                    <input type="hidden" name="kind" value="folder">
                                    <input type="hidden" name="id" value=(folder.id.clone())>
                                    <button class="quiet-danger" type="submit">(t(language, Key::DeleteForever))</button>
                                </form>
                            </div>
                        }
                    </div>
                </section>
            }
            if !listing.files.is_empty() {
                <section class="panel">
                    <h2 class="panel-title">(t(language, Key::FilesHeading))</h2>
                    <div class="panel-body">
                        for file in &listing.files {
                            <div class="member-row">
                                <span class="member-name">(file.name.clone())</span>
                                <span class="field-note">(crate::settings::human_bytes(file.size_bytes))</span>
                                <form class="pop-row-form" method="post" action="/api/trash/restore">
                                    <input type="hidden" name="kind" value="file">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <button type="submit">(t(language, Key::Restore))</button>
                                </form>
                                <form class="pop-row-form" method="post" action="/api/trash/purge">
                                    <input type="hidden" name="kind" value="file">
                                    <input type="hidden" name="id" value=(file.id.clone())>
                                    <button class="quiet-danger" type="submit">(t(language, Key::DeleteForever))</button>
                                </form>
                            </div>
                        }
                    </div>
                </section>
            }
        </main>
    }
}
