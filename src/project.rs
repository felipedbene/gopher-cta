//! Lat/lon -> braille-pixel projection for the CTA service area.
//!
//! The plot must be geographically faithful, which means correcting for two
//! separate distortions:
//!   1. Longitude shrink — at Chicago's latitude a degree of longitude spans far
//!      less ground than a degree of latitude, so we scale by `cos(lat_c)`.
//!   2. Terminal cell aspect — a character cell is taller than it is wide, so a
//!      square patch of ground must occupy fewer rows than columns.
//!
//! Model (km-based):
//! ```text
//!   lon_km_per_deg = LAT_KM_PER_DEG * cos(lat_c)        // (1) longitude shrink
//!   w_km = (lon_max - lon_min) * lon_km_per_deg
//!   h_km = (lat_max - lat_min) * LAT_KM_PER_DEG
//!   H    = round((h_km / w_km) * (W / CELL_ASPECT))     // (2) row budget
//! ```
//! so one column and one row span the same real distance on screen and the city
//! comes out north-up and taller-than-wide.
//!
//! The canvas is Unicode braille: each character cell is a 2x4 dot grid, so the
//! plottable pixel grid is `(2*W) x (4*H)`. With `CELL_ASPECT = 2.0` a braille
//! dot is square on screen, so subdividing each faithful cell into dots stays
//! faithful.

// --- TUNABLE: geographic bounding box of the plotted area (the 'L' system). ---
pub const LAT_MIN: f64 = 41.65;
pub const LAT_MAX: f64 = 42.07;
pub const LON_MIN: f64 = -87.90;
// East edge pushed past the shoreline (~-87.52) into open lake so there is water
// east of the coast — room for the atlas's "LAKE MICHIGAN" label and the coast's
// eastward bulges, instead of the coastline pinned to the frame edge.
pub const LON_MAX: f64 = -87.45;

/// Kilometres per degree of latitude (constant everywhere on Earth).
pub const LAT_KM_PER_DEG: f64 = 111.32;

/// Terminal cell aspect ratio: how many times taller a character cell is than
/// wide. Drives the derived row budget. 2.0 is typical for a monospace terminal
/// and, conveniently, makes a 2x4 braille dot square on screen.
pub const CELL_ASPECT: f64 = 2.0;

// --- TUNABLE: column budget in character cells. Rows are derived from it. ---
// 48 keeps the full-system map close to one terminal page (body ~36 rows) so it
// reads as a map rather than a multi-page scroll; raise it for more resolution.
pub const W: usize = 48;

/// Canvas geometry derived from the bbox, the column budget `W`, and CELL_ASPECT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub wc: usize, // width in character cells  (= W)
    pub hc: usize, // height in character cells (derived, the "H" above)
    pub wp: usize, // width in braille pixels   (= 2 * wc)
    pub hp: usize, // height in braille pixels  (= 4 * hc)
}

/// Longitude km/deg at the bbox's mid-latitude — the longitude-shrink factor.
fn lon_km_per_deg() -> f64 {
    let lat_c = (LAT_MIN + LAT_MAX) / 2.0;
    LAT_KM_PER_DEG * lat_c.to_radians().cos()
}

/// Real-world size of the bbox in kilometres, as `(w_km, h_km)`.
fn bbox_km() -> (f64, f64) {
    let w_km = (LON_MAX - LON_MIN) * lon_km_per_deg();
    let h_km = (LAT_MAX - LAT_MIN) * LAT_KM_PER_DEG;
    (w_km, h_km)
}

/// Compute the canvas geometry. The row budget is chosen so a single column and
/// a single row cover the same real distance on screen:
/// `H = round((h_km / w_km) * (W / CELL_ASPECT))`.
pub fn geometry() -> Geometry {
    let (w_km, h_km) = bbox_km();
    let wc = W;
    let hc = (((h_km / w_km) * (wc as f64 / CELL_ASPECT)).round() as usize).max(1);
    Geometry {
        wc,
        hc,
        wp: wc * 2,
        hp: hc * 4,
    }
}

