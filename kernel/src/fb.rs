//! Framebuffer text console (software rendering, PRD §6 MVP graphics).

use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use spaceabi::boot::{BootInfo, fb_format};

use crate::mm::phys_to_virt;
use crate::sync::SpinLock;

const FONT_H: usize = 16;
const BG: (u8, u8, u8) = (0x0B, 0x12, 0x20);
const FG: (u8, u8, u8) = (0xD8, 0xE0, 0xEC);

struct Fb {
    ptr: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
    bgr: bool,
    glyph_w: usize,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
}

// SAFETY: the framebuffer is only accessed under the lock.
unsafe impl Send for Fb {}

static FB: SpinLock<Option<Fb>> = SpinLock::new(None);

impl Fb {
    fn pack(&self, (r, g, b): (u8, u8, u8)) -> u32 {
        if self.bgr {
            ((r as u32) << 16) | ((g as u32) << 8) | b as u32
        } else {
            ((b as u32) << 16) | ((g as u32) << 8) | r as u32
        }
    }

    fn put_pixel(&mut self, x: usize, y: usize, v: u32) {
        if x < self.width && y < self.height {
            // SAFETY: bounds checked against the mode geometry.
            unsafe { self.ptr.add(y * self.stride + x).write_volatile(v) };
        }
    }

    fn clear(&mut self) {
        let bg = self.pack(BG);
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, bg);
            }
        }
        self.col = 0;
        self.row = 0;
    }

    fn scroll(&mut self) {
        let lines = FONT_H * self.stride;
        let total = self.height * self.stride;
        // SAFETY: both ranges lie inside the framebuffer.
        unsafe { core::ptr::copy(self.ptr.add(lines), self.ptr, total - lines) };
        let bg = self.pack(BG);
        for y in self.height - FONT_H..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, bg);
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    fn put_char(&mut self, c: char) {
        if c == '\n' {
            self.newline();
            return;
        }
        if c == '\r' {
            self.col = 0;
            return;
        }
        if c == '\u{8}' {
            // Backspace: step back and blank the cell, so a person editing a command
            // line sees the same thing on the screen as on the serial console.
            if self.col > 0 {
                self.col -= 1;
            }
            let x0 = self.col * self.glyph_w;
            let y0 = self.row * FONT_H;
            let bg = self.pack(BG);
            for dy in 0..FONT_H {
                for dx in 0..self.glyph_w {
                    self.put_pixel(x0 + dx, y0 + dy, bg);
                }
            }
            return;
        }
        if self.col >= self.cols {
            self.newline();
        }
        let glyph = get_raster(c, FontWeight::Regular, RasterHeight::Size16)
            .or_else(|| get_raster('?', FontWeight::Regular, RasterHeight::Size16));
        let x0 = self.col * self.glyph_w;
        let y0 = self.row * FONT_H;
        if let Some(g) = glyph {
            for (dy, line) in g.raster().iter().enumerate() {
                for (dx, &i) in line.iter().enumerate() {
                    let i = i as u32;
                    let mix = |f: u8, b: u8| ((f as u32 * i + b as u32 * (255 - i)) / 255) as u8;
                    let v = self.pack((mix(FG.0, BG.0), mix(FG.1, BG.1), mix(FG.2, BG.2)));
                    self.put_pixel(x0 + dx, y0 + dy, v);
                }
            }
        }
        self.col += 1;
    }
}

pub fn init(bi: &BootInfo) {
    let f = &bi.framebuffer;
    if f.present == 0 || f.bytes_per_pixel != 4 || f.format == fb_format::OTHER {
        println!("[kernel] framebuffer: none usable; serial console only");
        return;
    }
    let glyph_w = get_raster_width(FontWeight::Regular, RasterHeight::Size16);
    // Never address beyond what the firmware reported (and the bootloader mapped):
    // the drawable width is bounded by the stride and the rows by `size`.
    let stride = f.stride as usize;
    let width = (f.width as usize).min(stride);
    let rows_in_buffer = if stride == 0 { 0 } else { (f.size as usize / 4) / stride };
    let height = (f.height as usize).min(rows_in_buffer);
    if width == 0 || height < FONT_H || stride == 0 {
        println!(
            "[kernel] framebuffer: geometry unusable ({}x{}, stride {}, {} bytes)",
            f.width, f.height, f.stride, f.size
        );
        return;
    }
    let mut fb = Fb {
        ptr: phys_to_virt(f.phys_addr).as_mut_ptr::<u32>(),
        width,
        height,
        stride,
        bgr: f.format == fb_format::BGRX,
        glyph_w,
        cols: width / glyph_w,
        rows: height / FONT_H,
        col: 0,
        row: 0,
    };
    fb.clear();
    let cols = fb.cols;
    let rows = fb.rows;
    *FB.lock() = Some(fb);
    println!(
        "[kernel] framebuffer: {}x{} ({}), text console {}x{}",
        f.width,
        f.height,
        if f.format == fb_format::BGRX { "BGRX" } else { "RGBX" },
        cols,
        rows
    );
}

pub fn write_str(s: &str) {
    // The console lock is already held by the caller; FB has its own lock so the
    // panic path can bypass the console lock safely.
    // `try_lock`: if the framebuffer lock is held (panic inside the renderer) we
    // silently skip the framebuffer instead of deadlocking; serial still gets the text.
    let Some(mut g) = FB.try_lock() else { return };
    if let Some(fb) = g.as_mut() {
        for c in s.chars() {
            fb.put_char(c);
        }
    }
}
