//! gopher-cta: live CTA 'L' train positions as a Unicode-braille geographic map,
//! per-line listings, and per-train detail pages.
//!
//! Two front ends share one render core:
//!   gopher-cta fetch [--once] [--interval <secs>] [--out <dir>]
//!       Fetch the feed and publish a static gopher tree for a daemon to serve.
//!   gopher-cta serve            (transitional; removed once fetch fully lands)
//!       Run the built-in gopher server directly.
//!
//! Configuration via environment variables:
//!   CTA_TRAIN_API_KEY  Train Tracker key. Unset => offline fixture mode.
//!   CTA_ROUTES         comma route keys (default: red,blue,brn,g,org,p,pink,y)
//!   GOPHER_OUT         fetch output dir (default: public)
//!   GOPHER_PORT        serve listen port (default 7070)
//!   GOPHER_HOST        serve advertised host in menu links (default localhost)

mod braille;
mod fetch;
mod project;
mod protocol;
mod render;
mod server;
mod transit;

use std::env;
use std::io;

use server::Server;
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

    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("fetch") => {
            let cfg = fetch::Config::from_args(&args[2..]).map_err(io::Error::other)?;
            fetch::run(cfg, source).await
        }
        Some("serve") | None => {
            let host = env::var("GOPHER_HOST").unwrap_or_else(|_| "localhost".to_string());
            let port: u16 = env::var("GOPHER_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(7070);
            Server::new(host, port, source).run().await
        }
        Some(other) => Err(io::Error::other(format!(
            "unknown command: {other} (expected `fetch` or `serve`)"
        ))),
    }
}