/// Project `(lat, lon)` to integer braille-pixel coords within `geo`. Returns
/// `None` for points outside the bbox (drop-not-clamp, so off-map trains vanish
/// rather than smearing along the edges). North is up (the row axis is flipped).
///
/// The longitude-shrink factor cancels in the normalized horizontal position, so
/// `x` is effectively linear in `lon` and `y` linear in `lat`; the km model's
/// real effect lives in `geo`'s derived row budget (the aspect ratio).
pub fn project(lat: f64, lon: f64, geo: &Geometry) -> Option<(usize, usize)> {
    if !(LAT_MIN..=LAT_MAX).contains(&lat) || !(LON_MIN..=LON_MAX).contains(&lon) {
        return None;
    }
    let (w_km, h_km) = bbox_km();
    let x_km = (lon - LON_MIN) * lon_km_per_deg();
    let y_km = (lat - LAT_MIN) * LAT_KM_PER_DEG;
    let col = ((x_km / w_km) * (geo.wp as f64 - 1.0)).round() as usize;
    // Flip vertically: larger latitude (north) maps to a smaller row index.
    let row = (geo.hp - 1) - ((y_km / h_km) * (geo.hp as f64 - 1.0)).round() as usize;
    Some((col, row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_sane() {
        let g = geometry();
        assert_eq!(g.wc, W);
        assert_eq!(g.wp, 2 * W);
        // h_km/w_km ~ 1.254, * (48/2) = 30.1 -> 30 rows. Taller than wide.
        assert_eq!(g.hc, 30);
        assert_eq!(g.hp, 4 * g.hc);
        // Faithful aspect: on-screen height (rows * CELL_ASPECT) vs width (cols)
        // should track the real km aspect to within a row.
        let (w_km, h_km) = bbox_km();
        let screen_aspect = (g.hc as f64 * CELL_ASPECT) / g.wc as f64;
        assert!((screen_aspect - h_km / w_km).abs() < 0.05);
        assert!(g.hc * 2 > g.wc, "city should render taller than wide");
    }

    #[test]
    fn corners_map_to_canvas_corners() {
        let g = geometry();
        // SW corner (min lat, min lon): bottom-left -> x=0, y=hp-1
        assert_eq!(project(LAT_MIN, LON_MIN, &g).unwrap(), (0, g.hp - 1));
        // NE corner (max lat, max lon): top-right -> x=wp-1, y=0
        assert_eq!(project(LAT_MAX, LON_MAX, &g).unwrap(), (g.wp - 1, 0));
        // NW corner: top-left
        assert_eq!(project(LAT_MAX, LON_MIN, &g).unwrap(), (0, 0));
        // SE corner: bottom-right
        assert_eq!(project(LAT_MIN, LON_MAX, &g).unwrap(), (g.wp - 1, g.hp - 1));
    }

    #[test]
    fn known_point_maps_to_expected_cell() {
        // A point a quarter of the way up; lon now 0.095/0.45 across the wider bbox.
        // lon: -87.805 is 0.095/0.45 = 0.211 across; lat: 41.755 is 0.105/0.42 = 0.25 up.
        let g = geometry();
        let (col, row) = project(41.755, -87.805, &g).unwrap();
        // col = round(0.211 * (wp-1)) = round(0.211*95) = 20
        assert_eq!(col, 20);
        // row = (hp-1) - round(0.25*(hp-1)) = 119 - round(29.75) = 119 - 30 = 89
        assert_eq!(row, 89);
    }

    #[test]
    fn midpoint_lands_near_center() {
        let g = geometry();
        let lat_mid = (LAT_MIN + LAT_MAX) / 2.0;
        let lon_mid = (LON_MIN + LON_MAX) / 2.0;
        let (px, py) = project(lat_mid, lon_mid, &g).unwrap();
        assert!((px as i64 - (g.wp as i64 - 1) / 2).abs() <= 1, "px={px}");
        assert!((py as i64 - (g.hp as i64 - 1) / 2).abs() <= 1, "py={py}");
    }

    #[test]
    fn out_of_bbox_is_dropped() {
        let g = geometry();
        assert!(project(40.0, -87.6, &g).is_none()); // too far south
        assert!(project(41.8, -88.5, &g).is_none()); // too far west
        assert!(project(42.5, -87.6, &g).is_none()); // too far north
        assert!(project(41.8, -87.0, &g).is_none()); // too far east
    }

    #[test]
    fn the_loop_is_roughly_centered() {
        // Chicago Loop ~ (41.88, -87.63). Should land in the interior.
        let g = geometry();
        let (px, py) = project(41.88, -87.63, &g).unwrap();
        assert!(px > 20 && px < (g.wp - 20));
        assert!(py > 20 && py < (g.hp - 20));
    }
}
