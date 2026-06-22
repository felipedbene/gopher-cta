//! AI narrative panels (Dispatch / SITREP / Event Advisory) for the gopher tree.
//!
//! These three texts are generated and cached by the deployed Worker — the same
//! one the `cta-tui` daemon polls. Nothing here talks to DeepSeek: the Worker is
//! the single generation point, and we are just another reader of its endpoints.
//!
//! The fetcher's train path (fast, ~30s) must never hard-depend on this, so the
//! poller runs as a DETACHED background task updating a shared [`NarrationView`].
//! Each publish reads a clone of that view and never blocks on the network. On a
//! fetch error the prior text is kept; an endpoint never reached yet renders a
//! placeholder. Panels refresh on slow cadences (Worker is heavily cached):
//! dispatch ~1 min, SITREP ~5 min, events ~30 min — mirroring the TUI daemon.
//!
//! Config (env):
//!   CTA_AI_BASE     Worker base URL (default: production worker).
//!   CTA_HOME_MAPID  SITREP station map id (default: 41070 = Kedzie/Green).
//!   CTA_HOME_NAME   SITREP station label   (default: Kedzie).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::transit::Positions;

/// Per-panel refresh cadences, in seconds (mirrors the cta-tui daemon).
const DISPATCH_SECS: i64 = 60;
const SITREP_SECS: i64 = 300;
const EVENTS_SECS: i64 = 1800;

/// How often the background poller wakes to check which panels are due.
const TICK_SECS: u64 = 30;

/// Per-request timeout so a hung Worker can't stall the poller indefinitely
/// (and, being off the publish path, never stalls the train map either).
const HTTP_TIMEOUT_SECS: u64 = 8;

/// Worker base URL; override with `CTA_AI_BASE`.
fn base() -> String {
    std::env::var("CTA_AI_BASE")
        .unwrap_or_else(|_| "https://cta-track-grid.felipe-debene.workers.dev".into())
}

/// Current wall-clock time in epoch seconds (used to age the narrative panels).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Shape of every AI endpoint response (`{summary, count, error}`), matching the
/// Worker contract the cta-tui client uses.
#[derive(Deserialize, Default)]
struct AiResp {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// One cached panel: the last-good text and when it was last successfully
/// fetched (`None` = never retrieved).
#[derive(Clone, Default)]
pub struct Panel {
    pub summary: String,
    pub updated_at: Option<i64>,
}

/// The three panels plus the SITREP station label, cloned cheaply per publish.
#[derive(Clone, Default)]
pub struct NarrationView {
    pub dispatch: Panel,
    pub sitrep: Panel,
    pub events: Panel,
    pub home_name: String,
}

/// Spawn the background poller and return the shared view it keeps fresh. The
/// view starts empty (placeholders) and fills in as the Worker answers; the
/// caller reads `lock().clone()` each publish and never awaits the network.
pub fn spawn() -> Arc<Mutex<NarrationView>> {
    let home_mapid = std::env::var("CTA_HOME_MAPID").unwrap_or_else(|_| "41070".into());
    let home_name = std::env::var("CTA_HOME_NAME").unwrap_or_else(|_| "Kedzie".into());

    let shared = Arc::new(Mutex::new(NarrationView {
        home_name: home_name.clone(),
        ..Default::default()
    }));
    let view = shared.clone();

    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .user_agent("gopher-cta/0.1 (+narration)")
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[narration] http client build failed, narration disabled: {e}");
                return;
            }
        };

        // Last-attempt timestamps drive the per-panel cadence; all start due.
        let (mut last_dispatch, mut last_sitrep, mut last_events) = (None, None, None);
        loop {
            let now = now_secs();
            if due(last_dispatch, now, DISPATCH_SECS) {
                last_dispatch = Some(now);
                fetch_into(&view, &client, "/api/feed/narration", now, |v| {
                    &mut v.dispatch
                })
                .await;
            }
            if due(last_sitrep, now, SITREP_SECS) {
                last_sitrep = Some(now);
                let path = format!(
                    "/api/alerts/summary?station={}&stn={}",
                    urlenc(&home_mapid),
                    urlenc(&home_name)
                );
                fetch_into(&view, &client, &path, now, |v| &mut v.sitrep).await;
            }
            if due(last_events, now, EVENTS_SECS) {
                last_events = Some(now);
                fetch_into(&view, &client, "/api/events/advisory", now, |v| {
                    &mut v.events
                })
                .await;
            }
            tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;
        }
    });

    shared
}

