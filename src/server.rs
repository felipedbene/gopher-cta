//! Live gopher server (transitional).
//!
//! This is now a thin adapter over the pure [`crate::render`] module: it owns
//! only the tokio accept loop, selector routing, and serialization of render
//! output into RFC-1436 wire bytes. All content/menu construction lives in
//! `render`. The fetcher front end reuses the same `render` functions to write
//! static files; this server is removed once the fetcher fully replaces it.

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::project::{self, Geometry};
use crate::protocol::{render_menu, render_text, ItemType, MenuItem};
use crate::render::{self, Entry, ItemKind};
use crate::transit::{Positions, TransitSource};

/// Serialize a daemon-agnostic menu ([`Entry`] list) into RFC-1436 wire bytes,
/// filling in this server's advertised host/port for every link.
fn menu_to_wire(entries: &[Entry], host: &str, port: u16) -> String {
    let items: Vec<MenuItem> = entries
        .iter()
        .map(|e| match e {
            Entry::Info(s) => MenuItem::info(s.clone()),
            Entry::Link {
                kind,
                display,
                selector,
            } => {
                let it = match kind {
                    ItemKind::Text => ItemType::Text,
                    ItemKind::Menu => ItemType::Menu,
                };
                MenuItem::link(it, display.clone(), selector.clone(), host, port)
            }
        })
        .collect();
    render_menu(&items)
}

pub struct Server<S: TransitSource> {
    host: String,
    port: u16,
    source: S,
    geo: Geometry,
}

impl<S: TransitSource + Send + Sync + 'static> Server<S> {
    pub fn new(host: String, port: u16, source: S) -> Self {
        Server {
            host,
            port,
            source,
            geo: project::geometry(),
        }
    }

    /// Bind and serve forever. One selector per connection, then close.
    pub async fn run(self) -> io::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", self.port)).await?;
        eprintln!(
            "gopher-cta listening on 0.0.0.0:{} (advertising {}:{})",
            self.port, self.host, self.port
        );
        let me = Arc::new(self);
        loop {
            let (stream, peer) = listener.accept().await?;
            let me = Arc::clone(&me);
            tokio::spawn(async move {
                if let Err(e) = me.serve_conn(stream).await {
                    eprintln!("[conn {peer}] {e}");
                }
            });
        }
    }

    async fn serve_conn(&self, mut stream: TcpStream) -> io::Result<()> {
        // Read up to the first CRLF (or a sane cap) — the selector line.
        let mut buf = Vec::with_capacity(128);
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await?;
            if n == 0 {
                break; // client closed before sending CRLF
            }
            buf.push(byte[0]);
            if buf.ends_with(b"\n") || buf.len() > 1024 {
                break;
            }
        }
        let raw = String::from_utf8_lossy(&buf);
        let selector = crate::protocol::parse_selector(&raw);
        let response = self.route(selector).await;
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        Ok(())
    }

    /// Map a selector to a full gopher wire response (menu or text, terminated),
    /// delegating all content to `render`. Selectors are the served tree paths.
    async fn route(&self, selector: &str) -> String {
        let pos = self.snapshot().await;
        let sel = selector.trim_end_matches('/');
        match sel {
            "" => menu_to_wire(&render::root_menu(&pos), &self.host, self.port),
            "/map.txt" => render_text(&render::map_page(&pos, &self.geo, self.source.name())),
            "/about.txt" => render_text(&render::about_page()),
            s if s.starts_with("/train/") && s.ends_with(".txt") => {
                let run = &s["/train/".len()..s.len() - ".txt".len()];
                render_text(&render::train_page(&pos, run))
            }
            s if s.starts_with('/') => {
                let line = &s[1..];
                menu_to_wire(&render::line_menu(&pos, line), &self.host, self.port)
            }
            other => render_text(&format!(
                "Unknown selector: {other}\r\n\r\nGo back to the root menu."
            )),
        }
    }

    async fn snapshot(&self) -> Positions {
        match self.source.positions().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[snapshot] {e}");
                Positions::default()
            }
        }
    }
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
    fn root_menu_serializes_to_wire() {
        let wire = menu_to_wire(&render::root_menu(&fixture_positions()), "localhost", 7070);
        assert!(wire.ends_with(".\r\n"));
        // map link is a type-0 line pointing at /map.txt on this host
        assert!(wire.contains("0Live train map (braille)\t/map.txt\tlocalhost\t7070\r\n"));
        // each line is a type-1 submenu link
        assert!(wire.contains("\t/red\tlocalhost\t7070\r\n"));
    }

    #[test]
    fn line_menu_serializes_train_links() {
        let wire = menu_to_wire(&render::line_menu(&fixture_positions(), "red"), "h", 70);
        assert!(wire.ends_with(".\r\n"));
        assert!(wire.contains("0Run 801"));
        assert!(wire.contains("\t/train/801.txt\th\t70\r\n"));
    }

    #[test]
    fn info_lines_serialize_as_type_i() {
        let wire = menu_to_wire(&[Entry::Info("hello".into())], "h", 70);
        assert!(wire.starts_with("ihello\t"));
    }
}
