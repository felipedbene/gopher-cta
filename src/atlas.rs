//! Char-cell geographic atlas: a second map surface alongside the braille plot.
//!
//! The braille map (`map.txt`) is a monochrome dot field — it shows *where*
//! trains are but not *what* city is under them. This atlas renders the same
//! projected scene on a one-glyph-per-cell character grid, so distinct features
//! read as distinct markers, disambiguated by a legend below the map.
//!
//! Markers are **plain ASCII** for portability across old/non-UTF-8 gopher
//! clients (and to dodge double-width emoji/CJK glyphs that would shear the
//! grid): shoreline `#`, each landmark a unique mnemonic letter (W = Willis,
//! F = Field Museum…), live trains as a 4-way heading arrow `^ > v <` (`o` when
//! the feed reports no heading). The legend lists only the markers actually
//! visible on the grid (not covered by a train or lost to a collision).
//!
//! Pixel-locked to the trains: cells come from the SAME [`project::project`] the
//! braille map uses, collapsed from braille-pixel to char-cell `(px/2, py/4)`.
//! There is no second projection.
//!
//! The static scene (shoreline + landmarks) is rasterized ONCE into a base grid
//! at startup ([`Atlas::build`]); each publish clones that base and paints live
//! trains on top, so geo never re-rasterizes.
//!
//! Z-order (painter's algorithm, higher priority wins):
//!   shoreline < expressways < landmarks < trains.
//! Expressways are drawn with a slope-aware glyph (`= | / \`).

use serde::Deserialize;

use crate::project::{self, Geometry};
use crate::render::{line_ansi256, line_label, Entry, ItemKind, LINE_ORDER};
use crate::transit::Positions;

/// The overlay scene, compiled in so the fetcher needs no data file at runtime
/// (mirrors the bundled positions fixture).
const GEO_JSON: &str = include_str!("../chicago_geo.json");

// Z-order priorities. The gaps (2, 4) are where track / station layers would
// slot in if the overlay ever carried that geometry — the feed doesn't today.
// The water label sits above everything so a stray near-shore train can't break
// it (it lives in open water east of the coast, so nothing else is there anyway).
const PRIO_SHORE: u8 = 1;
const PRIO_EXPRESSWAY: u8 = 2; // the reserved "track"-layer slot
const PRIO_LANDMARK: u8 = 3;
const PRIO_TRAIN: u8 = 5;
const PRIO_LABEL: u8 = 7;

/// Identifies the body of water for any viewer, written down the open lake east
/// of the coastline (the bbox is widened east to make room).
const LAKE_LABEL: &str = "LAKE MICHIGAN";

// ANSI 256-colour codes for the static layers in the `.ansi` variant (trains
// take their CTA line colour). `0` = uncoloured.
const WATER_COLOR: u8 = 44; // cyan: shoreline + lake label
const LANDMARK_COLOR: u8 = 250; // light grey, so coloured trains pop
const EXPRESSWAY_COLOR: u8 = 240; // dark grey: roads recede behind coloured trains

/// Expressway-segment glyph by slope (in cells): `=` horizontal, `|` vertical,
/// `/` SW-NE, `\` NW-SE. Screen rows increase downward, so same-sign dx/dy is `\`.
fn road_glyph(dx: i32, dy: i32) -> char {
    let (adx, ady) = (dx.abs(), dy.abs());
    if adx >= 2 * ady {
        '='
    } else if ady >= 2 * adx {
        '|'
    } else if (dx > 0) == (dy > 0) {
        '\\'
    } else {
        '/'
    }
}

/// On-grid ASCII markers. Single-width on every client; no emoji/CJK glyphs to
/// shear the fixed-width grid, and readable on non-UTF-8 gopher clients.
const SHORE_GLYPH: char = '#';
const TRAIN_GLYPH: char = 'o';

/// On-grid marker for a train: a 4-way ASCII heading arrow (`^ > v <`), so the
/// direction of travel reads on any client. Diagonals round to the nearest
/// cardinal; `o` when the feed reports no heading.
fn heading_glyph(heading: Option<u16>) -> char {
    match heading {
        None => TRAIN_GLYPH,
        Some(deg) => match deg % 360 {
            315..=359 | 0..=44 => '^', // N
            45..=134 => '>',           // E
            135..=224 => 'v',          // S
            _ => '<',                  // W (225..=314)
        },
    }
}