/// Whether a panel last attempted at `last` is due again at `now`.
fn due(last: Option<i64>, now: i64, interval: i64) -> bool {
    last.is_none_or(|t| now - t >= interval)
}

/// Fetch one endpoint and, on success, write the text into the panel selected by
/// `pick`. On any error (network, HTTP, empty/`error` body) the prior text is
/// kept — the lock is only taken to apply a good result.
async fn fetch_into(
    view: &Arc<Mutex<NarrationView>>,
    client: &reqwest::Client,
    path: &str,
    now: i64,
    pick: impl Fn(&mut NarrationView) -> &mut Panel,
) {
    match fetch(client, path).await {
        Ok(summary) => {
            let mut v = view.lock().unwrap();
            let panel = pick(&mut v);
            panel.summary = summary;
            panel.updated_at = Some(now);
        }
        Err(e) => eprintln!("[narration] {path} fetch failed (keeping last-good): {e}"),
    }
}

/// GET a Worker endpoint, returning its non-empty `summary` text.
async fn fetch(client: &reqwest::Client, path: &str) -> Result<String, String> {
    let url = format!("{}{path}", base());
    let resp: AiResp = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(e) = resp.error {
        return Err(e);
    }
    let summary = resp.summary.unwrap_or_default();
    if summary.trim().is_empty() {
        return Err("empty summary".into());
    }
    Ok(summary)
}

/// Minimal percent-encoding for query values (station names contain spaces, `/`).
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------- page rendering (pure: cached text in -> gopher text body out) ----------

/// Human age of a panel's last update, e.g. "updated 2m ago" / "not yet
/// retrieved". `now` is passed in so rendering stays pure and testable.
fn fmt_age(updated_at: Option<i64>, now: i64) -> String {
    match updated_at {
        None => "not yet retrieved".into(),
        Some(t) => {
            let age = (now - t).max(0);
            if age < 90 {
                format!("updated {age}s ago")
            } else if age < 5400 {
                format!("updated {}m ago", age / 60)
            } else {
                format!("updated {}h ago", age / 3600)
            }
        }
    }
}

/// `/dispatch.txt`: the one-line AI summary plus authoritative feed stats
/// (total / approaching / delayed / last feed) read straight from the positions
/// snapshot — so the page is useful even when the narration is unavailable.
pub fn dispatch_page(view: &NarrationView, pos: &Positions, now: i64) -> String {
    let approaching = pos.trains.iter().filter(|t| t.approaching).count();
    let delayed = pos.trains.iter().filter(|t| t.delayed).count();

    let summary = if view.dispatch.summary.trim().is_empty() {
        "(dispatch narration unavailable — feed stats below)".to_string()
    } else {
        view.dispatch.summary.clone()
    };

    let mut out = String::new();
    out.push_str("CTA 'L' -- dispatch\n");
    out.push_str(&"=".repeat(40));
    out.push_str("\n\n");
    out.push_str(&summary);
    out.push_str("\n\nfeed stats:\n");
    out.push_str(&format!("  trains reporting : {}\n", pos.trains.len()));
    out.push_str(&format!("  approaching      : {approaching}\n"));
    out.push_str(&format!("  delayed          : {delayed}\n"));
    out.push_str(&format!(
        "  last feed        : {}\n",
        pos.feed_time.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "\nsource: AI dispatch narration ({})\n",
        fmt_age(view.dispatch.updated_at, now)
    ));
    out
}

/// `/sitrep.txt`: the AI alerts SITREP body for the configured home station.
pub fn sitrep_page(view: &NarrationView, now: i64) -> String {
    body_page(
        "CTA 'L' -- SITREP",
        Some(&view.home_name),
        &view.sitrep,
        "(SITREP unavailable right now.)",
        "AI alerts summary",
        now,
    )
}

