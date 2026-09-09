//! Settings: the reader's profile and quota, plus the admin's user panel.
//!
//! `GET /settings` renders one rail section at a time (`?section=`, the
//! profile when the pair is missing or unknown): the profile with the quota
//! usage bar; the reader's own live share links with their revoke buttons —
//! including the copy-once banner after a creation (`?created=<token>` on
//! the query, rendered once and never stored); and, for an admin, the
//! Everyone panel of every account with its quota and disabled flag, plus
//! the server address the public links carry. `POST
//! /api/settings/quota|disable|base_url` are admin-only: the first sets a
//! user's byte quota, the second disables or re-enables them (a disabled
//! account reads as signed-out everywhere, the im session untouched, and
//! disabling yourself is refused), the third stores the origin share links
//! are built from.

use in_core::store::{ShareKind, User};
use serde::Deserialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::request::uri;
use topcoat::router::{HeaderName, StatusCode, header, page, route};
use topcoat::view::view;
use crate::server::{
    Refusal, app, back_to, require_admin, require_user, share_link_url, share_origin,
};
use crate::share::{refusal_banner, refusal_of, share_copy_script};
use crate::i18n::{Key, Lang, lang, t};
use crate::layout::{NavPage, topbar};

/// Bytes in human units: `512 B`, `1.5 KiB`, `2.0 GiB`. One decimal past
/// bytes so a quota line reads at a glance.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The value half of a Connection card's age line: the seconds when the
/// moment is known, the `never` wording before its first occurrence —
/// a card that went silent and a card that never spoke read differently.
fn age_text(age: Option<i64>, language: Lang) -> String {
    match age {
        Some(age) => format!("{age}s"),
        None => t(language, Key::Never).to_string(),
    }
}

/// The share of the quota already held, for the usage bar's width.
fn quota_percent(user: &User) -> u8 {
    if user.quota_bytes == 0 {
        return 100;
    }
    (user.used_bytes.saturating_mul(100) / user.quota_bytes).min(100) as u8
}

/// One mebibyte / gibibyte in bytes: the only two units the limit forms speak.
/// Raw bytes never reach the browser — the panel shows `human_bytes` and the
/// forms post a decimal amount plus one of these units, converted back here.
const MIB_BYTES: f64 = 1_048_576.0;
const GIB_BYTES: f64 = 1_073_741_824.0;

/// The `(amount, unit)` pair a human-unit form field defaults to: gibibytes
/// once the value reaches one, mebiBytes below it, so `2 GiB` edits as `2`
/// GiB rather than `2048` MiB, while a small `512 MiB` cap stays addressable.
fn bytes_as_unit(bytes: u64) -> (String, &'static str) {
    if bytes >= GIB_BYTES as u64 {
        (trim_amount(bytes as f64 / GIB_BYTES), "GiB")
    } else {
        (trim_amount(bytes as f64 / MIB_BYTES), "MiB")
    }
}

/// A form amount for a number input: two decimals at most, trailing zeros
/// (and a bare point) trimmed, so the field reads `2` and `2.5`, never
/// `2.00` or `2.5000000001`.
fn trim_amount(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    let text = format!("{rounded:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The bytes an `(amount, unit)` pair names, or `None` when the pair is not
/// a usable quota: an unknown unit, or an amount that is not a finite
/// positive number.
fn unit_bytes(amount: &str, unit: &str) -> Option<u64> {
    let per = match unit {
        "MiB" => MIB_BYTES,
        "GiB" => GIB_BYTES,
        _ => return None,
    };
    let value: f64 = amount.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * per).round() as u64)
}

type Redirect = Result<(StatusCode, [(HeaderName, String); 1])>;

/// Back to the posting page's section, the refusal (if any) on the query —
/// or `saved=<call>` when there was nothing to refuse, the way iz's
/// `saved_or_refused` marks a save the page should chip. `nowhere` names
/// the section the call is answered on, for a post that arrives without a
/// `Referer` to go back to.
fn redirect_back(cx: &Cx, nowhere: &str, call: &str, refusal: Option<Refusal>) -> Redirect {
    let back = back_to(cx, nowhere);
    let separator = if back.contains('?') { '&' } else { '?' };
    let location = match refusal {
        Some(refusal) => format!("{back}{separator}refusal={}&on={call}", refusal.code()),
        None => format!("{back}{separator}saved={call}"),
    };
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]))
}