// ---------- overlay data model (parsed from chicago_geo.json) ----------

#[derive(Debug, Deserialize)]
pub struct GeoData {
    pub meta: Meta,
    pub shoreline: Shoreline,
    #[serde(default)]
    pub expressways: Vec<Expressway>,
    /// Watercourses (the Chicago River + branches). Drawn by the braille map's
    /// ANSI overlay; the char-cell atlas doesn't render them today.
    #[serde(default)]
    pub rivers: Vec<River>,
    pub landmarks: Vec<Landmark>,
}

#[derive(Debug, Deserialize)]
pub struct Expressway {
    pub name: String,
    /// `[lat, lon]` anchors along the route.
    pub points: Vec<[f64; 2]>,
}

#[derive(Debug, Deserialize)]
pub struct River {
    /// `[lat, lon]` anchors along the watercourse. (The JSON `name` is dropped by
    /// serde — it's inline documentation; the map draws rivers without a legend.)
    pub points: Vec<[f64; 2]>,
}

#[derive(Debug, Deserialize)]
pub struct Meta {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Shoreline {
    pub name: String,
    /// `[lat, lon]` anchors, ordered north -> south.
    pub points: Vec<[f64; 2]>,
}

#[derive(Debug, Deserialize)]
pub struct Landmark {
    pub name: String,
    /// On-grid marker: a unique mnemonic letter (W = Willis, F = Field Museum…),
    /// chosen in the data so it relates to the name. The legend maps it back.
    pub marker: char,
    pub lat: f64,
    pub lon: f64,
    /// Reserved for the per-landmark detail page (a deferred commit); parsed now
    /// so the overlay model stays complete.
    #[allow(dead_code)]
    pub category: String,
}

impl GeoData {
    /// Parse the compiled-in overlay. Panics loudly on malformed JSON: the data
    /// ships inside the binary, so a parse failure is a build-time bug, not a
    /// runtime condition to paper over.
    pub fn load() -> GeoData {
        serde_json::from_str(GEO_JSON).expect("chicago_geo.json (compiled-in) is malformed")
    }
}

// ---------- char-cell canvas ----------

/// A one-glyph-per-cell grid with painter's-algorithm layering: a cell keeps the
/// glyph of the highest-priority layer written to it.
#[derive(Clone)]
struct CharGrid {
    wc: usize,
    hc: usize,
    cells: Vec<char>,
    prio: Vec<u8>,
    color: Vec<u8>,
}

impl CharGrid {
    fn new(wc: usize, hc: usize) -> CharGrid {
        CharGrid {
            wc,
            hc,
            cells: vec![' '; wc * hc],
            prio: vec![0u8; wc * hc],
            color: vec![0u8; wc * hc],
        }
    }

    /// Paint `glyph` (with ANSI 256-colour `color`, `0` = none) at `(col, row)`
    /// if `priority` is at least the cell's current layer. Out-of-frame cells —
    /// including negative coords from off-bbox geo — are clipped, so callers
    /// never need bounds bookkeeping.
    fn put(&mut self, col: i32, row: i32, glyph: char, priority: u8, color: u8) {
        if col < 0 || row < 0 {
            return;
        }
        let (c, r) = (col as usize, row as usize);
        if c >= self.wc || r >= self.hc {
            return;
        }
        let i = r * self.wc + c;
        if priority >= self.prio[i] {
            self.cells[i] = glyph;
            self.prio[i] = priority;
            self.color[i] = color;
        }
    }

    /// The glyph currently at `(col, row)`, or `None` if out of frame. Used to
    /// tell whether a landmark's marker survived on the rendered grid.
    fn cell_at(&self, col: i32, row: i32) -> Option<char> {
        if col < 0 || row < 0 {
            return None;
        }
        let (c, r) = (col as usize, row as usize);
        if c >= self.wc || r >= self.hc {
            return None;
        }
        Some(self.cells[r * self.wc + c])
    }

