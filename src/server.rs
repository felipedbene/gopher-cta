//! Gopher server: tokio accept loop, selector routing, and the view builders
//! that turn a `Positions` snapshot into gopher menus / text.
//!
//! The view builders ([`build_root`], [`build_map`], ...) are pure functions of
//! their inputs so they can be unit-tested without a socket; the async layer
//! just fetches a snapshot and feeds it in.

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::braille::Canvas;
use crate::project::{self, Geometry};
use crate::protocol::{render_menu, render_text, ItemType, MenuItem};
use crate::transit::{Positions, Train, TransitSource};

/// Pretty display name for a route key (subset of cta-tui's `pretty_route`).
fn line_label(key: &str) -> &str {
    match key {
        "red" => "Red",
        "blue" => "Blue",
        "brn" => "Brown",
        "g" => "Green",
        "org" => "Orange",
        "p" | "pexp" => "Purple",
        "pink" => "Pink",
        "y" => "Yellow",
        other => other,
    }
}

/// The route keys we advertise in the `/cta` menu, in board order.
const LINE_ORDER: &[&str] = &["red", "blue", "brn", "g", "org", "p", "pink", "y"];

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

    /// Map a selector to a full gopher wire response (menu or text, terminated).
    async fn route(&self, selector: &str) -> String {
        // Normalize: treat "", "/" identically as root.
        let sel = selector.trim_end_matches('/');
        match sel {
            "" => build_root(&self.host, self.port),
            "/about" => render_text(&about_text()),
            "/map" => {
                let pos = self.snapshot().await;
                render_text(&build_map(&pos, &self.geo, self.source.name()))
            }
            "/cta" => {
                let pos = self.snapshot().await;
                build_cta_menu(&pos, &self.host, self.port)
            }
            s if s.starts_with("/cta/") => {
                let line = &s["/cta/".len()..];
                let pos = self.snapshot().await;
                render_text(&build_line_text(&pos, line))
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

// ---------- Pure view builders ----------

/// The root menu.
pub fn build_root(host: &str, port: u16) -> String {
    let items = vec![
        MenuItem::info("==============================================="),
        MenuItem::info("  gopher-cta : live CTA 'L' trains over Gopher"),
        MenuItem::info("==============================================="),
        MenuItem::info(""),
        MenuItem::link(
            ItemType::Text,
            "Live train map (braille)",
            "/map",
            host,
            port,
        ),
        MenuItem::link(ItemType::Menu, "Trains by line", "/cta", host, port),
        MenuItem::link(ItemType::Text, "About this server", "/about", host, port),
        MenuItem::info(""),
        MenuItem::info("Data: CTA Train Tracker. Not affiliated with the CTA."),
    ];
    render_menu(&items)
}

/// The `/cta` menu: one entry per line, with a live train count.
pub fn build_cta_menu(pos: &Positions, host: &str, port: u16) -> String {
    let mut items = vec![
        MenuItem::info("CTA 'L' lines -- select one for live positions"),
        MenuItem::info(""),
    ];
    for &key in LINE_ORDER {
        let count = pos.trains.iter().filter(|t| t.line == key).count();
        let display = format!("{:<8} ({} running)", line_label(key), count);
        items.push(MenuItem::link(
            ItemType::Text,
            display,
            format!("/cta/{key}"),
            host,
            port,
        ));
    }
    render_menu(&items)
}

/// Per-line text view: one block per train (run, dest, next stop, flags).
pub fn build_line_text(pos: &Positions, line: &str) -> String {
    let mut trains: Vec<&Train> = pos.trains.iter().filter(|t| t.line == line).collect();
    trains.sort_by(|a, b| a.run.cmp(&b.run));

    let mut out = String::new();
    out.push_str(&format!("{} Line -- live trains\n", line_label(line)));
    out.push_str(&"=".repeat(40));
    out.push('\n');
    if trains.is_empty() {
        if LINE_ORDER.contains(&line) {
            out.push_str("\nNo trains currently reporting on this line.\n");
        } else {
            out.push_str(&format!("\nUnknown line '{line}'.\n"));
            out.push_str("Known: red blue brn g org p pink y\n");
        }
        return out;
    }
    out.push_str(&format!("\n{} train(s) running:\n\n", trains.len()));
    for t in trains {
        let mut flags = Vec::new();
        if t.approaching {
            flags.push("approaching");
        }
        if t.delayed {
            flags.push("DELAYED");
        }
        let flag_str = if flags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", flags.join(", "))
        };
        let head = t
            .heading
            .map(|h| format!(" hdg {h:03}"))
            .unwrap_or_default();
        out.push_str(&format!("  Run {:<5} -> {}{}\n", t.run, t.dest, flag_str));
        out.push_str(&format!(
            "    next: {:<24}{}  ({:.5}, {:.5})\n",
            t.next_station, head, t.lat, t.lon
        ));
    }
    if pos.from_fixture {
        out.push_str("\n(offline demo data from bundled fixture)\n");
    }
    out
}

/// The headline view: a braille geographic plot of every live train.
pub fn build_map(pos: &Positions, geo: &Geometry, source_name: &str) -> String {
    let mut canvas = Canvas::new(geo.wc, geo.hc);
    let mut plotted = 0usize;
    for t in &pos.trains {
        if let Some((px, py)) = project::project(t.lat, t.lon, geo) {
            canvas.set(px, py);
            plotted += 1;
        }
    }

    let mut out = String::new();
    out.push_str("CTA 'L' -- live train map\n");
    out.push_str(&format!(
        "source: {source_name}{}\n",
        if pos.from_fixture {
            " (offline fixture)"
        } else {
            ""
        }
    ));
    out.push_str(&format!(
        "feed time: {}\n",
        pos.feed_time.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&"-".repeat(geo.wc.min(78)));
    out.push('\n');
    out.push_str(&canvas.render());
    out.push('\n');
    out.push_str(&"-".repeat(geo.wc.min(78)));
    out.push('\n');
    out.push_str(&format!(
        "{} trains plotted of {} reporting.  bbox lat[{}..{}] lon[{}..{}]\n",
        plotted,
        pos.trains.len(),
        project::LAT_MIN,
        project::LAT_MAX,
        project::LON_MIN,
        project::LON_MAX,
    ));
    // Per-line legend with counts (one dot = one train; braille is monochrome).
    out.push_str("\nlegend (trains per line):\n");
    for &key in LINE_ORDER {
        let count = pos.trains.iter().filter(|t| t.line == key).count();
        if count > 0 {
            out.push_str(&format!("  {:<8} {}\n", line_label(key), count));
        }
    }
    out
}

fn about_text() -> String {
    let g = project::geometry();
    format!(
        "gopher-cta\n\
         ==========\n\n\
         A Gopher (RFC 1436) server that plots live CTA 'L' train positions as a\n\
         Unicode-braille geographic map, projected from lat/lon onto a sub-character\n\
         canvas. Per-line text positions are served under /cta.\n\n\
         Canvas: {wc}x{hc} braille cells ({wp}x{hp} pixels).\n\
         Projection: linear bbox map with cos(lat) longitude aspect correction.\n\n\
         Built in Rust (tokio). Data from the CTA Train Tracker API.\n\
         Not affiliated with the Chicago Transit Authority.\n",
        wc = g.wc,
        hc = g.hc,
        wp = g.wp,
        hp = g.hp,
    )
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
    fn root_menu_is_well_formed() {
        let menu = build_root("localhost", 7070);
        assert!(menu.ends_with(".\r\n"));
        assert!(menu.contains("\t/map\tlocalhost\t7070\r\n"));
        assert!(menu.contains("1Trains by line\t/cta\tlocalhost\t7070\r\n"));
    }

    #[test]
    fn map_has_braille_and_legend() {
        let pos = fixture_positions();
        let geo = project::geometry();
        let text = build_map(&pos, &geo, "CTA 'L'");
        // contains braille glyphs (>= U+2800)
        assert!(text
            .chars()
            .any(|c| (c as u32) >= 0x2800 && (c as u32) <= 0x28FF));
        assert!(text.contains("18 trains plotted of 18 reporting") || text.contains("plotted"));
        assert!(text.contains("legend"));
        assert!(text.contains("offline fixture"));
    }

    #[test]
    fn map_row_count_matches_geometry() {
        let pos = fixture_positions();
        let geo = project::geometry();
        let text = build_map(&pos, &geo, "CTA 'L'");
        // The canvas block is hc rows; find them by braille content.
        let braille_rows = text
            .lines()
            .filter(|l| l.chars().any(|c| (0x2800..=0x28FF).contains(&(c as u32))))
            .count();
        assert_eq!(braille_rows, geo.hc);
    }

    #[test]
    fn all_fixture_trains_plot_inside_bbox() {
        let pos = fixture_positions();
        let geo = project::geometry();
        let text = build_map(&pos, &geo, "CTA 'L'");
        assert!(text.contains("18 trains plotted of 18 reporting"));
    }

    #[test]
    fn line_text_lists_red_trains() {
        let pos = fixture_positions();
        let text = build_line_text(&pos, "red");
        assert!(text.starts_with("Red Line"));
        assert!(text.contains("5 train(s) running"));
        assert!(text.contains("Run 801"));
        assert!(text.contains("Howard"));
    }

    #[test]
    fn line_text_unknown_line() {
        let pos = fixture_positions();
        let text = build_line_text(&pos, "chartreuse");
        assert!(text.contains("Unknown line"));
    }

    #[test]
    fn cta_menu_lists_all_lines_with_counts() {
        let pos = fixture_positions();
        let menu = build_cta_menu(&pos, "h", 70);
        assert!(menu.ends_with(".\r\n"));
        for &k in LINE_ORDER {
            assert!(menu.contains(&format!("/cta/{k}\t")), "missing line {k}");
        }
        assert!(menu.contains("(5 running)")); // red
    }
}
