//! The one family-health probe, shared by the signed-in wordmark's flyout:
//! a service is asked `GET {url}/healthz` where it stands — no credentials,
//! two seconds to answer — and the callers fan the probes out concurrently,
//! so a family member that is down costs its two seconds, not two seconds
//! each.

/// One `/healthz` reading. `Up` carries the body — the deploy contract's
/// `ok <build sha>` — and the answer's latency; everything else, refused
/// or wrong status or a body that does not begin ok or the two-second
/// ceiling, is `Down`.
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