    /// One line per row with trailing blanks trimmed, rows joined by `\n`.
    fn render(&self) -> String {
        let mut out = String::with_capacity(self.hc * (self.wc + 1));
        for r in 0..self.hc {
            let row: String = self.cells[r * self.wc..(r + 1) * self.wc].iter().collect();
            out.push_str(row.trim_end());
            if r + 1 < self.hc {
                out.push('\n');
            }
        }
        out
    }

    /// As [`render`], but each coloured cell is wrapped in an ANSI 256-colour SGR
    /// (cells with colour `0` stay plain). For the atlas `.ansi` variant.
    fn render_ansi(&self) -> String {
        let mut out = String::with_capacity(self.hc * (self.wc + 1) * 2);
        for r in 0..self.hc {
            let row = &self.cells[r * self.wc..(r + 1) * self.wc];
            // Trim trailing blanks: emit up to the last non-space cell.
            let last = row.iter().rposition(|&c| c != ' ');
            if let Some(last) = last {
                for c in 0..=last {
                    let i = r * self.wc + c;
                    let ch = self.cells[i];
                    let col = self.color[i];
                    if ch != ' ' && col != 0 {
                        out.push_str(&format!("\x1b[38;5;{col}m{ch}\x1b[0m"));
                    } else {
                        out.push(ch);
                    }
                }
            }
            if r + 1 < self.hc {
                out.push('\n');
            }
        }
        out
    }
}

/// Project `(lat, lon)` to a char cell using the SAME map as the trains: the
/// braille pixel from [`project::project`], collapsed to the cell that contains
/// it (`px/2`, `py/4`). `None` for points outside the bbox (dropped, exactly like
/// an off-map train).
fn project_cell(lat: f64, lon: f64, geo: &Geometry) -> Option<(i32, i32)> {
    project::project(lat, lon, geo).map(|(px, py)| ((px / 2) as i32, (py / 4) as i32))
}

/// Integer Bresenham. Calls `plot(x, y)` for every point on the segment. Works in
/// any integer grid (atlas char cells or, reused by the braille map, raw pixels).
pub(crate) fn bresenham(
    mut x0: i32,
    mut y0: i32,
    x1: i32,
    y1: i32,
    mut plot: impl FnMut(i32, i32),
) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        plot(x0, y0);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

// ---------- the atlas ----------

/// The atlas scene: the parsed overlay, the canvas geometry, and a base grid with
/// the static layers (shoreline + landmarks) already rasterized. Built once;
/// [`Atlas::render`] clones the base per publish and paints trains on top.
pub struct Atlas {
    geo: GeoData,
    geom: Geometry,
    base: CharGrid,
}

impl Atlas {
    /// Load the overlay and rasterize the static layers into the base grid. Call
    /// once at startup; clone-and-paint per publish via [`Atlas::render`].
    pub fn build(geom: Geometry) -> Atlas {
        let geo = GeoData::load();
        let mut base = CharGrid::new(geom.wc, geom.hc);

        // Shoreline: Bresenham each segment whose BOTH endpoints project. A
        // segment with an off-bbox endpoint is skipped (its neighbour still
        // draws), so the coast clips cleanly at the frame instead of streaking.
        for seg in geo.shoreline.points.windows(2) {
            if let (Some((c0, r0)), Some((c1, r1))) = (
                project_cell(seg[0][0], seg[0][1], &geom),
                project_cell(seg[1][0], seg[1][1], &geom),
            ) {
                bresenham(c0, r0, c1, r1, |c, r| {
                    base.put(c, r, SHORE_GLYPH, PRIO_SHORE, WATER_COLOR)
                });
            }
        }

        // Expressways (the reserved "track" layer): each segment drawn with a
        // glyph that follows its slope, so the road reads as a road.
        for road in &geo.expressways {
            for seg in road.points.windows(2) {
                if let (Some((c0, r0)), Some((c1, r1))) = (
                    project_cell(seg[0][0], seg[0][1], &geom),
                    project_cell(seg[1][0], seg[1][1], &geom),
                ) {
                    let g = road_glyph(c1 - c0, r1 - r0);
                    bresenham(c0, r0, c1, r1, |c, r| {
                        base.put(c, r, g, PRIO_EXPRESSWAY, EXPRESSWAY_COLOR)
                    });
                }
            }
        }

        // Landmarks: one marker cell each (its mnemonic letter).
        for m in &geo.landmarks {
            if let Some((c, r)) = project_cell(m.lat, m.lon, &geom) {
                base.put(c, r, m.marker, PRIO_LANDMARK, LANDMARK_COLOR);
            }
        }

        // "LAKE MICHIGAN" down the far-east water column, vertically centered.
        // With the lake band tightened, the frame edge sits near the coast, so
        // the label reads as water (east of the coast) without floating.
        let col = geom.wc as i32 - 2;
        let start = (geom.hc as i32 - LAKE_LABEL.len() as i32) / 2;
        for (i, ch) in LAKE_LABEL.chars().enumerate() {
            if ch != ' ' {
                base.put(col, start + i as i32, ch, PRIO_LABEL, WATER_COLOR);
            }
        }

        Atlas { geo, geom, base }
    }

