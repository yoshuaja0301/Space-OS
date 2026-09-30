//! Software drawing (ADR-0020): windows and the desktop are blocks of `0x00RRGGBB`
//! pixels, and everything here is clipped to the block it draws on.

use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};

/// Height of a line of text, in pixels.
pub const GLYPH_H: i32 = 16;

/// Width of one character cell, in pixels.
pub fn glyph_w() -> i32 {
    get_raster_width(FontWeight::Regular, RasterHeight::Size16) as i32
}

/// Blend `fg` over `bg`, `alpha` of 255 being all `fg`.
pub fn blend(fg: u32, bg: u32, alpha: u32) -> u32 {
    let ch = |shift: u32| {
        let f = (fg >> shift) & 0xFF;
        let b = (bg >> shift) & 0xFF;
        ((f * alpha + b * (255 - alpha)) / 255) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// A block of pixels to draw on: `stride` pixels per row, `w` by `h` of them used.
pub struct Surface<'a> {
    pub px: &'a mut [u32],
    pub w: i32,
    pub h: i32,
    pub stride: usize,
}

impl<'a> Surface<'a> {
    /// A surface over `px`, which must hold at least `stride * h` pixels.
    pub fn new(px: &'a mut [u32], w: i32, h: i32, stride: usize) -> Surface<'a> {
        let h = h.min((px.len() / stride.max(1)) as i32);
        Surface { px, w: w.min(stride as i32), h, stride }
    }

    /// Clip a rectangle to the surface: `(x0, y0, x1, y1)`, empty when `x0 >= x1`
    /// or `y0 >= y1`.
    fn clip(&self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = x.saturating_add(w.max(0)).min(self.w);
        let y1 = y.saturating_add(h.max(0)).min(self.h);
        (x0, y0, x1, y1)
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for yy in y0..y1 {
            let row = yy as usize * self.stride;
            self.px[row + x0 as usize..row + x1 as usize].fill(color);
        }
    }

    /// Blend `color` over the rectangle (`alpha` 0..=255).
    pub fn shade(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32, alpha: u32) {
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        for yy in y0..y1.max(y0) {
            let row = yy as usize * self.stride;
            for p in &mut self.px[row + x0 as usize..row + x1.max(x0) as usize] {
                *p = blend(color, *p, alpha);
            }
        }
    }

    /// A rectangle's outline, `t` pixels thick, inside the rectangle.
    pub fn frame(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, color: u32) {
        self.fill(x, y, w, t, color);
        self.fill(x, y + h - t, w, t, color);
        self.fill(x, y, t, h, color);
        self.fill(x + w - t, y, t, h, color);
    }

    /// Draw `s` with its top-left corner at `x, y`, blended onto what is there.
    /// Characters outside the font's range show as `?`. Returns the width drawn.
    pub fn text(&mut self, x: i32, y: i32, s: &str, color: u32) -> i32 {
        let gw = glyph_w();
        let mut cx = x;
        for c in s.chars() {
            if cx >= self.w {
                break;
            }
            let glyph = get_raster(c, FontWeight::Regular, RasterHeight::Size16)
                .or_else(|| get_raster('?', FontWeight::Regular, RasterHeight::Size16));
            if let Some(g) = glyph {
                for (dy, line) in g.raster().iter().enumerate() {
                    let py = y + dy as i32;
                    if py < 0 || py >= self.h {
                        continue;
                    }
                    let row = py as usize * self.stride;
                    for (dx, &a) in line.iter().enumerate() {
                        let px = cx + dx as i32;
                        if a == 0 || px < 0 || px >= self.w {
                            continue;
                        }
                        let p = &mut self.px[row + px as usize];
                        *p = blend(color, *p, a as u32);
                    }
                }
            }
            cx += gw;
        }
        cx - x
    }

    /// Copy `src` (`sw` by `sh`, `sstride` pixels per row) with its top-left corner
    /// at `dx, dy`, clipped to both.
    pub fn blit(&mut self, src: &[u32], sw: i32, sh: i32, sstride: usize, dx: i32, dy: i32) {
        let sh = sh.min((src.len() / sstride.max(1)) as i32);
        let (x0, y0, x1, y1) = self.clip(dx, dy, sw.min(sstride as i32), sh);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for yy in y0..y1 {
            let s_row = (yy - dy) as usize * sstride + (x0 - dx) as usize;
            let d_row = yy as usize * self.stride + x0 as usize;
            let n = (x1 - x0) as usize;
            self.px[d_row..d_row + n].copy_from_slice(&src[s_row..s_row + n]);
        }
    }
}
