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
//! trains `o`. The legend maps letters to names.
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
//!   shoreline < landmarks < trains.

use serde::Deserialize;

use crate::project::{self, Geometry};
use crate::render::{line_label, LINE_ORDER};
use crate::transit::Positions;

/// The overlay scene, compiled in so the fetcher needs no data file at runtime
/// (mirrors the bundled positions fixture).
const GEO_JSON: &str = include_str!("../chicago_geo.json");

// Z-order priorities. The gaps (2, 4) are where track / station layers would
// slot in if the overlay ever carried that geometry — the feed doesn't today.
const PRIO_SHORE: u8 = 1;
const PRIO_LANDMARK: u8 = 3;
const PRIO_TRAIN: u8 = 5;

/// On-grid ASCII markers. Single-width on every client; no emoji/CJK glyphs to
/// shear the fixed-width grid, and readable on non-UTF-8 gopher clients.
const SHORE_GLYPH: char = '#';
const TRAIN_GLYPH: char = 'o';

/// On-grid marker for a landmark: a letter keyed by id (1→A, 2→B, …). Ids run
/// 1..=14 so this stays within A..N; the legend maps letters back to names.
fn landmark_marker(id: u32) -> char {
    (b'A' + (id.saturating_sub(1) % 26) as u8) as char
}

// ---------- overlay data model (parsed from chicago_geo.json) ----------

#[derive(Debug, Deserialize)]
pub struct GeoData {
    pub meta: Meta,
    pub shoreline: Shoreline,
    pub landmarks: Vec<Landmark>,
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
}

impl CharGrid {
    fn new(wc: usize, hc: usize) -> CharGrid {
        CharGrid {
            wc,
            hc,
            cells: vec![' '; wc * hc],
            prio: vec![0u8; wc * hc],
        }
    }

    /// Paint `glyph` at `(col, row)` if `priority` is at least the cell's current
    /// layer. Out-of-frame cells — including negative coords from off-bbox geo —
    /// are clipped, so callers never need bounds bookkeeping.
    fn put(&mut self, col: i32, row: i32, glyph: char, priority: u8) {
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
                    base.put(c, r, SHORE_GLYPH, PRIO_SHORE)
                });
            }
        }

        // Landmarks: one marker cell each; off-bbox ones recorded for the legend.
        let mut off_map = Vec::new();
        for m in &geo.landmarks {
            match project_cell(m.lat, m.lon, &geom) {
                Some((c, r)) => base.put(c, r, landmark_marker(m.id), PRIO_LANDMARK),
                None => off_map.push(m.id),
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
    /// Trains outside the bbox are logged at debug level, mirroring the braille
    /// map's drop diagnostic.
    pub fn render(&self, pos: &Positions, source_name: &str) -> String {
        let mut grid = self.base.clone();
        let mut plotted = 0usize;
        let mut dropped: Vec<&str> = Vec::new();
        for t in &pos.trains {
            if let Some((c, r)) = project_cell(t.lat, t.lon, &self.geom) {
                grid.put(c, r, TRAIN_GLYPH, PRIO_TRAIN);
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
        out.push_str(&"-".repeat(self.geom.wc.min(78)));
        out.push('\n');
        out.push_str(&grid.render());
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
    }

    #[test]
    fn landmark_markers_are_letters_by_id() {
        assert_eq!(landmark_marker(1), 'A');
        assert_eq!(landmark_marker(5), 'E');
        assert_eq!(landmark_marker(14), 'N');
    }

    // -- char grid layering --

    #[test]
    fn char_grid_respects_priority() {
        let mut g = CharGrid::new(3, 3);
        g.put(1, 1, SHORE_GLYPH, PRIO_SHORE);
        g.put(1, 1, TRAIN_GLYPH, PRIO_TRAIN); // higher: wins
        g.put(1, 1, 'A', PRIO_LANDMARK); // lower than train: ignored
        assert!(g.render().contains(TRAIN_GLYPH));
        assert!(!g.render().contains('A'));
        assert!(!g.render().contains(SHORE_GLYPH));
    }

    #[test]
    fn char_grid_clips_out_of_frame() {
        let mut g = CharGrid::new(2, 2);
        g.put(-1, 0, 'x', PRIO_TRAIN); // negative col
        g.put(0, 5, 'x', PRIO_TRAIN); // row past frame
        g.put(9, 0, 'x', PRIO_TRAIN); // col past frame
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
    fn atlas_plots_fixture_trains_and_legend() {
        let atlas = Atlas::build(project::geometry());
        let body = atlas.render(&fixture_positions(), "CTA 'L'");
        assert!(body.starts_with("CTA 'L' -- geographic atlas"));
        assert!(body.contains('o'), "train marker missing");
        assert!(body.contains("18 trains plotted of 18 reporting"));
        assert!(body.contains("offline fixture"));
        // Legend present, keyed by the on-grid letter.
        assert!(body.contains("LANDMARKS"));
        assert!(body.contains("  A  Willis Tower"));
        // Per-line counts mirror the braille map.
        assert!(body.contains("legend (trains per line):"));
        assert!(body.contains("Red      5"));
    }
}
