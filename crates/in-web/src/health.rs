//! The one family-health probe, shared by the signed-in wordmark's flyout:
//! a service is asked `GET {url}/healthz` where it stands — no credentials,
//! two seconds to answer — and the callers fan the probes out concurrently,
//! so a family member that is down costs its two seconds, not two seconds
//! each.
//!
//! A reading outlives its render: [`family_probes`] caches one round for
//! [`PROBE_TTL`] and reuses it for every page painted inside the window, so
//! a dark sibling stalls one render per window, not every render. The
//! client is shared too — one connection pool for the whole process, built
//! once — instead of a fresh one per render.

use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// One `/healthz` reading. `Up` carries the body — the deploy contract's
/// `ok <build sha>` — and the answer's latency; everything else, refused
/// or wrong status or a body that does not begin ok or the two-second
/// ceiling, is `Down`.
#[derive(Clone)]
pub enum Probe {
    Up { body: String, ms: u128 },
    Down,
}

pub(crate) async fn probe_healthz(http: &reqwest::Client, url: &str) -> Probe {
    let started = std::time::Instant::now();
    let Ok(answer) = http
        .get(url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    else {
        return Probe::Down;
    };
    if !answer.status().is_success() {
        return Probe::Down;
    }
    let body = answer.text().await.unwrap_or_default();
    let body = body.trim();
    if body.starts_with("ok") {
        Probe::Up {
            body: body.chars().take(64).collect(),
            ms: started.elapsed().as_millis(),
        }
    } else {
        Probe::Down
    }
}

/// How long one round of probes stays true. Longer than a walk through the
/// pages, shorter than a sibling's outage outlasting notice.
const PROBE_TTL: Duration = Duration::from_secs(30);

/// The one client every probe rides: `reqwest` pools connections per
/// client, so a fresh `Client::new()` per render would also mean a fresh
/// TLS handshake per render. Built once, shared.
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// One cached round: when it was read, and what each sibling read — keyed
/// by the sibling's key and address together, so a moved service probes
/// again even inside the window.
type Round = (Instant, Vec<(String, String, Probe)>);

static ROUND: LazyLock<Mutex<Round>> = LazyLock::new(|| Mutex::new((Instant::now(), Vec::new())));

/// The family's health, one `Probe` per `(key, url)` pair, in order. A
/// round younger than [`PROBE_TTL`] over the same key-and-address set is
/// reused; anything else fans the probes out concurrently and files the
/// round. The two-second per-sibling ceiling lives in [`probe_healthz`].
pub(crate) async fn family_probes(siblings: &[(&str, &str)]) -> Vec<Probe> {
    let lock = || ROUND.lock().unwrap_or_else(PoisonError::into_inner);
    {
        let round = lock();
        let same = round.0.elapsed() < PROBE_TTL
            && round.1.len() == siblings.len()
            && round
                .1
                .iter()
                .zip(siblings)
                .all(|((key, url, _), (want_key, want_url))| key == want_key && url == want_url);
        if same {
            return round.1.iter().map(|(_, _, probe)| probe.clone()).collect();
        }
    }
    let mut handles = Vec::new();
    for (_, url) in siblings {
        let http = CLIENT.clone();
        let url = format!("{}/healthz", url.trim_end_matches('/'));
        handles.push(tokio::spawn(async move { probe_healthz(&http, &url).await }));
    }
    let mut round: Vec<(String, String, Probe)> = Vec::new();
    for ((key, url), handle) in siblings.iter().zip(handles) {
        let probe = handle.await.unwrap_or(Probe::Down);
        round.push(((*key).to_string(), (*url).to_string(), probe));
    }
    let probes = round.iter().map(|(_, _, probe)| probe.clone()).collect();
    *lock() = (Instant::now(), round);
    probes
}