    /// Render the atlas page (plain text): clone the static base, paint live
    /// trains, then append the numbered landmark legend and per-line counts.
    pub fn render(&self, pos: &Positions, source_name: &str) -> String {
        self.build_page(pos, source_name, false)
    }

    /// As [`render`], but ANSI-coloured (shoreline/label cyan, landmarks grey,
    /// trains by CTA line). For the `atlas.ansi` selector; strict clients use
    /// [`render`].
    pub fn render_ansi(&self, pos: &Positions, source_name: &str) -> String {
        self.build_page(pos, source_name, true)
    }

    /// Clone the static base, paint live trains, and assemble the page. Trains
    /// outside the bbox are logged at debug level, mirroring the braille map's
    /// drop diagnostic. The grid body is colourised when `ansi`.
    fn build_page(&self, pos: &Positions, source_name: &str, ansi: bool) -> String {
        let mut grid = self.base.clone();
        let mut plotted = 0usize;
        let mut dropped: Vec<&str> = Vec::new();
        for t in &pos.trains {
            if let Some((c, r)) = project_cell(t.lat, t.lon, &self.geom) {
                grid.put(
                    c,
                    r,
                    heading_glyph(t.heading),
                    PRIO_TRAIN,
                    line_ansi256(&t.line),
                );
                plotted += 1;
            } else {
                dropped.push(&t.run);
            }
        }
        if !dropped.is_empty() {
            eprintln!(
                "[debug][atlas] dropped {} train(s) off-bbox: {}",
                dropped.len(),
                dropped.join(", ")
            );
        }

        // Only landmarks whose marker still shows on the rendered grid — i.e.
        // not off-bbox, not lost to a same-cell collision, and not covered by a
        // train — so the legend lists exactly what's actually on the map.
        let visible: Vec<&Landmark> = self
            .geo
            .landmarks
            .iter()
            .filter(|m| {
                project_cell(m.lat, m.lon, &self.geom)
                    .and_then(|(c, r)| grid.cell_at(c, r))
                    .is_some_and(|ch| ch == m.marker)
            })
            .collect();

        let mut out = String::new();
        out.push_str("CTA 'L' -- geographic atlas\n");
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
        out.push_str(&format!(
            "overlay: {} ({})\n",
            self.geo.meta.name, self.geo.shoreline.name
        ));
        out.push_str("view: north up; west = city, east = Lake Michigan\n");
        out.push_str(&"-".repeat(self.geom.wc.min(78)));
        out.push('\n');
        out.push_str(&if ansi {
            grid.render_ansi()
        } else {
            grid.render()
        });
        out.push('\n');
        out.push_str(&"-".repeat(self.geom.wc.min(78)));
        out.push('\n');
        out.push_str(&format!(
            "{} trains plotted of {} reporting.  {} of {} landmarks shown.\n",
            plotted,
            pos.trains.len(),
            visible.len(),
            self.geo.landmarks.len(),
        ));
        out.push_str(&format!(
            "bbox lat[{}..{}] lon[{}..{}]\n",
            project::LAT_MIN,
            project::LAT_MAX,
            project::LON_MIN,
            project::LON_MAX,
        ));
        if !self.geo.expressways.is_empty() {
            let names: Vec<&str> = self
                .geo
                .expressways
                .iter()
                .map(|r| r.name.as_str())
                .collect();
            out.push_str(&format!("expressways  (= | / \\):  {}\n", names.join("; ")));
        }

        // Landmark legend — only the markers actually visible on the grid above,
        // keyed by the on-grid letter (labels never go inline, where they'd
        // collide on a char cell).
        out.push_str("\nLANDMARKS  (marker -> place)\n");
        for m in &visible {
            out.push_str(&format!("  {}  {}\n", m.marker, m.name));
        }

        // Per-line train counts (mirrors the braille map's legend).
        out.push_str("\nlegend (trains per line):\n");
        for &key in LINE_ORDER {
            let count = pos.trains.iter().filter(|t| t.line == key).count();
            if count > 0 {
                out.push_str(&format!("  {:<8} {}\n", line_label(key), count));
            }
        }
        out
    }

