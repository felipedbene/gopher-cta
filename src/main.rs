//! gopher-cta: fetch live CTA 'L' train positions and publish a static gopher
//! tree — a Unicode-braille geographic map, per-line listings, and per-train
//! detail pages — for an external gopher daemon (geomyidae) to serve.
//!
//! Usage:
//!   gopher-cta [--once] [--interval <secs>] [--out <dir>]
//!
//! Configuration via environment variables:
//!   CTA_TRAIN_API_KEY  Train Tracker key. Unset => offline fixture mode.
//!   CTA_ROUTES         comma route keys (default: red,blue,brn,g,org,p,pink,y)
//!   GOPHER_OUT         output dir (default: public); the daemon serves <out>/current

mod atlas;
mod braille;
mod fetch;
mod project;
mod render;
mod transit;

use std::env;
use std::io;

use transit::{CtaSource, DEFAULT_ROUTES};

/// The recorded positions snapshot, compiled in so the tool runs fully offline
/// with no key and no network.
const FIXTURE: &str = include_str!("../fixtures/positions.json");

#[tokio::main]
async fn main() -> io::Result<()> {
    // Load a local .env (gitignored) if present; a real exported env var wins.
    let _ = dotenvy::dotenv();

    let key = env::var("CTA_TRAIN_API_KEY").ok().filter(|k| !k.is_empty());
    let routes: Vec<String> = env::var("CTA_ROUTES")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|r| r.trim().to_string()).collect())
        .unwrap_or_else(|| DEFAULT_ROUTES.iter().map(|s| s.to_string()).collect());

    if key.is_some() {
        eprintln!("CTA_TRAIN_API_KEY set: live data (fixture fallback on error).");
    } else {
        eprintln!("CTA_TRAIN_API_KEY unset: using bundled offline fixture.");
    }
    let source = CtaSource::new(key, routes, FIXTURE.to_string());

    // The binary is the fetcher; all args are fetch flags. Tolerate a leading
    // `fetch` token for muscle memory from the transitional two-subcommand form.
    let args: Vec<String> = env::args().collect();
    let flags = match args.get(1).map(String::as_str) {
        Some("fetch") => &args[2..],
        _ => &args[1..],
    };
    let cfg = fetch::Config::from_args(flags).map_err(io::Error::other)?;
    fetch::run(cfg, source).await
}
