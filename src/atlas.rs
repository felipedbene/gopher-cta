//! Char-cell geographic atlas: a second map surface alongside the braille plot.
//!
//! The braille map (`map.txt`) is a monochrome dot field — it shows *where*
//! trains are but not *what* city is under them. This atlas renders the same
//! projected scene on a one-glyph-per-cell character grid, so distinct features
//! read as distinct markers, disambiguated by a legend below the map.
//!
//! Markers are **plain ASCII** for portability across old/non-UTF-8 gopher
//! clients (and to dodge double-width emoji/CJK glyphs that would shear the
//! grid): shoreline `#`, each landmark a letter `A`–`N` keyed by its id, live
//! trains as a 4-way heading arrow `^ > v <` (`o` when the feed reports no
//! heading). The legend maps letters to names.
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
use crate::render::{line_ansi256, line_label, LINE_ORDER};
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

/// On-grid marker for a landmark: a letter keyed by id (1→A, 2→B, …). Ids run
/// 1..=14 so this stays within A..N; the legend maps letters back to names.
fn landmark_marker(id: u32) -> char {
    (b'A' + (id.saturating_sub(1) % 26) as u8) as char
}

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
    pub landmarks: Vec<Landmark>,
}

#[derive(Debug, Deserialize)]
pub struct Expressway {
    pub name: String,
    /// `[lat, lon]` anchors along the route.
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
    pub id: u32,
    pub name: String,
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

/// Integer Bresenham. Calls `plot(col, row)` for every cell on the segment.
fn bresenham(mut x0: i32, mut y0: i32, x1: i32, y1: i32, mut plot: impl FnMut(i32, i32)) {
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
    /// Landmark ids that fall outside the bbox and so never paint (e.g. O'Hare,
    /// just west of the frame). Surfaced in the legend, not silently lost.
    off_map: Vec<u32>,
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

        // Landmarks: one marker cell each; off-bbox ones recorded for the legend.
        let mut off_map = Vec::new();
        for m in &geo.landmarks {
            match project_cell(m.lat, m.lon, &geom) {
                Some((c, r)) => {
                    base.put(c, r, landmark_marker(m.id), PRIO_LANDMARK, LANDMARK_COLOR)
                }
                None => off_map.push(m.id),
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

        Atlas {
            geo,
            geom,
            base,
            off_map,
        }
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
            "{} trains plotted of {} reporting.  {} landmarks ({} off current bbox).\n",
            plotted,
            pos.trains.len(),
            self.geo.landmarks.len(),
            self.off_map.len(),
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

        // Numbered landmark legend (keyed by id; labels never go inline on the
        // grid, where they would collide on a char cell).
        out.push_str("\nLANDMARKS  (marker on the grid -> place)\n");
        for m in &self.geo.landmarks {
            let mark = if self.off_map.contains(&m.id) {
                "  [off map]"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {}  {}{}\n",
                landmark_marker(m.id),
                m.name,
                mark
            ));
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
        let willis = geo.landmarks.iter().find(|m| m.id == 1).unwrap();
        assert_eq!(willis.name, "Willis Tower");
        let ohare = geo.landmarks.iter().find(|m| m.id == 14).unwrap();
        assert_eq!(ohare.category, "transit_hub");
        assert_eq!(geo.expressways.len(), 1);
        assert!(geo.expressways[0].name.contains("90/94"));
        assert_eq!(geo.expressways[0].points.len(), 11);
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
    fn landmark_markers_are_letters_by_id() {
        assert_eq!(landmark_marker(1), 'A');
        assert_eq!(landmark_marker(5), 'E');
        assert_eq!(landmark_marker(14), 'N');
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
        assert!(body.contains('A'), "Willis Tower marker missing from grid");
        assert!(body.contains("0 trains plotted of 0 reporting"));
        // O'Hare sits just west of the bbox and is reported off-map.
        assert!(body.contains("14 landmarks (1 off current bbox)"));
        assert!(body.contains("  N  O'Hare (ORD)  [off map]"));
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
        // Legend present, keyed by the on-grid letter.
        assert!(body.contains("LANDMARKS"));
        assert!(body.contains("  A  Willis Tower"));
        // Per-line counts mirror the braille map.
        assert!(body.contains("legend (trains per line):"));
        assert!(body.contains("Red      5"));
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
