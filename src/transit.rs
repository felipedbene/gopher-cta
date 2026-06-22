//! Transit data sources.
//!
//! `TransitSource` is the seam for pluggable agencies. Only CTA is implemented
//! this run; `MetraSource` is a deliberate stub so the extension point is real
//! but unbuilt (no GTFS-RT). Static dispatch via native async-fn-in-trait keeps
//! it dependency-free — the server holds a concrete `CtaSource`.
//!
//! Wire parsing mirrors `cta-tui/src/cta.rs` (the source of truth): the CTA
//! JSON-from-XML conversion collapses single-element arrays into bare objects,
//! so every list field is really "one or many" and needs [`OneOrMany`].

use serde::Deserialize;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

const TT_BASE: &str = "https://lapi.transitchicago.com/api/1.0";
/// Default 'L' route keys, matching cta-tui's `CTA_ROUTES` default.
pub const DEFAULT_ROUTES: &[&str] = &["red", "blue", "brn", "g", "org", "p", "pink", "y"];

/// A single train with everything the map and per-line views need.
#[derive(Debug, Clone, PartialEq)]
pub struct Train {
    pub line: String,         // route key: "red", "blue", "brn", ...
    pub run: String,          // run number ("rn")
    pub dest: String,         // destination name
    pub next_station: String, // next station name
    pub lat: f64,
    pub lon: f64,
    pub heading: Option<u16>,
    pub approaching: bool,
    pub delayed: bool,
}

/// A snapshot of all live trains plus the feed's own timestamp.
#[derive(Debug, Clone, Default)]
pub struct Positions {
    pub trains: Vec<Train>,
    pub feed_time: Option<String>,
    /// True when these came from the bundled fixture rather than the live API.
    pub from_fixture: bool,
}

/// The pluggable-agency seam. Static dispatch only (no `dyn`): the server holds
/// a concrete source, and additional agencies slot in behind this trait. The
/// returned future is `Send` so sources can be driven from `tokio::spawn`.
pub trait TransitSource {
    /// Human label for menus / the night log.
    fn name(&self) -> &str;
    /// Fetch the current positions for this agency.
    fn positions(&self) -> impl std::future::Future<Output = Result<Positions, BoxErr>> + Send;
}

// ---------- CTA ----------

pub struct CtaSource {
    http: reqwest::Client,
    key: Option<String>,
    routes: Vec<String>,
    fixture: String, // raw JSON, loaded once at construction
}

impl CtaSource {
    /// Build a CTA source. `key` is the Train Tracker API key (None => offline
    /// fixture mode). `fixture` is the recorded `ttpositions` JSON used both as
    /// the offline data and as a fallback if a live fetch fails.
    pub fn new(key: Option<String>, routes: Vec<String>, fixture: String) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("gopher-cta/0.1 (+gopher)")
            .build()
            .expect("http client");
        CtaSource {
            http,
            key,
            routes,
            fixture,
        }
    }

    fn route_param(&self) -> String {
        self.routes.join(",")
    }

    async fn fetch_live(&self, key: &str) -> Result<Positions, BoxErr> {
        let url = format!(
            "{TT_BASE}/ttpositions.aspx?key={key}&rt={}&outputType=JSON",
            self.route_param()
        );
        let body = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let mut pos = parse_positions(&body)?;
        pos.from_fixture = false;
        Ok(pos)
    }
}

impl TransitSource for CtaSource {
    fn name(&self) -> &str {
        "CTA 'L'"
    }

    async fn positions(&self) -> Result<Positions, BoxErr> {
        // With a key, try live; on any failure fall back to the fixture so the
        // map always renders something (the failure is surfaced via the log,
        // not by blanking the server).
        if let Some(key) = &self.key {
            match self.fetch_live(key).await {
                Ok(pos) => return Ok(pos),
                Err(e) => {
                    eprintln!("[cta] live fetch failed, using fixture: {e}");
                }
            }
        }
        let mut pos = parse_positions(&self.fixture)?;
        pos.from_fixture = true;
        Ok(pos)
    }
}

// ---------- Metra (stub) ----------

/// Deliberate stub: the extension point exists, the implementation does not.
/// No GTFS-RT this run. `positions()` returns empty so a caller iterating over
/// sources degrades gracefully rather than erroring. Not wired into `main` yet
/// (the server serves CTA only), hence unconstructed outside tests.
#[allow(dead_code)]
pub struct MetraSource;

impl TransitSource for MetraSource {
    fn name(&self) -> &str {
        "Metra (unimplemented)"
    }

    async fn positions(&self) -> Result<Positions, BoxErr> {
        // TODO(felipe): implement Metra via the GTFS-RT vehicle-positions feed.
        Ok(Positions::default())
    }
}

// ---------- Wire parsing (mirrors cta-tui) ----------

/// CTA JSON collapses single-element arrays into a bare object: every list
/// field is "one or many".
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(x) => vec![x],
            OneOrMany::Many(v) => v,
        }
    }
}

fn flat<T>(o: Option<OneOrMany<T>>) -> Vec<T> {
    o.map(OneOrMany::into_vec).unwrap_or_default()
}

