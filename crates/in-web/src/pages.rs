//! The front door.
//!
//! Which screen `/` is depends on the browser: a signed-in one is sent on
//! to the drive, a signed-out one is offered the sign-in card. There is no
//! setup screen — the first account is provisioned by signing in, and the
//! first user ever becomes the admin.
//!
//! Every page here is server-rendered on every request rather than fetched
//! once and patched: there is no hydration, so the gate a browser sees is
//! decided fresh each time.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{HeaderValue, StatusCode, header, page};
use topcoat::view::view;

use crate::i18n::{Key, Lang, lang, t};
use crate::layout::wordmark;
use crate::server::current_user;

/// The front door: the drive for a signed-in browser, the sign-in card for
/// everybody else.
#[page("/")]
async fn landing(cx: &Cx) -> Result {
    match current_user(cx).await {
        Ok(Some(_)) => {
            let location = (header::LOCATION, HeaderValue::from_static("/drive"));
            view! {
                cx =>
                (StatusCode::SEE_OTHER)
                (location)
            }
        }
        Ok(None) => sign_in_card(cx).await,
        Err(_) => view! {
            cx =>
            <main class="scaffold-note">
                <p>(t(Lang::En, Key::SomethingWentWrong))</p>
            </main>
        },
    }
}

/// The sign-in card for a browser with nobody in it: the wordmark and one
/// link, which starts the OIDC round-trip at `/auth/login`.
async fn sign_in_card(cx: &Cx) -> Result {
    let language = lang(cx).await;
    view! {
        cx =>
        <main class="auth-stage">
            <div class="auth-column">
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">(wordmark(cx).await?)</div>
                        <div class="auth-sub">(t(language, Key::WelcomeBlurb))</div>
                    </div>
                    <a class="auth-submit" href="/auth/login">
                        <span class="auth-submit-text">(t(language, Key::SignIn))</span>
                    </a>
                </div>
            </div>
        </main>
    }
}
