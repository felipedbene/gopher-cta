//! Pure rendering: parsed feed data in -> text / menu structures out.
//!
//! No sockets, no gopher protocol bytes, no daemon-specific formatting — this is
//! the testable core. Text pages come out as plain `String`s; menus come out as
//! a daemon-agnostic [`Vec<Entry>`]. Turning entries into a specific daemon's
//! index format (geomyidae `.gph`, Gophernicus `gophermap`, raw RFC-1436 bytes)
//! happens in the front end, not here.
//!
//! Selectors are the gopher selectors as served from the tree root, i.e. the
//! on-disk paths the fetcher writes:
//!   /map.txt            the braille map (text)
//!   /about.txt          about page (text)
//!   /<line>             per-line menu (a directory)
//!   /train/<run>.txt    per-train detail (text)

use crate::braille::Canvas;
use crate::project::{self, Geometry};
use crate::transit::{Positions, Train};

/// Route keys in board order.
pub const LINE_ORDER: &[&str] = &["red", "blue", "brn", "g", "org", "p", "pink", "y"];

/// Pretty display name for a route key (subset of cta-tui's `pretty_route`).
pub fn line_label(key: &str) -> &str {
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

/// CTA brand hex for a line key, shown on the per-train detail page.
pub fn line_color(key: &str) -> &'static str {
    match key {
        "red" => "#c60c30",
        "blue" => "#00a1de",
        "brn" => "#62361b",
        "g" => "#009b3a",
        "org" => "#f9461c",
        "p" | "pexp" => "#522398",
        "pink" => "#e27ea6",
        "y" => "#f9e300",
        _ => "#888888",
    }
}

/// Gopher item type for a link. Daemon-agnostic; serialized per-daemon elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Text, // gopher type 0
    Menu, // gopher type 1
}

/// One line of a menu: either an info line (not selectable) or a link.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Info(String),
    Link {
        kind: ItemKind,
        display: String,
        selector: String,
    },
}

fn info(s: impl Into<String>) -> Entry {
    Entry::Info(s.into())
}

fn link(kind: ItemKind, display: impl Into<String>, selector: impl Into<String>) -> Entry {
    Entry::Link {
        kind,
        display: display.into(),
        selector: selector.into(),
    }
}

/// Selector (served path) for a train's detail page.
pub fn train_selector(run: &str) -> String {
    format!("/train/{run}.txt")
}

/// Selector (served path) for a line's menu directory.
pub fn line_selector(line: &str) -> String {
    format!("/{line}")
}

/// The root menu: a link to the map, one entry per line (with a live count), and
/// an about link. Drill-down begins here.
pub fn root_menu(pos: &Positions) -> Vec<Entry> {
    let mut e = vec![
        info("==============================================="),
        info("  gopher-cta : live CTA 'L' trains over Gopher"),
        info("==============================================="),
        info(""),
        link(ItemKind::Text, "Live train map (braille)", "/map.txt"),
        link(
            ItemKind::Text,
            "Geographic atlas (coast + landmarks)",
            "/atlas.txt",
        ),
        info(""),
        link(
            ItemKind::Text,
            "Dispatch (summary + feed stats)",
            "/dispatch.txt",
        ),
        link(ItemKind::Text, "SITREP (AI alerts summary)", "/sitrep.txt"),
        link(ItemKind::Text, "Event advisory (AI)", "/events.txt"),
        info(""),
        info("Trains by line:"),
    ];
    for &key in LINE_ORDER {
        let count = pos.trains.iter().filter(|t| t.line == key).count();
        e.push(link(
            ItemKind::Menu,
            format!("{:<8} ({} running)", line_label(key), count),
            line_selector(key),
        ));
    }
    e.push(info(""));
    e.push(link(ItemKind::Text, "About this service", "/about.txt"));
    e.push(info(""));
    e.push(info(
        "Data: CTA Train Tracker. Not affiliated with the CTA.",
    ));
    if pos.from_fixture {
        e.push(info("(offline demo data from bundled fixture)"));
    }
    e
}

