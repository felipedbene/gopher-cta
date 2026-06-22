//! Linear lat/lon -> pixel projection for the CTA service area.
//!
//! Full Mercator is overkill at city scale; we use a plain linear map of the
//! bounding box onto the canvas, with a single longitude aspect correction by
//! `cos(lat_mid)` so the city isn't horizontally stretched. All bbox constants
//! live here and are TUNABLE — widen them to include more of the suburbs, or
//! tighten for a zoomed-in view.

// --- TUNABLE: geographic bounding box of the plotted area (the 'L' system). ---
pub const LAT_MIN: f64 = 41.65;
pub const LAT_MAX: f64 = 42.07;
pub const LON_MIN: f64 = -87.90;
pub const LON_MAX: f64 = -87.52;

// --- TUNABLE: canvas width in pixels. Height is derived to keep aspect. ---
pub const WP: usize = 160; // 80 braille cells wide

/// A pixel canvas geometry derived from the bbox + fixed width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub wp: usize, // pixel width
    pub hp: usize, // pixel height
    pub wc: usize, // cell width  (wp / 2)
    pub hc: usize, // cell height (ceil(hp / 4))
}

/// Compute the canvas geometry from the bbox constants. Height is chosen so a
/// degree of latitude and an (aspect-corrected) degree of longitude map to the
/// same number of pixels, i.e. the plot is not distorted.
pub fn geometry() -> Geometry {
    let lat_mid = (LAT_MIN + LAT_MAX) / 2.0;
    let deg_h = LAT_MAX - LAT_MIN;
    let deg_w_corr = (LON_MAX - LON_MIN) * lat_mid.to_radians().cos();
    let wp = WP;
    let hp = ((wp as f64) * deg_h / deg_w_corr).round() as usize;
    let hc = hp.div_ceil(4);
    Geometry {
        wp,
        hp,
        wc: wp / 2,
        hc,
    }
}

/// Project `(lat, lon)` to integer pixel coords within `geo`. Returns `None`
/// for points outside the bbox (drop-not-clamp, so off-map trains vanish rather
/// than smearing along the edges). Y is flipped so north is up.
pub fn project(lat: f64, lon: f64, geo: &Geometry) -> Option<(usize, usize)> {
    if !(LAT_MIN..=LAT_MAX).contains(&lat) || !(LON_MIN..=LON_MAX).contains(&lon) {
        return None;
    }
    let fx = (lon - LON_MIN) / (LON_MAX - LON_MIN);
    let fy = (lat - LAT_MIN) / (LAT_MAX - LAT_MIN);
    let px = (fx * (geo.wp as f64 - 1.0)).round() as usize;
    let py = ((1.0 - fy) * (geo.hp as f64 - 1.0)).round() as usize;
    Some((px, py))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_sane() {
        let g = geometry();
        assert_eq!(g.wp, 160);
        assert_eq!(g.wc, 80);
        // deg_h 0.42, deg_w_corr ~0.283 -> hp ~237. Allow a small band.
        assert!(g.hp > 220 && g.hp < 250, "hp = {}", g.hp);
        assert_eq!(g.hc, g.hp.div_ceil(4));
    }

    #[test]
    fn corners_map_to_canvas_corners() {
        let g = geometry();
        // SW corner (min lat, min lon): bottom-left -> x=0, y=hp-1
        let (px, py) = project(LAT_MIN, LON_MIN, &g).unwrap();
        assert_eq!(px, 0);
        assert_eq!(py, g.hp - 1);
        // NE corner (max lat, max lon): top-right -> x=wp-1, y=0
        let (px, py) = project(LAT_MAX, LON_MAX, &g).unwrap();
        assert_eq!(px, g.wp - 1);
        assert_eq!(py, 0);
        // NW corner: top-left
        let (px, py) = project(LAT_MAX, LON_MIN, &g).unwrap();
        assert_eq!((px, py), (0, 0));
        // SE corner: bottom-right
        let (px, py) = project(LAT_MIN, LON_MAX, &g).unwrap();
        assert_eq!((px, py), (g.wp - 1, g.hp - 1));
    }

    #[test]
    fn midpoint_lands_near_center() {
        let g = geometry();
        let lat_mid = (LAT_MIN + LAT_MAX) / 2.0;
        let lon_mid = (LON_MIN + LON_MAX) / 2.0;
        let (px, py) = project(lat_mid, lon_mid, &g).unwrap();
        // within 1px of geometric center on each axis
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
