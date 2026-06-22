//! Unicode-braille sub-character canvas.
//!
//! Each glyph (base U+2800) is a 2-wide x 4-tall dot grid (8 dots). Setting a
//! pixel ORs its dot's bit into the cell's byte; `render()` walks the grid and
//! emits one braille char per cell, rows separated by `\n`.
//!
//! Dot -> bit map for cell-local `(col, row)`, col in {0,1}, row in {0,1,2,3}:
//! ```text
//!   (0,0)=0x01  (1,0)=0x08
//!   (0,1)=0x02  (1,1)=0x10
//!   (0,2)=0x04  (1,2)=0x20
//!   (0,3)=0x40  (1,3)=0x80
//! ```

/// Base braille codepoint; the 8 dot bits are added on top of this.
const BRAILLE_BASE: u32 = 0x2800;

/// Bit value for a pixel at cell-local `(col, row)`.
fn dot_bit(col: usize, row: usize) -> u8 {
    match (col, row) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (0, 3) => 0x40,
        (1, 3) => 0x80,
        _ => 0, // out of the 2x4 cell: contributes nothing
    }
}

/// A grid of `wc` x `hc` braille cells, i.e. `2*wc` x `4*hc` plottable pixels.
pub struct Canvas {
    wc: usize,
    hc: usize,
    cells: Vec<u8>, // row-major, wc per row
}

impl Canvas {
    pub fn new(wc: usize, hc: usize) -> Self {
        Canvas {
            wc,
            hc,
            cells: vec![0u8; wc * hc],
        }
    }

    /// Pixel width (2 per cell).
    pub fn width_px(&self) -> usize {
        self.wc * 2
    }

    /// Pixel height (4 per cell).
    pub fn height_px(&self) -> usize {
        self.hc * 4
    }

    /// Set the pixel at `(px, py)`. Out-of-bounds pixels are silently ignored
    /// so callers can clamp-or-drop without bounds bookkeeping.
    pub fn set(&mut self, px: usize, py: usize) {
        if px >= self.width_px() || py >= self.height_px() {
            return;
        }
        let (cx, cy) = (px / 2, py / 4);
        let (col, row) = (px % 2, py % 4);
        self.cells[cy * self.wc + cx] |= dot_bit(col, row);
    }

    /// Whether any dot is set in the given cell (used by tests / overlays).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cell(&self, cx: usize, cy: usize) -> u8 {
        self.cells[cy * self.wc + cx]
    }

    /// Render to a string: one braille glyph per cell, rows joined by `\n`.
    /// A fully-empty cell renders as the blank braille pattern U+2800 (not a
    /// space) so the canvas keeps a fixed visual width in monospaced clients.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.hc * (self.wc + 1));
        for cy in 0..self.hc {
            for cx in 0..self.wc {
                let bits = self.cells[cy * self.wc + cx] as u32;
                // BRAILLE_BASE + bits is always a valid braille codepoint.
                out.push(char::from_u32(BRAILLE_BASE + bits).unwrap_or('?'));
            }
            if cy + 1 < self.hc {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_top_left_dot() {
        // only (0,0) -> 0x2801 -> ⠁
        let mut c = Canvas::new(1, 1);
        c.set(0, 0);
        assert_eq!(c.cell(0, 0), 0x01);
        assert_eq!(c.render(), "⠁");
        assert_eq!(c.render().chars().next().unwrap() as u32, 0x2801);
    }

    #[test]
    fn all_eight_dots() {
        // all 8 dots -> 0x28FF -> ⣿
        let mut c = Canvas::new(1, 1);
        for px in 0..2 {
            for py in 0..4 {
                c.set(px, py);
            }
        }
        assert_eq!(c.cell(0, 0), 0xFF);
        assert_eq!(c.render(), "⣿");
        assert_eq!(c.render().chars().next().unwrap() as u32, 0x28FF);
    }

    #[test]
    fn top_left_and_bottom_right() {
        // (0,0)+(1,3) -> 0x01|0x80 = 0x81 -> 0x2881 -> ⢁
        let mut c = Canvas::new(1, 1);
        c.set(0, 0);
        c.set(1, 3);
        assert_eq!(c.cell(0, 0), 0x81);
        assert_eq!(c.render(), "⢁");
        assert_eq!(c.render().chars().next().unwrap() as u32, 0x2881);
    }

    #[test]
    fn dot_bits_match_spec() {
        assert_eq!(dot_bit(0, 0), 0x01);
        assert_eq!(dot_bit(0, 1), 0x02);
        assert_eq!(dot_bit(0, 2), 0x04);
        assert_eq!(dot_bit(1, 0), 0x08);
        assert_eq!(dot_bit(1, 1), 0x10);
        assert_eq!(dot_bit(1, 2), 0x20);
        assert_eq!(dot_bit(0, 3), 0x40);
        assert_eq!(dot_bit(1, 3), 0x80);
    }

    #[test]
    fn dimensions_and_blank_render() {
        let c = Canvas::new(80, 10);
        assert_eq!(c.width_px(), 160);
        assert_eq!(c.height_px(), 40);
        let rendered = c.render();
        // 10 rows -> 9 newlines, each row 80 blank-braille glyphs.
        assert_eq!(rendered.matches('\n').count(), 9);
        assert_eq!(rendered.lines().count(), 10);
        assert!(rendered.chars().next().unwrap() as u32 == 0x2800);
    }

    #[test]
    fn out_of_bounds_is_ignored() {
        let mut c = Canvas::new(2, 2); // 4x8 px
        c.set(99, 99);
        c.set(4, 0);
        c.set(0, 8);
        assert_eq!(c.render(), "⠀⠀\n⠀⠀");
    }
}