/// Per-line menu: each running train is a link drilling into its detail page.
pub fn line_menu(pos: &Positions, line: &str) -> Vec<Entry> {
    let mut trains: Vec<&Train> = pos.trains.iter().filter(|t| t.line == line).collect();
    trains.sort_by(|a, b| a.run.cmp(&b.run));

    let mut e = vec![
        info(format!("{} Line -- live trains", line_label(line))),
        info("=".repeat(40)),
    ];
    if trains.is_empty() {
        if LINE_ORDER.contains(&line) {
            e.push(info("No trains currently reporting on this line."));
        } else {
            e.push(info(format!("Unknown line '{line}'.")));
            e.push(info("Known: red blue brn g org p pink y"));
        }
        return e;
    }
    e.push(info(format!(
        "{} train(s) running -- select one for details:",
        trains.len()
    )));
    e.push(info(""));
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
        let display = format!("Run {:<5} -> {}{}", t.run, t.dest, flag_str);
        // The target is a text page, so the link is type 0 (text), not type 1
        // (RFC 1436): a client fetches it as a document, not a directory.
        e.push(link(ItemKind::Text, display, train_selector(&t.run)));
    }
    if pos.from_fixture {
        e.push(info(""));
        e.push(info("(offline demo data from bundled fixture)"));
    }
    e
}

/// The per-train detail page (plain text). Reuses the current snapshot only — no
/// second fetch — so the upcoming-stop sequence is just the next stop the
/// positions feed reports. An unknown/expired run yields a clean notice.
pub fn train_page(pos: &Positions, run_id: &str) -> String {
    let Some(t) = pos.trains.iter().find(|t| t.run == run_id) else {
        let mut out = String::new();
        out.push_str(&format!("Run {run_id} -- no longer reporting\n"));
        out.push_str(&"=".repeat(40));
        out.push_str("\n\nThis run is not in the current live feed. It may have finished\n");
        out.push_str("its trip, gone out of service, or changed run number.\n\n");
        out.push_str("Head back to the line listing to pick another train.\n");
        return out;
    };

    let status = if t.delayed {
        "DELAYED".to_string()
    } else if t.approaching {
        format!("approaching {}", t.next_station)
    } else {
        "en route".to_string()
    };
    let heading = t
        .heading
        .map(|h| format!("{h:03}"))
        .unwrap_or_else(|| "unknown".into());
    let next = match &t.arr_time {
        Some(arr) => format!("{}  (predicted {arr})", t.next_station),
        None => t.next_station.clone(),
    };

    let mut out = String::new();
    out.push_str(&format!("Run {} -- {} Line\n", t.run, line_label(&t.line)));
    out.push_str(&"=".repeat(40));
    out.push('\n');
    out.push('\n');
    out.push_str(&format!(
        "line:        {} ({})\n",
        line_label(&t.line),
        line_color(&t.line)
    ));
    out.push_str(&format!("status:      {status}\n"));
    out.push_str(&format!("destination: {}\n", t.dest));
    out.push_str(&format!(
        "position:    {:.5}, {:.5}   heading {heading}\n",
        t.lat, t.lon
    ));
    out.push_str(&format!("next stop:   {next}\n"));
    out.push_str(
        "\nThe CTA positions feed reports only the next stop, so the full\n\
         upcoming-stop sequence isn't available here without a separate\n\
         per-station query.\n",
    );
    if pos.from_fixture {
        out.push_str("\n(offline demo data from bundled fixture)\n");
    }
    out
}