#[derive(Deserialize)]
struct QuotaForm {
    user_id: String,
    #[serde(default)]
    quota: String,
    #[serde(default)]
    quota_unit: String,
}

/// Sets one account's byte ceiling from a human-unit pair (`quota` amount in
/// `quota_unit` MiB/GiB), converted to bytes for the store. The bytes already
/// held are untouched — a quota lowered under current usage refuses new
/// uploads until the person frees space, rather than deleting anything.
#[route(POST "/api/settings/quota")]
async fn set_quota(cx: &Cx, Form(input): Form<QuotaForm>) -> Redirect {
    if let Err(refusal) = require_admin(cx).await {
        return redirect_back(cx, "/settings?section=everyone", "quota", Some(refusal));
    }
    match unit_bytes(&input.quota, &input.quota_unit) {
        Some(quota_bytes) => match app(cx)
            .store
            .set_user_quota(&input.user_id, quota_bytes)
            .await
        {
            Ok(()) => redirect_back(cx, "/settings?section=everyone", "quota", None),
            Err(error) => redirect_back(cx, "/settings?section=everyone", "quota", Some(refusal_of(error))),
        },
        None => redirect_back(cx, "/settings?section=everyone", "quota", Some(Refusal::BadLimit)),
    }
}

#[derive(Deserialize)]
struct DisableForm {
    user_id: String,
    #[serde(default)]
    disabled: Option<String>,
}

/// Disables or re-enables one account. Disabling yourself is refused — the
/// install must never be left with no way back in through its only admin.
#[route(POST "/api/settings/disable")]
async fn set_disabled(cx: &Cx, Form(input): Form<DisableForm>) -> Redirect {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return redirect_back(cx, "/settings?section=everyone", "disable", Some(refusal)),
    };
    if input.user_id == admin.id {
        return redirect_back(cx, "/settings?section=everyone", "disable", Some(Refusal::Forbidden));
    }
    let disabled = match input.disabled.as_deref() {
        None => true,
        Some(value) => !matches!(
            value.trim().to_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
    };
    match app(cx)
        .store
        .set_user_disabled(&input.user_id, disabled)
        .await
    {
        Ok(()) => redirect_back(cx, "/settings?section=everyone", "disable", None),
        Err(error) => redirect_back(cx, "/settings?section=everyone", "disable", Some(refusal_of(error))),
    }
}

#[derive(Deserialize)]
struct PreferencesForm {
    ui: String,
    theme: String,
    language: String,
}

/// The values the interface field offers.
const UI_OPTIONS: [&str; 2] = ["instrument", "ledger"];

/// The values the theme field offers.
const THEME_OPTIONS: [&str; 2] = ["light", "dark"];

/// The values the language field offers.
const LANGUAGE_OPTIONS: [&str; 2] = ["en", "tr"];

/// Writes the reader's own display preferences: interface, theme, language.
/// Each field is checked against what its dropdown offers — another value is
/// a hand-built post, not a typo, and is refused with the field's own
/// refusal, the way iz's `save_profile` answers its preferences block.
#[route(POST "/api/settings/preferences")]
async fn set_preferences(cx: &Cx, Form(input): Form<PreferencesForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect_back(cx, "/settings?section=profile", "preferences", Some(refusal)),
    };
    if !UI_OPTIONS.contains(&input.ui.as_str()) {
        return redirect_back(cx, "/settings?section=profile", "preferences", Some(Refusal::BadUi));
    }
    if !THEME_OPTIONS.contains(&input.theme.as_str()) {
        return redirect_back(cx, "/settings?section=profile", "preferences", Some(Refusal::BadTheme));
    }
    if !LANGUAGE_OPTIONS.contains(&input.language.as_str()) {
        return redirect_back(cx, "/settings?section=profile", "preferences", Some(Refusal::BadLanguage));
    }
    match app(cx)
        .store
        .set_preferences(&user.id, &input.theme, &input.language, &input.ui)
        .await
    {
        Ok(()) => redirect_back(cx, "/settings?section=profile", "preferences", None),
        Err(error) => redirect_back(cx, "/settings?section=profile", "preferences", Some(refusal_of(error))),
    }
}