/// `/events.txt`: the AI Event Advisory body.
pub fn events_page(view: &NarrationView, now: i64) -> String {
    body_page(
        "CTA 'L' -- event advisory",
        None,
        &view.events,
        "(no event advisory available right now.)",
        "AI events advisory",
        now,
    )
}

/// Shared layout for the SITREP / events bodies.
fn body_page(
    title: &str,
    station: Option<&str>,
    panel: &Panel,
    placeholder: &str,
    source: &str,
    now: i64,
) -> String {
    let mut out = String::new();
    out.push_str(title);
    out.push('\n');
    out.push_str(&"=".repeat(40));
    out.push('\n');
    if let Some(s) = station {
        out.push_str(&format!("station: {s}\n"));
    }
    out.push('\n');
    if panel.summary.trim().is_empty() {
        out.push_str(placeholder);
    } else {
        out.push_str(&panel.summary);
    }
    out.push_str(&format!(
        "\n\nsource: {source} ({})\n",
        fmt_age(panel.updated_at, now)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transit::parse_positions;

    const FIXTURE: &str = include_str!("../fixtures/positions.json");

    fn fixture_positions() -> Positions {
        let mut p = parse_positions(FIXTURE).unwrap();
        p.from_fixture = true;
        p
    }

    #[test]
    fn due_cadence() {
        assert!(due(None, 1000, 60)); // never fetched -> due
        assert!(!due(Some(1000), 1030, 60)); // 30s < 60s -> not due
        assert!(due(Some(1000), 1060, 60)); // exactly the interval -> due
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(None, 1000), "not yet retrieved");
        assert_eq!(fmt_age(Some(1000), 1042), "updated 42s ago");
        assert_eq!(fmt_age(Some(1000), 1300), "updated 5m ago");
        assert_eq!(fmt_age(Some(1000), 1000 + 7200), "updated 2h ago");
    }

    #[test]
    fn dispatch_page_has_summary_and_feed_stats() {
        let view = NarrationView {
            dispatch: Panel {
                summary: "Service running well system-wide.".into(),
                updated_at: Some(900),
            },
            ..Default::default()
        };
        let page = dispatch_page(&view, &fixture_positions(), 942);
        assert!(page.starts_with("CTA 'L' -- dispatch"));
        assert!(page.contains("Service running well system-wide."));
        assert!(page.contains("trains reporting : 18"));
        assert!(page.contains("approaching      : 3")); // fixture has 3 approaching
        assert!(page.contains("delayed          : 1")); // fixture has 1 delayed
        assert!(page.contains("last feed        : 2026-06-21T02:14:30"));
        assert!(page.contains("updated 42s ago"));
    }

    #[test]
    fn dispatch_page_placeholder_without_narration() {
        let page = dispatch_page(&NarrationView::default(), &fixture_positions(), 1000);
        assert!(page.contains("dispatch narration unavailable"));
        // feed stats still render — the page is useful without the AI text.
        assert!(page.contains("trains reporting : 18"));
        assert!(page.contains("not yet retrieved"));
    }

    #[test]
    fn sitrep_page_shows_station_and_body() {
        let view = NarrationView {
            sitrep: Panel {
                summary: "Minor delays inbound.".into(),
                updated_at: Some(500),
            },
            home_name: "Kedzie".into(),
            ..Default::default()
        };
        let page = sitrep_page(&view, 560);
        assert!(page.starts_with("CTA 'L' -- SITREP"));
        assert!(page.contains("station: Kedzie"));
        assert!(page.contains("Minor delays inbound."));
        assert!(page.contains("source: AI alerts summary"));
    }

    #[test]
    fn events_page_placeholder() {
        let page = events_page(&NarrationView::default(), 1000);
        assert!(page.starts_with("CTA 'L' -- event advisory"));
        assert!(page.contains("no event advisory available"));
        assert!(page.contains("AI events advisory"));
    }

    #[test]
    fn urlenc_encodes_spaces_and_slash() {
        assert_eq!(urlenc("Kedzie"), "Kedzie");
        assert_eq!(urlenc("95th/Dan Ryan"), "95th%2FDan%20Ryan");
    }
}