    /// Type-1 menu of every landmark, each drilling into its detail page. The
    /// menu lists all of them (not just the visible-on-grid subset) so a covered
    /// or off-bbox landmark is still reachable.
    pub fn landmarks_menu(&self) -> Vec<Entry> {
        let mut e = vec![
            Entry::Info("Chicago landmarks".to_string()),
            Entry::Info("=".repeat(40)),
            Entry::Info("Marker letters key the /atlas.txt grid.".to_string()),
            Entry::Info(String::new()),
        ];
        for m in &self.geo.landmarks {
            e.push(Entry::Link {
                kind: ItemKind::Text,
                display: format!("{}  {} ({})", m.marker, m.name, m.category),
                selector: landmark_selector(m.marker),
            });
        }
        e
    }

    /// (marker, detail-page text) for every landmark, for writing the tree.
    pub fn landmark_pages(&self) -> Vec<(char, String)> {
        self.geo
            .landmarks
            .iter()
            .map(|m| (m.marker, self.landmark_page(m.marker)))
            .collect()
    }

    /// The detail page (type-0 text) for one landmark by its marker. An unknown
    /// marker yields a clean notice.
    pub fn landmark_page(&self, marker: char) -> String {
        let Some(m) = self.geo.landmarks.iter().find(|m| m.marker == marker) else {
            return format!(
                "Unknown landmark '{marker}'\n{}\n\nNo landmark uses this marker. \
                 Head back to the landmarks menu.\n",
                "=".repeat(40)
            );
        };
        let mut out = String::new();
        out.push_str(&format!("{}\n", m.name));
        out.push_str(&"=".repeat(40));
        out.push_str("\n\n");
        out.push_str(&format!("category:     {}\n", m.category));
        out.push_str(&format!("atlas marker: {}\n", m.marker));
        out.push_str(&format!("position:     {:.5}, {:.5}\n", m.lat, m.lon));
        out.push_str(&format!(
            "\nPlotted on the geographic atlas (/atlas.txt) at marker '{}'.\n",
            m.marker
        ));
        out.push_str(
            "The CTA positions feed carries no station geometry, so a nearest-'L'\n\
             stop isn't available here.\n",
        );
        out
    }
}

/// Selector (served path) for a landmark's detail page, keyed by its marker.
fn landmark_selector(marker: char) -> String {
    format!("/landmark/{marker}.txt")
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

    // -- overlay data model --

    #[test]
    fn geo_json_parses() {
        let geo = GeoData::load();
        assert_eq!(geo.shoreline.points.len(), 17);
        assert_eq!(geo.landmarks.len(), 14);
        // Mnemonic markers come from the data and are unique.
        let willis = geo
            .landmarks
            .iter()
            .find(|m| m.name == "Willis Tower")
            .unwrap();
        assert_eq!(willis.marker, 'W');
        let field = geo
            .landmarks
            .iter()
            .find(|m| m.name == "Field Museum")
            .unwrap();
        assert_eq!(field.marker, 'F');
        let markers: std::collections::HashSet<char> =
            geo.landmarks.iter().map(|m| m.marker).collect();
        assert_eq!(markers.len(), 14, "landmark markers must be unique");
        // The Kennedy/Dan Ryan stays first; the main radials were added after it.
        assert_eq!(geo.expressways.len(), 4);
        assert!(geo.expressways[0].name.contains("90/94"));
        assert_eq!(geo.expressways[0].points.len(), 11);
        let road_names: Vec<&str> = geo.expressways.iter().map(|r| r.name.as_str()).collect();
        assert!(road_names.iter().any(|n| n.contains("Eisenhower")));
        assert!(road_names.iter().any(|n| n.contains("Stevenson")));
        assert!(road_names.iter().any(|n| n.contains("Lake Shore")));
        // The Chicago River (main stem + two branches) is parsed for the map.
        assert_eq!(geo.rivers.len(), 3);
        assert!(geo.rivers.iter().all(|r| r.points.len() >= 2));
    }

    #[test]
    fn road_glyphs_follow_slope() {
        assert_eq!(road_glyph(10, 0), '='); // E-W
        assert_eq!(road_glyph(0, 10), '|'); // N-S
        assert_eq!(road_glyph(5, 5), '\\'); // down-right (NW-SE)
        assert_eq!(road_glyph(5, -5), '/'); // up-right (SW-NE)
        assert_eq!(road_glyph(-4, -4), '\\'); // up-left
    }

    #[test]
    fn heading_glyphs_are_cardinal() {
        assert_eq!(heading_glyph(Some(0)), '^');
        assert_eq!(heading_glyph(Some(358)), '^');
        assert_eq!(heading_glyph(Some(90)), '>');
        assert_eq!(heading_glyph(Some(180)), 'v');
        assert_eq!(heading_glyph(Some(270)), '<');
        assert_eq!(heading_glyph(None), 'o');
    }

    // -- char grid layering --

    #[test]
    fn char_grid_respects_priority() {
        let mut g = CharGrid::new(3, 3);
        g.put(1, 1, SHORE_GLYPH, PRIO_SHORE, WATER_COLOR);
        g.put(1, 1, TRAIN_GLYPH, PRIO_TRAIN, 196); // higher: wins
        g.put(1, 1, 'A', PRIO_LANDMARK, LANDMARK_COLOR); // lower than train: ignored
        assert!(g.render().contains(TRAIN_GLYPH));
        assert!(!g.render().contains('A'));
        assert!(!g.render().contains(SHORE_GLYPH));
    }

    #[test]
    fn char_grid_clips_out_of_frame() {
        let mut g = CharGrid::new(2, 2);
        g.put(-1, 0, 'x', PRIO_TRAIN, 0); // negative col
        g.put(0, 5, 'x', PRIO_TRAIN, 0); // row past frame
        g.put(9, 0, 'x', PRIO_TRAIN, 0); // col past frame
                                         // Nothing painted: every row trims to empty (just the inter-row newline).
        assert!(!g.render().contains('x'));
        assert_eq!(g.render(), "\n");
    }

    // -- projection reuse --

    #[test]
    fn project_cell_collapses_the_train_projection() {
        let geom = project::geometry();
        // The same call the trains use, collapsed to a char cell.
        let (px, py) = project::project(41.88, -87.63, &geom).unwrap();
        assert_eq!(
            project_cell(41.88, -87.63, &geom),
            Some(((px / 2) as i32, (py / 4) as i32))
        );
    }

    // -- atlas rendering --

    #[test]
    fn atlas_base_shows_shoreline_and_landmarks_without_trains() {
        let atlas = Atlas::build(project::geometry());
        // No trains, so nothing overwrites the static layers.
        let body = atlas.render(&Positions::default(), "CTA 'L'");
        assert!(body.contains('#'), "shoreline marker missing from grid");
        assert!(body.contains("0 trains plotted of 0 reporting"));
        assert!(body.contains("of 14 landmarks shown"));
        // A marker with an isolated cell (Midway, far SW) is listed by mnemonic.
        assert!(body.contains("  D  Midway (MDW)"));
        // The off-bbox O'Hare is filtered out, not floated in the legend.
        assert!(!body.contains("O'Hare"));
    }

    #[test]
    fn atlas_labels_the_lake() {
        let geom = project::geometry();
        let atlas = Atlas::build(geom);
        let body = atlas.render(&Positions::default(), "CTA 'L'");
        assert!(body.contains("north up; west = city, east = Lake Michigan"));
        // The label is painted vertically at column wc-2; read that column down
        // the grid body (between the dashed rules) — it spells the lake's name.
        let column: String = body
            .lines()
            .skip_while(|l| !l.starts_with("---"))
            .skip(1)
            .take_while(|l| !l.starts_with("---"))
            .filter_map(|l| l.chars().nth(geom.wc - 2))
            .filter(|c| *c != ' ')
            .collect();
        // The label reads top-to-bottom; the south coast can also touch this
        // column lower down, so assert containment rather than exact equality.
        assert!(column.contains("LAKEMICHIGAN"), "got column {column:?}");
    }

    #[test]
    fn atlas_plots_fixture_trains_and_legend() {
        let atlas = Atlas::build(project::geometry());
        let body = atlas.render(&fixture_positions(), "CTA 'L'");
        assert!(body.starts_with("CTA 'L' -- geographic atlas"));
        assert!(
            ['^', '>', 'v', '<', 'o'].iter().any(|&c| body.contains(c)),
            "no train heading marker on the grid"
        );
        assert!(body.contains("18 trains plotted of 18 reporting"));
        assert!(body.contains("offline fixture"));
        // Legend present, listing only the visible markers.
        assert!(body.contains("LANDMARKS"));
        assert!(body.contains("of 14 landmarks shown"));
        // Per-line counts mirror the braille map.
        assert!(body.contains("legend (trains per line):"));
        assert!(body.contains("Red      5"));
    }

    #[test]
    fn landmarks_menu_links_every_landmark() {
        let atlas = Atlas::build(project::geometry());
        let menu = atlas.landmarks_menu();
        // All 14 landmarks present as type-0 links to /landmark/<marker>.txt.
        let links: Vec<_> = menu
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
        assert_eq!(links.len(), 14);
        assert!(links
            .iter()
            .any(|(d, s)| d.contains("Willis Tower") && *s == "/landmark/W.txt"));
        assert!(links.iter().any(|(_, s)| *s == "/landmark/F.txt")); // Field Museum
    }

    #[test]
    fn landmark_page_known_and_unknown() {
        let atlas = Atlas::build(project::geometry());
        let willis = atlas.landmark_page('W');
        assert!(willis.starts_with("Willis Tower"));
        assert!(willis.contains("category:     skyline"));
        assert!(willis.contains("atlas marker: W"));
        assert!(willis.contains("position:     41.87890, -87.63590"));
        // Unknown marker -> clean notice, no panic.
        let bogus = atlas.landmark_page('Z');
        assert!(bogus.starts_with("Unknown landmark 'Z'"));
    }

    #[test]
    fn atlas_draws_expressways() {
        let atlas = Atlas::build(project::geometry());
        let body = atlas.render(&Positions::default(), "CTA 'L'");
        // A road glyph appears in the grid body (between the dashed rules).
        let grid: String = body
            .lines()
            .skip_while(|l| !l.starts_with("---"))
            .skip(1)
            .take_while(|l| !l.starts_with("---"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            ['=', '|', '/', '\\'].iter().any(|&c| grid.contains(c)),
            "no expressway glyph in the grid"
        );
        // The legend names the route.
        assert!(body.contains("expressways"));
        assert!(body.contains("Kennedy + Dan Ryan"));
    }

    #[test]
    fn atlas_ansi_colours_grid_plain_does_not() {
        let atlas = Atlas::build(project::geometry());
        let plain = atlas.render(&fixture_positions(), "CTA 'L'");
        let ansi = atlas.render_ansi(&fixture_positions(), "CTA 'L'");
        assert!(!plain.contains('\x1b'), "plain atlas must be ESC-free");
        // Shoreline is always drawn, so its water colour is always present.
        assert!(ansi.contains("\x1b[38;5;44m"), "shoreline colour missing");
        // Same plain legend in both.
        assert!(ansi.contains("LANDMARKS"));
        assert!(ansi.contains("18 trains plotted of 18 reporting"));
    }
}