#[derive(Deserialize)]
struct BaseUrlForm {
    #[serde(default)]
    base_url: String,
}

/// Stores the origin the public links are built from, in the instance
/// `setting` store under `base_url`. While a non-empty value sits there it
/// is what [`crate::server::share_origin`] hands every link builder;
/// saving empty clears it, and the origin falls back to the config chain.
/// A value the boot would refuse is refused here for the same reason —
/// [`in_core::Config::validate_base_url`], the very check `parse` runs —
/// and the stored setting is left untouched.
#[route(POST "/api/settings/base_url")]
async fn set_base_url(cx: &Cx, Form(input): Form<BaseUrlForm>) -> Redirect {
    if let Err(refusal) = require_admin(cx).await {
        return redirect_back(cx, "/settings?section=server", "base_url", Some(refusal));
    }
    let trimmed = input.base_url.trim().to_string();
    if !trimmed.is_empty() && in_core::Config::validate_base_url(&trimmed).is_err() {
        return redirect_back(cx, "/settings?section=server", "base_url", Some(Refusal::BadBaseUrl));
    }
    match app(cx).store.set_setting("base_url", &trimmed).await {
        Ok(()) => redirect_back(cx, "/settings?section=server", "base_url", None),
        Err(error) => redirect_back(cx, "/settings?section=server", "base_url", Some(refusal_of(error))),
    }
}

/// Which rail section the page renders. Only one is drawn at a time; an
/// admin-only value asked for by anyone else falls back to `Profile`, same
/// as a section name it does not recognize at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Profile,
    Links,
    Everyone,
    Server,
}

/// The class a rail link wears: `active` on the section it points to when
/// that is the one showing, plain otherwise.
fn rail_class(current: Section, target: Section) -> &'static str {
    if current == target {
        "settings-section-link active"
    } else {
        "settings-section-link"
    }
}

/// The value of one query pair, if present.
fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_string())
    })
}

