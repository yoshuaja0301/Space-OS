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

/// Contrast between two colours (`0x00RRGGBB`) as WCAG defines it, times 100 and
/// rounded down: 450 is the least for text (AA), 700 for text in a high-contrast
/// theme (AAA), 300 for the outline of a control or a focus indicator. A `const fn`,
/// so a palette can be held to it when the program is compiled.
pub const fn contrast_x100(a: u32, b: u32) -> u32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    ((hi + 50_000) * 100 / (lo + 50_000)) as u32
}

/// Relative luminance of `c`, in millionths.
const fn luminance(c: u32) -> u64 {
    let r = SRGB_TO_LINEAR[((c >> 16) & 0xFF) as usize] as u64;
    let g = SRGB_TO_LINEAR[((c >> 8) & 0xFF) as usize] as u64;
    let b = SRGB_TO_LINEAR[(c & 0xFF) as usize] as u64;
    (2126 * r + 7152 * g + 722 * b) / 10_000
}

/// Each 8-bit sRGB level as linear light, in millionths (the sRGB transfer function,
/// tabulated because it needs a power the compiler cannot evaluate in a constant).
#[rustfmt::skip]
const SRGB_TO_LINEAR: [u32; 256] = [
    0, 304, 607, 911, 1214, 1518, 1821, 2125, 2428, 2732, 3035, 3347,
    3677, 4025, 4391, 4777, 5182, 5605, 6049, 6512, 6995, 7499, 8023, 8568,
    9134, 9721, 10330, 10960, 11612, 12286, 12983, 13702, 14444, 15209, 15996, 16807,
    17642, 18500, 19382, 20289, 21219, 22174, 23153, 24158, 25187, 26241, 27321, 28426,
    29557, 30713, 31896, 33105, 34340, 35601, 36889, 38204, 39546, 40915, 42311, 43735,
    45186, 46665, 48172, 49707, 51269, 52861, 54480, 56128, 57805, 59511, 61246, 63010,
    64803, 66626, 68478, 70360, 72272, 74214, 76185, 78187, 80220, 82283, 84376, 86500,
    88656, 90842, 93059, 95307, 97587, 99899, 102242, 104616, 107023, 109462, 111932, 114435,
    116971, 119538, 122139, 124772, 127438, 130136, 132868, 135633, 138432, 141263, 144128, 147027,
    149960, 152926, 155926, 158961, 162029, 165132, 168269, 171441, 174647, 177888, 181164, 184475,
    187821, 191202, 194618, 198069, 201556, 205079, 208637, 212231, 215861, 219526, 223228, 226966,
    230740, 234551, 238398, 242281, 246201, 250158, 254152, 258183, 262251, 266356, 270498, 274677,
    278894, 283149, 287441, 291771, 296138, 300544, 304987, 309469, 313989, 318547, 323143, 327778,
    332452, 337164, 341914, 346704, 351533, 356400, 361307, 366253, 371238, 376262, 381326, 386429,
    391572, 396755, 401978, 407240, 412543, 417885, 423268, 428690, 434154, 439657, 445201, 450786,
    456411, 462077, 467784, 473531, 479320, 485150, 491021, 496933, 502886, 508881, 514918, 520996,
    527115, 533276, 539479, 545724, 552011, 558340, 564712, 571125, 577580, 584078, 590619, 597202,
    603827, 610496, 617207, 623960, 630757, 637597, 644480, 651406, 658375, 665387, 672443, 679542,
    686685, 693872, 701102, 708376, 715694, 723055, 730461, 737910, 745404, 752942, 760525, 768151,
    775822, 783538, 791298, 799103, 806952, 814847, 822786, 830770, 838799, 846873, 854993, 863157,
    871367, 879622, 887923, 896269, 904661, 913099, 921582, 930111, 938686, 947307, 955973, 964686,
    973445, 982251, 991102, 1000000,
];

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
