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
//!   /faq.txt /help.txt  FAQ + troubleshooting (text)
//!   /dig.txt            hidden easter egg (in the tree, linked from no menu)
//!   /<line>             per-line menu (a directory)
//!   /train/<run>.txt    per-train detail (text)

use crate::atlas::{bresenham, GeoData};
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

/// ANSI 256-colour code approximating each CTA line's brand colour, for the
/// colour (`.ansi`) map variants. `0` would mean "no colour", so every line maps
/// to a real code.
pub fn line_ansi256(key: &str) -> u8 {
    match key {
        "red" => 196,
        "blue" => 39,
        "brn" => 130,
        "g" => 40,
        "org" => 208,
        "p" | "pexp" => 93,
        "pink" => 213,
        "y" => 226,
        _ => 244,
    }
}

/// 8-point compass label for a heading in degrees (0 = N, clockwise). Diagonals
/// included, so `045 -> NE`, `358 -> N`.
pub fn cardinal8(deg: u16) -> &'static str {
    const PTS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    PTS[(((deg % 360) as u32 + 22) / 45 % 8) as usize]
}

/// Gopher item type for a link. Daemon-agnostic; serialized per-daemon elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Text, // gopher type 0
    Menu, // gopher type 1
    Url,  // gopher type h -- external link, selector is `URL:<addr>`
    Bin,  // gopher type 9 -- binary download (e.g. the source tarball)
}

/// One line of a menu: either an info line (not selectable) or a link.
///
/// `host`/`port` are normally `None` — the serializer then emits geomyidae's own
/// host/port placeholders, so the tree stays address-agnostic. They are `Some`
/// only for cross-server links (e.g. the hub link to the phlog hole), which must
/// advertise a concrete address the client dials directly.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Info(String),
    Link {
        kind: ItemKind,
        display: String,
        selector: String,
        host: Option<String>,
        port: Option<u16>,
    },
}

fn info(s: impl Into<String>) -> Entry {
    Entry::Info(s.into())
}

/// A link served from this tree (host/port default to the daemon's own).
fn link(kind: ItemKind, display: impl Into<String>, selector: impl Into<String>) -> Entry {
    Entry::Link {
        kind,
        display: display.into(),
        selector: selector.into(),
        host: None,
        port: None,
    }
}