/// The settings screen, one rail section at a time: the reader's profile
/// and quota, their live share links, and — for an admin — every account
/// with its quota and kill switch, plus the server address the public
/// links are built from. An admin section asked for by anyone else renders
/// the profile, the way an unknown section name does.
#[page("/settings")]
async fn settings(cx: &Cx) -> Result {
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
    let app = app(cx);
    let store = app.store;
    // The Connection card reads only what the config already carries and
    // the sync task's own atomics — the client secret is never touched on
    // any path that reaches this render.
    let connected = app.health.stream_connected();
    let last_event = app.health.last_event_age_secs();
    let last_pass = app.health.last_pass_age_secs();
    // Both ages share one shape: the seconds when the moment is known,
    // the `never` wording before the first occurrence — a card that went
    // silent and a card that never spoke read differently.
    // Re-read the row so the quota bar never shows a stale number.
    let fresh = store.user(&user.id).await?.unwrap_or_else(|| user.clone());
    let administers = fresh.admin;
    let query = uri(cx).query().unwrap_or("");
    let section = match query_value(query, "section").as_deref() {
        Some("links") => Section::Links,
        Some("everyone") if administers => Section::Everyone,
        Some("server") if administers => Section::Server,
        _ => Section::Profile,
    };
    let created = created_token(query);
    // Only the section on show pays for its rows: the links panel names
    // each live link's target, Everyone lists the accounts, and the server
    // panel needs the origin — nothing else reads them.
    let mut link_names: Vec<(&in_core::store::ShareLink, String, Option<String>)> = Vec::new();
    let links = if section == Section::Links {
        store.share_links(&user.id).await?
    } else {
        Vec::new()
    };
    if section == Section::Links {
        // The display name per live link's target: the row carries only
        // the id. A missing or trashed target keeps its id — the grant row
        // outlives the trash, and the panel still revokes it. Enrichment
        // never fails the page: an unreadable target reads as its id, the
        // way the shared list degrades per row.
        for link in links.iter().filter(|link| link.revoked_at.is_none()) {
            let name = match link.kind {
                ShareKind::File => store
                    .file(&link.target_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|file| file.name),
                ShareKind::Folder => store
                    .folder(&link.target_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|folder| folder.name),
            }
            .unwrap_or_else(|| link.target_id.clone());
            // The full address, re-derived from the token the creation
            // sealed away; `None` keeps the row masked with the note.
            let url = share_link_url(cx, link).await;
            link_names.push((link, name, url));
        }
    }
    let users = if administers && section == Section::Everyone {
        store.users().await?
    } else {
        Vec::new()
    };
    // The origin the copy-once banner and the server panel show: the
    // stored setting when an admin set one, the config chain otherwise.
    let origin = if matches!(section, Section::Links | Section::Server) {
        share_origin(cx).await
    } else {
        String::new()
    };
    // What the server form's field holds: the stored setting alone. An
    // empty field is not an invented value — the effective line above it
    // shows what an empty save falls back to.
    let stored_base_url = if section == Section::Server {
        store
            .get_setting("base_url")
            .await?
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_default()
    } else {
        String::new()
    };
    view! {
        cx =>
        (topbar(cx, NavPage::Settings, &fresh, language).await?)
        <div class="settings-shell">
            <nav class="settings-sections">
                <a class=(rail_class(section, Section::Profile)) href="/settings?section=profile">(t(language, Key::Profile))</a>
                <a class=(rail_class(section, Section::Links)) href="/settings?section=links">(t(language, Key::ManageLinks))</a>
                if administers {
                    <a class=(rail_class(section, Section::Everyone)) href="/settings?section=everyone">(t(language, Key::AdminPanel))</a>
                    <a class=(rail_class(section, Section::Server)) href="/settings?section=server">(t(language, Key::ServerAddress))</a>
                }
            </nav>
            <main class="settings-stage stage-wide">
                <h1 class="settings-title">(t(language, Key::Settings))</h1>
                (refusal_banner(cx, language, &["create", "revoke", "add", "remove", "quota", "disable", "preferences", "base_url"]).await?)
                if section == Section::Profile {
                    <section class="panel">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(language, Key::Profile))</h2>
                        </div>
                        <div class="panel-body">
                            <label class="field">
                                <span class="field-label">(t(language, Key::DisplayName))</span>
                                <span class="field-static">(fresh.display_name.clone())</span>
                            </label>
                            <label class="field">
                                <span class="field-label">(t(language, Key::EmailAddress))</span>
                                <span class="field-static">(fresh.email.clone())</span>
                            </label>
                            <div class="field">
                                <span class="field-label">(t(language, Key::QuotaUsage))</span>
                                <span class="field-static">(format!("{} {} {}", human_bytes(fresh.used_bytes), t(language, Key::QuotaOf), human_bytes(fresh.quota_bytes)))</span>
                                <div class="quota-bar" role="progressbar" aria-valuenow=(quota_percent(&fresh).to_string()) aria-valuemin="0" aria-valuemax="100">
                                    <div class="quota-fill" style=(format!("width: {}%", quota_percent(&fresh)))></div>
                                </div>
                            </div>
                            <form class="field" method="post" action="/api/settings/preferences">
                                <label class="field">
                                    <span class="field-label">(t(language, Key::UiLabel))</span>
                                    <select class="field-input" name="ui">
                                        <option value="instrument" selected=(fresh.ui == "instrument")>"Instrument"</option>
                                        <option value="ledger" selected=(fresh.ui == "ledger")>"Ledger"</option>
                                    </select>
                                </label>
                                <label class="field">
                                    <span class="field-label">(t(language, Key::ThemeLabel))</span>
                                    <select class="field-input" name="theme">
                                        <option value="light" selected=(fresh.theme == "light")>(t(language, Key::LightOption))</option>
                                        <option value="dark" selected=(fresh.theme == "dark")>(t(language, Key::DarkOption))</option>
                                    </select>
                                </label>
                                <label class="field">
                                    <span class="field-label">(t(language, Key::LanguageLabel))</span>
                                    <select class="field-input" name="language">
                                        <option value="en" selected=(fresh.language == "en")>"English"</option>
                                        <option value="tr" selected=(fresh.language == "tr")>"Türkçe"</option>
                                    </select>
                                </label>
                                <div class="panel-foot">
                                    <button class="primary" type="submit">(t(language, Key::Save))</button>
                                </div>
                            </form>
                        </div>
                    </section>
                    // The Connection card: who this app talks to, and whether
                    // the live feed is up right now. Read-only, every
                    // signed-in reader — nothing here is per-user data.
                    <section class="panel">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(language, Key::Connection))</h2>
                        </div>
                        <div class="panel-body">
                            <div class="field">
                                <span class="field-label">(t(language, Key::ConnectionIssuer))</span>
                                <span class="field-static">(app.config.oidc.issuer.clone())</span>
                            </div>
                            <div class="field">
                                <span class="field-label">(t(language, Key::ConnectionClientId))</span>
                                <span class="field-static">(app.config.oidc.client_id.clone())</span>
                            </div>
                            <div class="field">
                                <span class="field-label">(t(language, Key::ConnectionStream))</span>
                                <span class="field-static">
                                    <span class=(if connected { "status-dot status-dot-done" } else { "status-dot status-dot-warn" })></span>
                                    if connected {
                                        (t(language, Key::ConnectionConnected))
                                    } else {
                                        (t(language, Key::ConnectionReconnecting))
                                    }
                                </span>
                                <div class="field-note">
                                    (format!("{} · {}", t(language, Key::ConnectionLastEvent), age_text(last_event, language)))
                                </div>
                                <div class="field-note">
                                    (format!("{} · {}", t(language, Key::ConnectionLastPass), age_text(last_pass, language)))
                                </div>
                            </div>
                        </div>
                    </section>
                }
                if section == Section::Links {
                    if let Some(token) = created {
                        <section class="panel">
                            <div class="panel-head">
                                <h2 class="panel-title">(t(language, Key::LinkCreated))</h2>
                            </div>
                            <div class="panel-body">
                                <p class="field-note">(t(language, Key::CopyLinkOnce))</p>
                                <p class="member-link-value">(format!("{origin}/s/{token}"))</p>
                            </div>
                        </section>
                    }
                    <section class="panel">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(language, Key::ManageLinks))</h2>
                        </div>
                        <div class="panel-body">
                            if link_names.is_empty() {
                                <p class="field-note">(t(language, Key::NoLinks))</p>
                            }
                            for (link, target_name, url) in &link_names {
                                <div class="member-row">
                                    <span class="member-name">(format!("{} · {}", link.kind.as_str(), target_name.clone()))</span>
                                    <span class="field-note">(expiry_line(language, link.expires_at))</span>
                                    <div class="spacer"></div>
                                    <span class="field-note">(if link.can_download { t(language, Key::CanDownload) } else { t(language, Key::ViewOnly) })</span>
                                    <form class="pop-row-form" method="post" action="/api/share/link/revoke">
                                        <input type="hidden" name="id" value=(link.id.clone())>
                                        <button class="quiet quiet-danger" type="submit">(t(language, Key::RevokeLink))</button>
                                    </form>
                                </div>
                                // The same affordance the modal's live row
                                // carries: the full address with its copy
                                // button, or the one-click mint plus the
                                // carry-forward note for a link from before
                                // the sealing.
                                <div class="share-link-row">
                                    if let Some(url) = url {
                                        <p class="member-link-value share-link-url" aria-label=(t(language, Key::ShareLink))>(url.clone())</p>
                                        <button class="quiet share-copy" type="button" data-copied-label=(t(language, Key::Copied))>(t(language, Key::CopyLink))</button>
                                    } else {
                                        <form class="pop-row-form" method="post" action="/api/share/link/remint">
                                            <input type="hidden" name="id" value=(link.id.clone())>
                                            <button class="quiet" type="submit">(t(language, Key::RemintLink))</button>
                                        </form>
                                    }
                                </div>
                                if url.is_none() {
                                    <p class="share-link-note">(t(language, Key::LegacyLinkNote))</p>
                                }
                            }
                        </div>
                    </section>
                    // One delegated listener serves every row's copy button
                    // above; the script is idempotent across re-renders.
                    (share_copy_script(cx).await?)
                }
                if section == Section::Everyone {
                    <section class="panel">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(language, Key::AdminPanel))</h2>
                        </div>
                        <div class="panel-body">
                            <div class="table-pan">
                                <table class="member-table">
                                    <thead>
                                        <tr>
                                            <th class="member-col-name" scope="col">(t(language, Key::NameColumn))</th>
                                            <th class="member-col-address" scope="col">(t(language, Key::EmailAddress))</th>
                                            <th class="member-col-account" scope="col">(t(language, Key::QuotaUsage))</th>
                                            <th class="member-col-quota" scope="col">(t(language, Key::QuotaBytes))</th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        for listed in &users {
                                            <tr class="member-row">
                                                <td class="member-col-name member-name">
                                                    <span class="member-name-row">
                                                        (listed.display_name.clone())
                                                        if listed.admin {
                                                            <span class="chip chip-admin">(t(language, Key::AdminBadge))</span>
                                                        }
                                                        if listed.disabled {
                                                            <span class="chip chip-off">(t(language, Key::DisabledBadge))</span>
                                                        }
                                                    </span>
                                                </td>
                                                <td class="member-col-address member-address">(listed.email.clone())</td>
                                                <td class="member-col-account member-account">(format!("{} {} {}", human_bytes(listed.used_bytes), t(language, Key::QuotaOf), human_bytes(listed.quota_bytes)))</td>
                                                <td class="member-col-quota">
                                                    <form class="pop-row-form member-quota" method="post" action="/api/settings/quota">
                                                        <input type="hidden" name="user_id" value=(listed.id.clone())>
                                                        <input class="field-input" type="number" name="quota" min="0" step="any" value=(bytes_as_unit(listed.quota_bytes).0) aria-label=(t(language, Key::QuotaBytes))>
                                                        <select class="field-input" name="quota_unit" aria-label=(t(language, Key::QuotaBytes))>
                                                            <option value="MiB" selected=(bytes_as_unit(listed.quota_bytes).1 == "MiB")>"MiB"</option>
                                                            <option value="GiB" selected=(bytes_as_unit(listed.quota_bytes).1 == "GiB")>"GiB"</option>
                                                        </select>
                                                        <button class="quiet" type="submit">(t(language, Key::SetQuota))</button>
                                                    </form>
                                                </td>
                                                <td class="member-col-role">
                                                    if listed.id != fresh.id {
                                                        <form class="pop-row-form" method="post" action="/api/settings/disable">
                                                            <input type="hidden" name="user_id" value=(listed.id.clone())>
                                                            if listed.disabled {
                                                                <input type="hidden" name="disabled" value="0">
                                                                <button class="quiet" type="submit">(t(language, Key::EnableUser))</button>
                                                            } else {
                                                                <input type="hidden" name="disabled" value="1">
                                                                <button class="quiet quiet-danger" type="submit">(t(language, Key::DisableUser))</button>
                                                            }
                                                        </form>
                                                    }
                                                </td>
                                            </tr>
                                        }
                                    </tbody>
                                </table>
                            </div>
                        </div>
                    </section>
                }
                if section == Section::Server {
                    <section class="panel">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(language, Key::ServerAddress))</h2>
                        </div>
                        <div class="panel-body">
                            <label class="field">
                                <span class="field-label">(t(language, Key::EffectiveOrigin))</span>
                                <span class="field-static">(origin.clone())</span>
                            </label>
                            <form class="field" method="post" action="/api/settings/base_url">
                                <label class="field">
                                    <span class="field-label">(t(language, Key::BaseUrlLabel))</span>
                                    <input class="field-input" type="url" name="base_url" value=(stored_base_url) placeholder="https://files.example.com">
                                </label>
                                <p class="field-note">(t(language, Key::BaseUrlNote))</p>
                                <div class="panel-foot">
                                    <button class="primary" type="submit">(t(language, Key::Save))</button>
                                </div>
                            </form>
                        </div>
                    </section>
                }
            </main>
        </div>
        (crate::dropdown::dropdown_script(cx).await?)
    }
}

/// The just-minted token off the redirect's `?created=` pair, if present.
fn created_token(query: &str) -> Option<String> {
    query_value(query, "created").filter(|value| !value.is_empty())
}

/// When the link stops opening, or never on its own.
fn expiry_line(language: crate::i18n::Lang, expires_at: Option<time::OffsetDateTime>) -> String {
    match expires_at {
        Some(at) => format!(
            "{} {}",
            t(language, Key::ExpiresLabel),
            at.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "?".to_string())
        ),
        None => t(language, Key::NeverExpires).to_string(),
    }
}
