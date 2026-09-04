//! The live channel: one long-lived connection per open tab, carrying the news
//! that something changed.
//!
//! What travels here is a topic and nothing else — never a row, never a name.
//! That is the whole security design. A channel that carried data would need
//! an ownership check on every message, and the day somebody adds a topic and
//! forgets the check is the day one person's drive leaks to another. A topic
//! name cannot leak anything: the client is told only *which* surface moved,
//! and re-fetches it through the ordinary route, where the ordinary owner
//! gate answers. The one thing this file must still get right is not naming
//! somebody else's surface to this reader — [`may_hear`] is that, and it is
//! the reason the filter runs here rather than in the browser.

use std::time::Duration;

use in_core::{Change, Topic};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;
use topcoat::context::{Cx, try_app_context};
use topcoat::router::content::sse::{Event, KeepAlive, Sse};
use topcoat::router::response::{IntoResponse, Response};
use topcoat::router::{StatusCode, route};

use crate::server::{app, require_user};

/// How long one connection is held before the server ends it and the browser
/// opens another. Set from `config/in.toml`; absent — as in the test router
/// — the default stands in.
#[derive(Clone, Copy, Debug)]
pub struct LiveWindow(pub Duration);

impl Default for LiveWindow {
    fn default() -> Self {
        Self(Duration::from_secs(300))
    }
}

/// Tells a live stream that the process is going down.
///
/// Without this, stopping the server takes as long as its graceful shutdown
/// allows — thirty seconds by default. The server stops accepting connections
/// and then waits for in-flight requests to finish, and an open live stream is
/// an in-flight request that intends to sit there for minutes. Every open tab
/// is one. So the streams are told, and they end; the browser reconnects when
/// the server comes back, which is what it does after any dropped connection.
#[derive(Clone)]
pub struct Shutdown(pub tokio::sync::watch::Receiver<bool>);

/// Whether this reader may be told that this surface moved.
///
/// Every topic carries its owner's id, and a reader hears only their own —
/// one person's drive never names itself on another person's connection. The
/// one exception is the admin surface, which an admin hears about: someone
/// who can act on every account may be told every account moved.
fn may_hear(topic: &Topic, me: &str, admin: bool) -> bool {
    if topic.id() == me {
        return true;
    }
    matches!(topic, Topic::Admin(_)) && admin
}

/// What one announcement looks like on the wire.
#[derive(Serialize)]
struct Frame<'a> {
    topic: &'a str,
    /// The owner the topic is about. Always the reader's own id by the time
    /// it is sent — see `may_hear` — so it carries no news, only a handle
    /// for the client to route on.
    id: &'a str,
    seq: u64,
}

/// A frame the client reads as "you may have missed something; re-read
/// everything you are showing".
const RESYNC: &str = r#"{"topic":"resync"}"#;

#[route(GET "/api/live")]
async fn live(cx: &Cx) -> topcoat::Result<Response> {
    let Ok(user) = require_user(cx).await else {
        return (StatusCode::UNAUTHORIZED, "").into_response(cx);
    };
    let admin = user.admin;
    let me = user.id.clone();
    let rx = app(cx).store.subscribe();
    let stopping = try_app_context::<Shutdown>(cx).map(|s| s.0.clone());
    let deadline = Instant::now()
        + try_app_context::<LiveWindow>(cx)
            .copied()
            .unwrap_or_default()
            .0;

    let events = futures_util::stream::unfold(
        (rx, admin, me, deadline, stopping),
        |(mut rx, admin, me, deadline, mut stopping)| async move {
            loop {
                // Already going down: say nothing and end, so this connection
                // is not one the shutdown has to sit and wait out.
                if stopping.as_ref().is_some_and(|watch| *watch.borrow()) {
                    return None;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                // Three things end the wait: an announcement, the window
                // running out, and the server being told to stop. The third is
                // watched rather than polled, so Ctrl+C is felt at once.
                let heard = tokio::time::timeout(left, async {
                    match stopping.as_mut() {
                        Some(watch) => tokio::select! {
                            _ = watch.changed() => None,
                            got = rx.recv() => Some(got),
                        },
                        None => Some(rx.recv().await),
                    }
                })
                .await;
                let frame = match heard {
                    // The window closed, or the server is stopping. Ending the
                    // stream is the point: the browser reconnects by itself and
                    // authenticates again.
                    Err(_) | Ok(None) => return None,
                    // The store is gone, which means the process is going.
                    Ok(Some(Err(RecvError::Closed))) => return None,
                    // This reader fell far enough behind that announcements were
                    // dropped. Which ones is unknowable, so the client is told to
                    // re-read everything rather than shown a mix of before and
                    // after. The stream stays open — a slow moment is not a fault.
                    Ok(Some(Err(RecvError::Lagged(_)))) => Event::new().data(RESYNC),
                    Ok(Some(Ok(Change { topic, seq }))) => {
                        // Not merely unsent: never named. Another owner's
                        // drive carries no evidence on this connection that
                        // it exists at all.
                        if !may_hear(&topic, &me, admin) {
                            continue;
                        }
                        let frame = Frame {
                            topic: topic.kind(),
                            id: topic.id(),
                            seq,
                        };
                        match Event::new().json_data(&frame) {
                            Ok(event) => event,
                            Err(problem) => {
                                return Some((Err(problem), (rx, admin, me, deadline, stopping)));
                            }
                        }
                    }
                };
                return Some((Ok(frame), (rx, admin, me, deadline, stopping)));
            }
        },
    );

    Sse::new(events)
        .keep_alive(KeepAlive::new())
        .into_response(cx)
}