#[derive(Deserialize)]
struct PosResp {
    ctatt: PosCtatt,
}
#[derive(Deserialize)]
struct PosCtatt {
    tmst: Option<String>,
    #[serde(rename = "errCd")]
    err_cd: Option<String>,
    #[serde(rename = "errNm")]
    err_nm: Option<String>,
    route: Option<OneOrMany<RawRoute>>,
}
#[derive(Deserialize)]
struct RawRoute {
    #[serde(rename = "@name")]
    name: String,
    train: Option<OneOrMany<RawTrain>>,
}
#[derive(Deserialize)]
struct RawTrain {
    rn: Option<String>,
    #[serde(rename = "destNm")]
    dest_nm: Option<String>,
    #[serde(rename = "nextStaNm")]
    next_sta_nm: Option<String>,
    #[serde(rename = "isApp")]
    is_app: Option<String>,
    #[serde(rename = "isDly")]
    is_dly: Option<String>,
    lat: Option<String>,
    lon: Option<String>,
    heading: Option<String>,
}

fn truthy(s: &Option<String>) -> bool {
    matches!(s.as_deref(), Some("1") | Some("true"))
}

/// Parse a `ttpositions` JSON body into a flat list of trains. Trains missing a
/// usable lat/lon are dropped (they can't be plotted and have no map value).
pub fn parse_positions(body: &str) -> Result<Positions, BoxErr> {
    let resp: PosResp = serde_json::from_str(body)?;
    let ctatt = resp.ctatt;
    if ctatt.err_cd.as_deref().unwrap_or("0") != "0" {
        return Err(ctatt.err_nm.unwrap_or_else(|| "CTA error".into()).into());
    }
    let mut trains = Vec::new();
    for route in flat(ctatt.route) {
        let line = route.name.clone();
        for t in flat(route.train) {
            let (Some(lat), Some(lon)) = (
                t.lat.as_deref().and_then(|v| v.parse::<f64>().ok()),
                t.lon.as_deref().and_then(|v| v.parse::<f64>().ok()),
            ) else {
                continue;
            };
            trains.push(Train {
                line: line.clone(),
                run: t.rn.unwrap_or_default(),
                dest: t.dest_nm.unwrap_or_default(),
                next_station: t.next_sta_nm.unwrap_or_default(),
                lat,
                lon,
                heading: t.heading.and_then(|h| h.parse().ok()),
                approaching: truthy(&t.is_app),
                delayed: truthy(&t.is_dly),
            });
        }
    }
    Ok(Positions {
        trains,
        feed_time: ctatt.tmst,
        from_fixture: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/positions.json");

    #[test]
    fn parses_fixture_into_trains() {
        let pos = parse_positions(FIXTURE).unwrap();
        // 5 red + 4 blue + 2 brn + 2 g + 2 org + 1 p + 1 pink + 1 y = 18
        assert_eq!(pos.trains.len(), 18);
        assert_eq!(pos.feed_time.as_deref(), Some("2026-06-21T02:14:30"));
    }

    #[test]
    fn one_or_many_handles_single_train() {
        // The "y" route in the fixture has a single train object, not an array.
        let pos = parse_positions(FIXTURE).unwrap();
        let yellow: Vec<_> = pos.trains.iter().filter(|t| t.line == "y").collect();
        assert_eq!(yellow.len(), 1);
        assert_eq!(yellow[0].run, "601");
        assert_eq!(yellow[0].dest, "Howard");
    }

    #[test]
    fn flags_and_coords_parse() {
        let pos = parse_positions(FIXTURE).unwrap();
        let delayed: Vec<_> = pos.trains.iter().filter(|t| t.delayed).collect();
        assert_eq!(delayed.len(), 1); // blue run 153
        assert_eq!(delayed[0].run, "153");
        let app: Vec<_> = pos.trains.iter().filter(|t| t.approaching).collect();
        assert_eq!(app.len(), 3);
        let r801 = pos.trains.iter().find(|t| t.run == "801").unwrap();
        assert!((r801.lat - 42.00857).abs() < 1e-6);
        assert!((r801.lon - (-87.66145)).abs() < 1e-6);
        assert_eq!(r801.heading, Some(358));
    }

    #[test]
    fn cta_error_payload_is_an_error() {
        let body = r#"{"ctatt":{"errCd":"107","errNm":"Invalid API key","route":null}}"#;
        assert!(parse_positions(body).is_err());
    }

    #[tokio::test]
    async fn cta_source_falls_back_to_fixture_without_key() {
        let src = CtaSource::new(
            None,
            DEFAULT_ROUTES.iter().map(|s| s.to_string()).collect(),
            FIXTURE.to_string(),
        );
        let pos = src.positions().await.unwrap();
        assert!(pos.from_fixture);
        assert_eq!(pos.trains.len(), 18);
    }

    #[tokio::test]
    async fn metra_stub_is_empty() {
        let pos = MetraSource.positions().await.unwrap();
        assert!(pos.trains.is_empty());
        assert_eq!(MetraSource.name(), "Metra (unimplemented)");
    }
}
