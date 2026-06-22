//! gopher-cta: a Gopher (RFC 1436) server serving live CTA 'L' train positions
//! as a Unicode-braille geographic map plus per-line text views.
//!
//! Configuration is entirely via environment variables:
//!   CTA_TRAIN_API_KEY  Train Tracker key. Unset => offline fixture mode.
//!   CTA_ROUTES         comma route keys (default: red,blue,brn,g,org,p,pink,y)
//!   GOPHER_PORT        listen port (default 7070)
//!   GOPHER_HOST        advertised host in menu links (default localhost)

mod braille;
mod project;
mod protocol;
mod render;
mod server;
mod transit;

use std::env;

use server::Server;
use transit::{CtaSource, DEFAULT_ROUTES};

/// The recorded positions snapshot, compiled in so the server boots and demos
/// fully offline with no key and no network.
const FIXTURE: &str = include_str!("../fixtures/positions.json");

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Load a local .env (gitignored) if present, so CTA_TRAIN_API_KEY and the
    // GOPHER_* overrides can live in a file instead of the shell environment.
    // A real exported env var still wins (dotenvy does not overwrite).
    let _ = dotenvy::dotenv();

    let host = env::var("GOPHER_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port: u16 = env::var("GOPHER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7070);

    let key = env::var("CTA_TRAIN_API_KEY").ok().filter(|k| !k.is_empty());
    let routes: Vec<String> = env::var("CTA_ROUTES")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|r| r.trim().to_string()).collect())
        .unwrap_or_else(|| DEFAULT_ROUTES.iter().map(|s| s.to_string()).collect());

    if key.is_some() {
        eprintln!("CTA_TRAIN_API_KEY set: serving live data (fixture fallback on error).");
    } else {
        eprintln!("CTA_TRAIN_API_KEY unset: serving bundled offline fixture.");
    }

    let source = CtaSource::new(key, routes, FIXTURE.to_string());
    let server = Server::new(host, port, source);
    server.run().await
}