/// The headline view: a braille geographic plot of every live train (plain text).
/// Trains that fail to plot (null coords / outside bbox) are logged at debug
/// level rather than silently dropped.
pub fn map_page(pos: &Positions, geo: &Geometry, source_name: &str) -> String {
    let mut canvas = Canvas::new(geo.wc, geo.hc);
    let mut plotted = 0usize;
    let mut dropped: Vec<&str> = Vec::new();
    for t in &pos.trains {
        if let Some((px, py)) = project::project(t.lat, t.lon, geo) {
            canvas.set(px, py);
            plotted += 1;
        } else {
            dropped.push(&t.run);
        }
    }
    if !dropped.is_empty() {
        // Debug-level diagnostic (no logging crate in use; stderr matches the
        // convention elsewhere). Makes the "X of Y reporting" gap traceable.
        eprintln!(
            "[debug][map] dropped {} train(s) that failed to plot (null coords / outside bbox): {}",
            dropped.len(),
            dropped.join(", ")
        );
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
    out.push_str("\nlegend (trains per line):\n");
    for &key in LINE_ORDER {
        let count = pos.trains.iter().filter(|t| t.line == key).count();
        if count > 0 {
            out.push_str(&format!("  {:<8} {}\n", line_label(key), count));
        }
    }
    out
}

/// The about page (plain text).
pub fn about_page() -> String {
    let g = project::geometry();
    format!(
        "gopher-cta\n\
         ==========\n\n\
         Live CTA 'L' train positions as a Unicode-braille geographic map, plus\n\
         per-line listings and per-train detail pages. Rendered to static gopher\n\
         files by a fetcher and served by a gopher daemon.\n\n\
         Canvas: {wc}x{hc} braille cells ({wp}x{hp} pixels).\n\
         Projection: km-based bbox map, cos(lat) longitude shrink + terminal\n\
         cell-aspect correction, so the city renders north-up and undistorted.\n\n\
         Built in Rust. Data from the CTA Train Tracker API.\n\
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

    // -- known feed -> expected listing --

    #[test]
    fn root_menu_links_map_and_each_line() {
        let entries = root_menu(&fixture_positions());
        // map link
        assert!(entries.iter().any(|e| matches!(e,
            Entry::Link { kind: ItemKind::Text, selector, .. } if selector == "/map.txt")));
        // one menu link per line, pointing at /<line>
        for &k in LINE_ORDER {
            let want = line_selector(k);
            assert!(
                entries.iter().any(|e| matches!(e,
                    Entry::Link { kind: ItemKind::Menu, selector, .. } if *selector == want)),
                "missing line link {k}"
            );
        }
    }

    #[test]
    fn line_menu_lists_red_trains_as_links() {
        let entries = line_menu(&fixture_positions(), "red");
        // header info present
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Info(s) if s == "Red Line -- live trains")));
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Info(s) if s.contains("5 train(s) running"))));
        // five type-0 train links with the expected selectors and displays
        let links: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Link {
                    kind: ItemKind::Text,
                    display,
                    selector,
                } => Some((display.as_str(), selector.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 5);
        assert!(links.contains(&("Run 801   -> Howard", "/train/801.txt")));
        for run in ["801", "812", "823", "834", "845"] {
            assert!(
                links
                    .iter()
                    .any(|(_, sel)| *sel == format!("/train/{run}.txt")),
                "missing /train/{run}.txt"
            );
        }
    }

    #[test]
    fn line_menu_unknown_line() {
        let entries = line_menu(&fixture_positions(), "chartreuse");
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Info(s) if s.contains("Unknown line"))));
        // no train links for a bogus line
        assert!(!entries.iter().any(|e| matches!(e, Entry::Link { .. })));
    }

    // -- map page --

    #[test]
    fn map_page_has_braille_and_footer() {
        let text = map_page(&fixture_positions(), &project::geometry(), "CTA 'L'");
        assert!(text
            .chars()
            .any(|c| (0x2800..=0x28FF).contains(&(c as u32))));
        assert!(text.contains("18 trains plotted of 18 reporting"));
        assert!(text.contains("legend"));
        assert!(text.contains("offline fixture"));
    }

    #[test]
    fn map_page_row_count_matches_geometry() {
        let geo = project::geometry();
        let text = map_page(&fixture_positions(), &geo, "CTA 'L'");
        let braille_rows = text
            .lines()
            .filter(|l| l.chars().any(|c| (0x2800..=0x28FF).contains(&(c as u32))))
            .count();
        assert_eq!(braille_rows, geo.hc);
    }

    // -- train detail --

    #[test]
    fn train_page_valid_id() {
        let text = train_page(&fixture_positions(), "801");
        assert!(text.starts_with("Run 801 -- Red Line"));
        assert!(text.contains("line:        Red (#c60c30)"));
        assert!(text.contains("destination: Howard"));
        assert!(text.contains("next stop:   Loyola"));
        assert!(text.contains("predicted 2026-06-21T02:17:00"));
        assert!(text.contains("heading 358"));
    }

    #[test]
    fn train_page_approaching_status() {
        let text = train_page(&fixture_positions(), "812");
        assert!(text.contains("status:      approaching"));
    }

    #[test]
    fn train_page_unknown_id() {
        let text = train_page(&fixture_positions(), "9999");
        assert!(text.starts_with("Run 9999 -- no longer reporting"));
        assert!(!text.contains("position:"));
    }
}