/// A link to a *different* gopher server: the `.gph` line advertises this
/// host/port so the client opens a fresh connection there.
fn link_remote(
    kind: ItemKind,
    display: impl Into<String>,
    selector: impl Into<String>,
    host: impl Into<String>,
    port: u16,
) -> Entry {
    Entry::Link {
        kind,
        display: display.into(),
        selector: selector.into(),
        host: Some(host.into()),
        port: Some(port),
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
/// an about link. Drill-down begins here. `phlog` is the optional hub link to the
/// sibling blog hole (`--phlog-link`): when `Some((host, port))` a single type-1
/// entry advertises that address; `None` omits it.
pub fn root_menu(pos: &Positions, src_available: bool, phlog: Option<(&str, u16)>) -> Vec<Entry> {
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
        link(ItemKind::Text, "  -> colour (ANSI): map", "/map.ansi"),
        link(ItemKind::Text, "  -> colour (ANSI): atlas", "/atlas.ansi"),
        link(ItemKind::Menu, "Chicago landmarks", "/landmarks"),
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
    e.push(link(ItemKind::Text, "FAQ", "/faq.txt"));
    e.push(link(ItemKind::Text, "Troubleshooting", "/help.txt"));
    // Served source tarball (gopher type 9). Only advertised when the archive is
    // actually present in the snapshot (baked into the image; absent in a bare
    // `cargo run`), so the link never dangles.
    if src_available {
        e.push(link(
            ItemKind::Bin,
            "Source code (tar.gz, fetch over gopher)",
            "/src.tar.gz",
        ));
    }
    e.push(link(
        ItemKind::Url,
        "Source code (GitHub)",
        "URL:https://github.com/felipedbene/gopher-cta",
    ));
    e.push(link(
        ItemKind::Url,
        "CTA tracker -- the web original (tracker.debene.dev)",
        "URL:https://tracker.debene.dev/",
    ));
    // Hub link to the sibling phlog hole (gopher-blog). A cross-server type-1
    // link: the client dials the advertised host/port directly; :70 never proxies.
    if let Some((host, port)) = phlog {
        e.push(link_remote(
            ItemKind::Menu,
            "Phlog -- the blog",
            "/",
            host,
            port,
        ));
    }
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
        // Fold the next stop into the status so a flag is meaningful: "approaching
        // X" reads, where a bare "[approaching]" left you guessing. Falls back to a
        // plain flag when the feed reports no next station.
        let next = t.next_station.trim();
        let status = match (t.delayed, t.approaching, next.is_empty()) {
            (true, _, false) => format!("DELAYED, next {next}"),
            (true, _, true) => "DELAYED".to_string(),
            (false, true, false) => format!("approaching {next}"),
            (false, true, true) => "approaching".to_string(),
            (false, false, false) => format!("next {next}"),
            (false, false, true) => String::new(),
        };
        let display = if status.is_empty() {
            format!("Run {:<5} -> {}", t.run, t.dest)
        } else {
            format!("Run {:<5} -> {:<14} {status}", t.run, t.dest)
        };
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
        .map(|h| format!("{h:03} ({})", cardinal8(h)))
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

// ANSI 256-colour codes for the braille map's geographic skeleton, drawn only in
// the `.ansi` variant. Trains keep their CTA line colour and win any shared cell,
// so the skeleton recedes behind live trains. Coast matches the atlas's water.
const GEO_COAST_COLOR: u8 = 44; // cyan: Lake Michigan shoreline
                                // The river is water too, so it shares the coast's cyan rather than a blue of its
                                // own — a distinct blue read as Blue Line trains, especially in the dense Loop.
const GEO_RIVER_COLOR: u8 = GEO_COAST_COLOR;
const GEO_ROAD_COLOR: u8 = 240; // dark grey: expressways
const GEO_LABEL_COLOR: u8 = 231; // bright white: place codes, so they read over the plot

/// The body of water, written down the open lake east of the coastline (same
/// placement the atlas uses: the far-east column, vertically centred).
const LAKE_LABEL: &str = "LAKE MICHIGAN";

/// Pre-rasterized Chicago skeleton for the braille map's ANSI variant: shoreline,
/// river, and expressways drawn ONCE into a braille canvas (with a matching
/// per-cell base colour). [`map_page_ansi`] clones this and paints live trains on
/// top, so geo never re-rasterizes — mirroring the atlas's build-once invariant.
/// The same [`project::project`] the trains use places every anchor, so the
/// skeleton is pixel-locked to the dots; there is no second projection.
pub struct MapBase {
    canvas: Canvas,
    colors: Vec<u8>,
    /// Sparse text layer (one slot per cell): place codes that render *over* the
    /// plot. Static, so it's built once and cloned-by-reference per publish.
    labels: Vec<Option<(char, u8)>>,
    /// `(code, name)` for every place code that actually landed, in placement
    /// order — the footer renders this as the decode legend (shared scheme with
    /// the atlas).
    legend: Vec<(String, String)>,
}

impl MapBase {
    /// Rasterize the static geo skeleton into a braille base. Call once at
    /// startup; clone-and-paint per publish via [`map_page_ansi`].
    pub fn build(geo: &Geometry) -> MapBase {
        let data = GeoData::load();
        let mut canvas = Canvas::new(geo.wc, geo.hc);
        let mut colors = vec![0u8; geo.wc * geo.hc];

        // Roads first, then water on top: where a road meets the river or coast,
        // the more iconic water colour wins the shared cell. (Dots just OR, so
        // draw order only decides the per-cell tint.)
        for road in &data.expressways {
            draw_polyline(&mut canvas, &mut colors, geo, &road.points, GEO_ROAD_COLOR);
        }
        draw_polyline(
            &mut canvas,
            &mut colors,
            geo,
            &data.shoreline.points,
            GEO_COAST_COLOR,
        );
        for river in &data.rivers {
            draw_polyline(
                &mut canvas,
                &mut colors,
                geo,
                &river.points,
                GEO_RIVER_COLOR,
            );
        }

        // Text layer: "LAKE MICHIGAN" down the open-water column (placed first so
        // it owns that column), then mnemonic codes for the same places the atlas
        // names — areas first, then landmarks — collision-avoided so the dense
        // downtown thins gracefully. A footer legend decodes them.
        let mut labels = vec![None; geo.wc * geo.hc];
        place_lake_label(&mut labels, geo);
        let mut legend: Vec<(String, String)> = Vec::new();
        let placeable = data
            .areas
            .iter()
            .map(|a| (&a.code, &a.name, a.lat, a.lon))
            .chain(
                data.landmarks
                    .iter()
                    .map(|m| (&m.code, &m.name, m.lat, m.lon)),
            );
        for (code, name, lat, lon) in placeable {
            if let Some((px, py)) = project::project(lat, lon, geo) {
                if place_label(
                    &mut labels,
                    geo,
                    (px / 2) as i32,
                    (py / 4) as i32,
                    code,
                    GEO_LABEL_COLOR,
                ) {
                    legend.push((code.clone(), name.clone()));
                }
            }
        }

        MapBase {
            canvas,
            colors,
            labels,
            legend,
        }
    }
}

/// Write `text` left-to-right into the label layer starting at cell `(col, row)`,
/// clipping anything past the frame — but ONLY if it doesn't overlap a label
/// already placed, so codes thin gracefully in dense areas (the loser drops whole)
/// instead of garbling. Off-frame characters clip and don't block. Returns whether
/// the label landed.
fn place_label(
    labels: &mut [Option<(char, u8)>],
    geo: &Geometry,
    col: i32,
    row: i32,
    text: &str,
    color: u8,
) -> bool {
    if row < 0 || row as usize >= geo.hc {
        return false;
    }
    let r = row as usize;
    for i in 0..text.chars().count() as i32 {
        let c = col + i;
        if (0..geo.wc as i32).contains(&c) && labels[r * geo.wc + c as usize].is_some() {
            return false;
        }
    }
    for (i, ch) in text.chars().enumerate() {
        let c = col + i as i32;
        if c < 0 || c as usize >= geo.wc {
            continue;
        }
        labels[r * geo.wc + c as usize] = Some((ch, color));
    }
    true
}

/// Write "LAKE MICHIGAN" vertically down the far-east (open-water) column,
/// vertically centred — the same placement the atlas uses, so both surfaces label
/// the lake the same way.
fn place_lake_label(labels: &mut [Option<(char, u8)>], geo: &Geometry) {
    let col = geo.wc as i32 - 2;
    let start = (geo.hc as i32 - LAKE_LABEL.len() as i32) / 2;
    for (i, ch) in LAKE_LABEL.chars().enumerate() {
        // Place the inter-word space too, so it RESERVES that cell — otherwise a
        // place code could slot into the lake column between the two words.
        place_label(
            labels,
            geo,
            col,
            start + i as i32,
            &ch.to_string(),
            GEO_COAST_COLOR,
        );
    }
}

/// Rasterize a lat/lon polyline into the braille canvas: project each anchor to a
/// braille pixel (the SAME projection the trains use) and Bresenham between
/// consecutive in-bbox anchors, setting the dot and tinting its cell `color`. A
/// segment with an off-bbox endpoint is skipped, so the line clips at the frame
/// instead of streaking — exactly like the atlas's shoreline.
fn draw_polyline(
    canvas: &mut Canvas,
    colors: &mut [u8],
    geo: &Geometry,
    pts: &[[f64; 2]],
    color: u8,
) {
    for seg in pts.windows(2) {
        if let (Some((x0, y0)), Some((x1, y1))) = (
            project::project(seg[0][0], seg[0][1], geo),
            project::project(seg[1][0], seg[1][1], geo),
        ) {
            bresenham(x0 as i32, y0 as i32, x1 as i32, y1 as i32, |px, py| {
                if px < 0 || py < 0 {
                    return;
                }
                let (px, py) = (px as usize, py as usize);
                canvas.set(px, py);
                let cell = (py / 4) * geo.wc + (px / 2);
                if cell < colors.len() {
                    colors[cell] = color;
                }
            });
        }
    }
}

/// The headline view: a braille geographic plot of every live train (plain text).
/// Trains that fail to plot (null coords / outside bbox) are logged at debug
/// level rather than silently dropped.
pub fn map_page(pos: &Positions, geo: &Geometry, source_name: &str) -> String {
    map_page_inner(None, pos, geo, source_name, false)
}

/// As [`map_page`], but ANSI-coloured and laid over the Chicago skeleton from
/// `base` (coast/river/expressways): each train cell takes its CTA line colour and
/// wins over the geo tint. For the `.ansi` selector; strict clients use
/// [`map_page`], which carries no overlay.
pub fn map_page_ansi(base: &MapBase, pos: &Positions, geo: &Geometry, source_name: &str) -> String {
    map_page_inner(Some(base), pos, geo, source_name, true)
}

fn map_page_inner(
    base: Option<&MapBase>,
    pos: &Positions,
    geo: &Geometry,
    source_name: &str,
    ansi: bool,
) -> String {
    // ANSI starts from the cloned geo skeleton; plain text starts blank.
    let (mut canvas, mut colors) = match base {
        Some(b) => (b.canvas.clone(), b.colors.clone()),
        None => (Canvas::new(geo.wc, geo.hc), vec![0u8; geo.wc * geo.hc]),
    };
    let mut plotted = 0usize;
    let mut dropped: Vec<&str> = Vec::new();
    for t in &pos.trains {
        if let Some((px, py)) = project::project(t.lat, t.lon, geo) {
            canvas.set(px, py);
            // Cell colour = the line of the last train to fall in it.
            colors[(py / 4) * geo.wc + (px / 2)] = line_ansi256(&t.line);
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
    out.push_str(&if ansi {
        // Labels come from the (static) base; plain text carries no overlay.
        let labels = base.map(|b| b.labels.as_slice()).unwrap_or(&[]);
        canvas.render_colored(&colors, labels)
    } else {
        canvas.render()
    });
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
    // The geo skeleton is ANSI-only; only name it when it's actually drawn.
    if let Some(b) = base {
        out.push_str(
            "overlay: water -- lake + river (cyan)  expressways (grey)  place codes (white)\n",
        );
        // Decode legend for the inline place codes actually on the grid (same
        // mnemonic scheme as the atlas).
        if !b.legend.is_empty() {
            out.push_str("\nplaces (code -> name):\n");
            for (code, name) in &b.legend {
                out.push_str(&format!("  {code:<4} {name}\n"));
            }
        }
    }
    out.push_str("\nlegend (trains per line):\n");
    for &key in LINE_ORDER {
        let count = pos.trains.iter().filter(|t| t.line == key).count();
        if count > 0 {
            out.push_str(&format!("  {:<8} {}\n", line_label(key), count));
        }
    }
    out
}

/// ASCII masthead for the about page — a little CTA 'L' car, branded.
const ABOUT_ICON: &str = r#"        ______________________________
     __/  []  []   gopher-cta   []    \__
    |  |____________________________|    |
    |__|  o   o   o   o   o   o   o  |____|
    /  (O)========================(O)     \
   '----------------------------------------'
           live CTA 'L', dot by dot"#;

/// The about page (plain text).
pub fn about_page() -> String {
    let g = project::geometry();
    format!(
        "{icon}\n\n\
         Live CTA 'L' train positions, rendered as a Unicode-braille map of\n\
         Chicago and served over Gopher -- the 1991 protocol, no HTTP, no\n\
         JavaScript.\n\n\
         What's here:\n\
         \x20 * a braille train map (plain), and an ANSI colour version overlaying\n\
         \x20   the Chicago coastline, river, expressways and landmark codes under\n\
         \x20   the trains\n\
         \x20 * a matching char-cell geographic atlas (atlas.ansi)\n\
         \x20 * per-line listings and a detail page for every running train\n\
         \x20 * Chicago landmarks, each with its own page\n\
         \x20 * AI narration panels -- dispatch, a per-station SITREP, event\n\
         \x20   advisories (fed by the cta-track-grid Worker; see below)\n\n\
         How it works:\n\
         \x20 A Rust fetcher polls the CTA Train Tracker API, renders a complete\n\
         \x20 static gopher tree each cycle, and atomically swaps it into place;\n\
         \x20 the geomyidae daemon serves it. One km-based projection (cos(lat)\n\
         \x20 longitude shrink + terminal cell-aspect correction) draws the city\n\
         \x20 north-up and undistorted. It cross-compiles to big-endian PowerPC --\n\
         \x20 it runs on a PowerMac G5.\n\n\
         \x20 Canvas: {wc}x{hc} braille cells ({wp}x{hp} pixels).\n\n\
         Source:  https://github.com/felipedbene/gopher-cta\n\
         Live:    gopher://gopher.debene.dev:70/\n\n\
         Part of a small CTA family. The flagship is cta-track-grid -- a\n\
         real-time web tracker whose Worker also powers the AI panels above:\n\
         \x20 https://tracker.debene.dev/\n\
         \x20 https://github.com/felipedbene/cta-track-grid\n\n\
         Built in Rust. Data from the CTA Train Tracker API.\n\
         Not affiliated with the Chicago Transit Authority.\n",
        icon = ABOUT_ICON,
        wc = g.wc,
        hc = g.hc,
        wp = g.wp,
        hp = g.hp,
    )
}

/// FAQ page (`/faq.txt`). Answers the "why does it render like that" questions
/// and ends with a sly hint at the hidden `/dig.txt`.
pub fn faq_page() -> String {
    "gopher-cta : FAQ\n\
     ================\n\n\
     Q: Why braille?\n\
     A: Each braille cell packs 2x4 dots, so the whole city fits in plain text\n\
     \x20  at high resolution. The dots are Unicode Braille Patterns\n\
     \x20  (U+2800..U+28FF).\n\n\
     Q: The map is just empty boxes or question marks.\n\
     A: Your terminal font has no Braille Patterns glyphs, or you're not\n\
     \x20  viewing in UTF-8. See /help.txt.\n\n\
     Q: What's the difference between map.txt, map.ansi and atlas.ansi?\n\
     A: map.txt is pure train dots -- no colour, no overlay. map.ansi adds\n\
     \x20  colour and overlays the coastline, river, expressways and place\n\
     \x20  codes. atlas.ansi is the same geography drawn with text characters\n\
     \x20  instead of braille.\n\n\
     Q: I see codes like ESC[38;5;39m instead of colours.\n\
     A: Your client or terminal isn't interpreting ANSI colour. See /help.txt.\n\n\
     Q: What do WIL, NVP, MDW... mean?\n\
     A: Mnemonic place codes; the decode legend prints under the map and atlas.\n\n\
     Q: How fresh is the data, and why \"N of M reporting\"?\n\
     A: The fetcher republishes about every 30 seconds. The CTA feed sometimes\n\
     \x20  reports a train without a usable position; those count in M, not N.\n\n\
     Q: Where do the Dispatch / SITREP / Event panels come from?\n\
     A: The cta-track-grid Worker -- the same brain behind the web tracker at\n\
     \x20  https://tracker.debene.dev/. This gopher site is just another reader.\n\n\
     Q: Is this an official CTA service?\n\
     A: No. Not affiliated with the Chicago Transit Authority.\n\n\
     Q: Is there anything hidden here?\n\
     A: Gophers burrow. Keep digging.\n"
        .to_string()
}

/// Troubleshooting page (`/help.txt`). Actionable fixes for the common
/// rendering problems; the map width is sourced from the live geometry.
pub fn help_page() -> String {
    let g = project::geometry();
    format!(
        "gopher-cta : Troubleshooting\n\
         ============================\n\n\
         Map shows boxes, squares, or question marks\n\
         \x20 Use a UTF-8 terminal and a font that includes Braille Patterns\n\
         \x20 (U+2800..U+28FF). Known-good: DejaVu Sans Mono, Cascadia Code,\n\
         \x20 Menlo, JuliaMono, Iosevka.\n\n\
         Colour map prints ESC[...m escape codes instead of colour\n\
         \x20 Your gopher client isn't passing the text to an ANSI-capable\n\
         \x20 terminal. Try:\n\
         \x20   curl gopher://gopher.debene.dev:70/0/map.ansi | less -R\n\
         \x20 ...or a client that renders ANSI (e.g. Bombadillo) in a real\n\
         \x20 terminal.\n\n\
         Map wraps or looks sheared\n\
         \x20 The map is {wc} columns wide -- give the terminal at least that\n\
         \x20 many columns and turn off line wrapping.\n\n\
         \"0 bytes\" or connection refused on port 70\n\
         \x20 Some networks filter the gopher port. Try another network, or\n\
         \x20 fetch a selector directly:\n\
         \x20   curl gopher://gopher.debene.dev:70/0/map.txt\n\n\
         Menu links point at the wrong host\n\
         \x20 Server-side quirk (geomyidae -h). Fetching selectors directly\n\
         \x20 still works.\n\n\
         Remember: selectors are files\n\
         \x20 Browse .../0/map.txt (type 0 = text) and .../1/ for menus.\n",
        wc = g.wc,
    )
}

/// Hidden easter-egg page (`/dig.txt`). Written into the tree but linked from
/// no menu -- reachable only by guessing the selector (the FAQ hints at it).
/// Cross-references gopherspace elders and the CTA family.
pub fn dig_page() -> String {
    r#"You found the burrow.
=====================

      .--.
     ( oo )   ding ding!
    _/`--'\_
   (__/  \__)   >> now approaching: gopherspace

Neighbours in the burrow
------------------------
  Floodgap   gopher://gopher.floodgap.com    (the gopher hub / Overbite)
  SDF        gopher://sdf.org                 (public-access UNIX since 1987)

The CTA family
--------------
  cta-track-grid   https://tracker.debene.dev/
                   https://github.com/felipedbene/cta-track-grid
                   (the web original; the Worker behind the AI panels --
                    la mama de los pollitos)
  cta-tui          the same data as a terminal UI
  gopher-cta       you are here

                -- dug by a gopher, for gophers --
"#
    .to_string()
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
        let entries = root_menu(&fixture_positions(), false, None);
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
                    ..
                } => Some((display.as_str(), selector.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 5);
        // Run 801 is en route to Howard; its next stop (Loyola) is folded in.
        assert!(links.iter().any(|(d, s)| *s == "/train/801.txt"
            && d.starts_with("Run 801")
            && d.contains("Howard")
            && d.contains("next Loyola")));
        // Run 812 is approaching — the row now names the station, not a bare flag.
        assert!(links
            .iter()
            .any(|(d, s)| *s == "/train/812.txt" && d.contains("approaching ")));
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
    fn map_ansi_colours_braille_plain_does_not() {
        let geo = project::geometry();
        let base = MapBase::build(&geo);
        let plain = map_page(&fixture_positions(), &geo, "CTA 'L'");
        let ansi = map_page_ansi(&base, &fixture_positions(), &geo, "CTA 'L'");
        assert!(!plain.contains('\x1b'), "plain map must be ESC-free");
        assert!(ansi.contains("\x1b[38;5;"), "ansi map must carry SGR codes");
        // still a braille plot underneath
        assert!(ansi
            .chars()
            .any(|c| (0x2800..=0x28FF).contains(&(c as u32))));
    }

    #[test]
    fn map_ansi_overlays_geo_skeleton_plain_does_not() {
        let geo = project::geometry();
        let base = MapBase::build(&geo);
        // Even with NO trains, the ANSI map carries the static geo skeleton:
        // shoreline (cyan) is always drawn, plus the overlay legend note.
        let ansi = map_page_ansi(&base, &Positions::default(), &geo, "CTA 'L'");
        assert!(
            ansi.contains("\x1b[38;5;44m"),
            "water (cyan: coast + river) missing from ANSI map overlay"
        );
        // Place-name labels render in white over the plot, even with no trains.
        assert!(
            ansi.contains("\x1b[38;5;231m"),
            "place-name labels (white) missing from ANSI map overlay"
        );
        assert!(ansi.contains("overlay: water"));
        // Inline place codes + a decode legend (same mnemonic scheme as the atlas).
        assert!(ansi.contains("places (code -> name)"));
        assert!(ansi.contains("MDW  Midway"), "Midway code/legend missing");
        // The plain map stays a pure-train view: no overlay, no geo dots.
        let plain = map_page(&Positions::default(), &geo, "CTA 'L'");
        assert!(
            !plain.contains("overlay:"),
            "plain map must not name an overlay"
        );
        let plain_grid: String = plain
            .lines()
            .skip_while(|l| !l.starts_with("---"))
            .skip(1)
            .take_while(|l| !l.starts_with("---"))
            .collect();
        assert!(
            plain_grid.chars().all(|c| c == '\u{2800}'),
            "plain map grid must be blank braille with no trains/geo"
        );
    }

    #[test]
    fn map_base_rasterizes_geo_into_braille() {
        let geo = project::geometry();
        let base = MapBase::build(&geo);
        // The base canvas has non-blank cells (the shoreline/river/expressways).
        let painted = base
            .canvas
            .render()
            .chars()
            .filter(|&c| (0x2801..=0x28FF).contains(&(c as u32)))
            .count();
        assert!(painted > 0, "geo skeleton left no dots on the base canvas");
        // ...and matching coloured cells in the base palette.
        assert!(base.colors.contains(&GEO_COAST_COLOR)); // coast + river share this cyan
        assert!(base.colors.contains(&GEO_ROAD_COLOR));
        // The static label layer carries the place names (lake + anchors).
        assert!(
            base.labels.iter().any(|l| l.is_some()),
            "no place-name labels were placed on the base"
        );
    }

    #[test]
    fn line_ansi256_distinct_per_line() {
        assert_eq!(line_ansi256("red"), 196);
        assert_ne!(line_ansi256("red"), line_ansi256("blue"));
        assert_eq!(line_ansi256("unknown"), 244);
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
        assert!(text.contains("heading 358 (N)"));
    }

    #[test]
    fn cardinal8_points() {
        assert_eq!(cardinal8(0), "N");
        assert_eq!(cardinal8(358), "N");
        assert_eq!(cardinal8(45), "NE");
        assert_eq!(cardinal8(90), "E");
        assert_eq!(cardinal8(180), "S");
        assert_eq!(cardinal8(270), "W");
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
