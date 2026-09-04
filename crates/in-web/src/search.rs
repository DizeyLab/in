//! Name search over the reader's own drive.
//!
//! `GET /search?q=` matches the query against file and folder names with an
//! owner-scoped `LIKE`, non-trashed rows only. Results link straight to the
//! file view or the containing folder. An empty query renders the empty
//! prompt, not the whole drive.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::page;
use topcoat::router::request::uri;
use topcoat::view::view;

use crate::i18n::{Key, lang, t};
use crate::layout::{NavPage, topbar};
use crate::server::{app, require_user};

/// How many hits each list carries at most.
const LIMIT: u32 = 50;

/// The search box's text, trimmed. Anything else on the query is ignored.
fn searched(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_string)
}

/// The reader's own live files and folders wearing the query in their name.
/// Never another owner's, never trashed, never everything on an empty box.
#[page("/search")]
async fn search(cx: &Cx) -> Result {
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
    let asked = searched(query_value(uri(cx).query().unwrap_or(""), "q").as_deref());
    let hits = match asked.as_deref() {
        Some(query) => Some(app(cx).store.search(&user.id, query, LIMIT).await?),
        None => None,
    };
    let box_text = asked.clone().unwrap_or_default();
    view! {
        cx =>
        <main class="settings-shell">
            (topbar(cx, NavPage::Search, &user, language).await?)
            <h1 class="settings-title">(t(language, Key::SearchResults))</h1>
            <form class="field-box-search" method="get" action="/search">
                <input
                    class="field-input"
                    type="search"
                    name="q"
                    value=(box_text)
                    placeholder=(t(language, Key::SearchPlaceholder))
                    aria-label=(t(language, Key::SearchPlaceholder))
                >
            </form>
            if let Some(hits) = hits {
                if hits.folders.is_empty() && hits.files.is_empty() {
                    <p class="field-note">(t(language, Key::NoResults))</p>
                }
                if !hits.folders.is_empty() {
                    <section class="panel">
                        <h2 class="panel-title">(t(language, Key::FoldersHeading))</h2>
                        <div class="panel-body">
                            for folder in &hits.folders {
                                <p><a href=(format!("/drive?folder={}", folder.id))>(folder.name.clone())</a></p>
                            }
                        </div>
                    </section>
                }
                if !hits.files.is_empty() {
                    <section class="panel">
                        <h2 class="panel-title">(t(language, Key::FilesHeading))</h2>
                        <div class="panel-body">
                            for file in &hits.files {
                                <p><a href=(format!("/file/{}", file.id))>(file.name.clone())</a></p>
                            }
                        </div>
                    </section>
                }
            } else {
                <p class="field-note">(t(language, Key::TypeToSearch))</p>
            }
        </main>
    }
}

/// The value of one query pair, if present. A hand-edited query names
/// nothing rather than failing the page.
fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_string())
    })
}
