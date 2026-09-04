//! Settings: the reader's profile and quota, plus the admin's user panel.
//!
//! `GET /settings` shows the profile im vouches for (name, address) with the
//! quota usage bar beside it, and the reader's own live share links with
//! their revoke buttons — including the copy-once banner after a creation
//! (`?created=<token>` on the query, rendered once and never stored).
//! Admins additionally see every account with its quota and disabled flag.
//! `POST /api/settings/quota|disable` are admin-only: the first sets a
//! user's byte quota, the second disables or re-enables them (a disabled
//! account reads as signed-out everywhere, the im session untouched, and
//! disabling yourself is refused).

use in_core::store::User;
use serde::Deserialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::request::uri;
use topcoat::router::{HeaderName, StatusCode, header, page, route};
use topcoat::view::view;

use crate::i18n::{Key, lang, t};
use crate::layout::{NavPage, topbar};
use crate::server::{Refusal, app, back_to, require_admin, require_user};
use crate::share::{refusal_banner, refusal_of};

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

/// The share of the quota already held, for the usage bar's width.
fn quota_percent(user: &User) -> u8 {
    if user.quota_bytes == 0 {
        return 100;
    }
    (user.used_bytes.saturating_mul(100) / user.quota_bytes).min(100) as u8
}

type Redirect = Result<(StatusCode, [(HeaderName, String); 1])>;

/// Back to settings, the refusal (if any) on the query — or `saved=<call>`
/// when there was nothing to refuse, the way iz's `saved_or_refused` marks
/// a save the page should chip.
fn redirect_back(cx: &Cx, call: &str, refusal: Option<Refusal>) -> Redirect {
    let back = back_to(cx, "/settings");
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
    quota_bytes: u64,
}

/// Sets one account's byte ceiling. The bytes it already holds are untouched —
/// a quota lowered under current usage refuses new uploads until the person
/// frees space, rather than deleting anything.
#[route(POST "/api/settings/quota")]
async fn set_quota(cx: &Cx, Form(input): Form<QuotaForm>) -> Redirect {
    if let Err(refusal) = require_admin(cx).await {
        return redirect_back(cx, "quota", Some(refusal));
    }
    match app(cx).store.set_user_quota(&input.user_id, input.quota_bytes).await {
        Ok(()) => redirect_back(cx, "quota", None),
        Err(error) => redirect_back(cx, "quota", Some(refusal_of(error))),
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
        Err(refusal) => return redirect_back(cx, "disable", Some(refusal)),
    };
    if input.user_id == admin.id {
        return redirect_back(cx, "disable", Some(Refusal::Forbidden));
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
        Ok(()) => redirect_back(cx, "disable", None),
        Err(error) => redirect_back(cx, "disable", Some(refusal_of(error))),
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
        Err(refusal) => return redirect_back(cx, "preferences", Some(refusal)),
    };
    if !UI_OPTIONS.contains(&input.ui.as_str()) {
        return redirect_back(cx, "preferences", Some(Refusal::BadUi));
    }
    if !THEME_OPTIONS.contains(&input.theme.as_str()) {
        return redirect_back(cx, "preferences", Some(Refusal::BadTheme));
    }
    if !LANGUAGE_OPTIONS.contains(&input.language.as_str()) {
        return redirect_back(cx, "preferences", Some(Refusal::BadLanguage));
    }
    match app(cx)
        .store
        .set_preferences(&user.id, &input.theme, &input.language, &input.ui)
        .await
    {
        Ok(()) => redirect_back(cx, "preferences", None),
        Err(error) => redirect_back(cx, "preferences", Some(refusal_of(error))),
    }
}

/// The reader's profile and quota, their live share links, and — for an
/// admin — every account with its quota and kill switch.
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
    let store = app(cx).store;
    // Re-read the row so the quota bar never shows a stale number.
    let fresh = store
        .user(&user.id)
        .await?
        .unwrap_or_else(|| user.clone());
    let links = store
        .share_links(&user.id)
        .await?;
    let live_links: Vec<_> = links
        .iter()
        .filter(|link| link.revoked_at.is_none())
        .collect();
    let users = if fresh.admin {
        store.users().await?
    } else {
        Vec::new()
    };
    let created = created_token(uri(cx).query().unwrap_or(""));
    let origin = app(cx).config.listen_url();
    view! {
        cx =>
        (topbar(cx, NavPage::Settings, &fresh, language).await?)
        <div class="settings-shell">
            <main class="settings-stage">
                <h1 class="settings-title">(t(language, Key::Settings))</h1>
                (refusal_banner(cx, language, &["create", "revoke", "add", "remove", "quota", "disable", "preferences"]).await?)
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
                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(language, Key::ManageLinks))</h2>
                    </div>
                    <div class="panel-body">
                        if live_links.is_empty() {
                            <p class="field-note">(t(language, Key::NoLinks))</p>
                        }
                        for link in live_links {
                            <div class="member-row">
                                <span class="member-name">(format!("{} · {}", link.kind.as_str(), link.target_id.clone()))</span>
                                <span class="field-note">(expiry_line(language, link.expires_at))</span>
                                <span class="field-note">(if link.can_download { t(language, Key::CanDownload) } else { t(language, Key::ViewOnly) })</span>
                                <form class="pop-row-form" method="post" action="/api/share/link/revoke">
                                    <input type="hidden" name="id" value=(link.id.clone())>
                                    <button type="submit">(t(language, Key::RevokeLink))</button>
                                </form>
                            </div>
                        }
                    </div>
                </section>
                if fresh.admin {
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
                                            <th class="member-col-role" scope="col"></th>
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
                                                <td class="member-col-role">
                                                    <form class="pop-row-form" method="post" action="/api/settings/quota">
                                                        <input type="hidden" name="user_id" value=(listed.id.clone())>
                                                        <input class="field-input" type="number" name="quota_bytes" min="0" value=(listed.quota_bytes.to_string()) aria-label=(t(language, Key::QuotaBytes))>
                                                        <button type="submit">(t(language, Key::SetQuota))</button>
                                                    </form>
                                                    if listed.id != fresh.id {
                                                        <form class="pop-row-form" method="post" action="/api/settings/disable">
                                                            <input type="hidden" name="user_id" value=(listed.id.clone())>
                                                            if listed.disabled {
                                                                <input type="hidden" name="disabled" value="0">
                                                                <button type="submit">(t(language, Key::EnableUser))</button>
                                                            } else {
                                                                <input type="hidden" name="disabled" value="1">
                                                                <button class="quiet-danger" type="submit">(t(language, Key::DisableUser))</button>
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
            </main>
        </div>
        (crate::dropdown::dropdown_script(cx).await?)
    }
}

/// The just-minted token off the redirect's `?created=` pair, if present.
fn created_token(query: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == "created" && !value.is_empty()).then(|| value.to_string())
    })
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
