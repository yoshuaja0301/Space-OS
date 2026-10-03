//! Shared, clipped software canvas submitted through safe firmware GOP BLT.

use alloc::vec::Vec;
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::console::gop::{BltOp, BltPixel, BltRegion, GraphicsOutput};

pub const BG: BltPixel = BltPixel::new(0xEE, 0xF2, 0xF6);
pub const INK: BltPixel = BltPixel::new(0x17, 0x2B, 0x42);
pub const MUTED: BltPixel = BltPixel::new(0x52, 0x67, 0x7D);
pub const ACCENT: BltPixel = BltPixel::new(0x08, 0x7F, 0x8C);
pub const WHITE: BltPixel = BltPixel::new(0xFF, 0xFF, 0xFF);
pub const BORDER: BltPixel = BltPixel::new(0xD5, 0xDF, 0xE8);
pub const SOFT: BltPixel = BltPixel::new(0xDD, 0xF0, 0xF2);
pub const WARNING: BltPixel = BltPixel::new(0x99, 0x5C, 0x12);
pub const TEXT_HEIGHT: usize = 16;

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

pub struct Ui {
    gop: ScopedProtocol<GraphicsOutput>,
    pixels: Vec<BltPixel>,
    width: usize,
    height: usize,
}

impl Ui {
    /// Returns the text-fallback signal if GOP or canvas allocation is unavailable.
    pub fn new() -> Option<Self> {
        let handle = boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
        let gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle).ok()?;
        let (width, height) = gop.current_mode_info().resolution();
        let count = width.checked_mul(height)?;
        if width < 640 || height < 480 || count > 3840 * 2160 {
            return None;
        }
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(count).ok()?;
        pixels.resize(count, BG);
        Some(Self { gop, pixels, width, height })
    }

    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    pub const fn inset(&self) -> usize {
        if self.width < 800 { 32 } else { 48 }
    }

    pub fn char_width(&self) -> usize {
        get_raster_width(FontWeight::Regular, RasterHeight::Size16)
    }

    pub fn begin(&mut self, title: &str, subtitle: &str) {
        self.pixels.fill(BG);
        let inset = self.inset();
        self.text(inset, 24, "SPACE OS", 1, ACCENT);
        self.rect(Rect { x: inset, y: 56, width: self.width - 2 * inset, height: 1 }, BORDER);
        self.text(inset, 70, title, 2, INK);
        self.text(inset, 112, subtitle, 1, MUTED);
    }

    pub fn rect(&mut self, area: Rect, color: BltPixel) {
        let right = area.x.saturating_add(area.width).min(self.width);
        let bottom = area.y.saturating_add(area.height).min(self.height);
        for y in area.y.min(self.height)..bottom {
            let row = y * self.width;
            self.pixels[row + area.x.min(self.width)..row + right].fill(color);
        }
    }

    pub fn panel(&mut self, area: Rect) {
        self.rect(area, BORDER);
        self.rect(
            Rect {
                x: area.x.saturating_add(1),
                y: area.y.saturating_add(1),
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            },
            WHITE,
        );
    }

    /// Draws a single clipped line; scales outside the documented 1..=3 use 1.
    pub fn text(&mut self, mut x: usize, y: usize, label: &str, scale: usize, color: BltPixel) {
        let scale = if (1..=3).contains(&scale) { scale } else { 1 };
        let advance = get_raster_width(FontWeight::Regular, RasterHeight::Size16) * scale;
        for c in label.chars() {
            if x.saturating_add(advance) > self.width || y >= self.height {
                break;
            }
            let glyph = get_raster(c, FontWeight::Regular, RasterHeight::Size16)
                .or_else(|| get_raster('?', FontWeight::Regular, RasterHeight::Size16));
            if let Some(glyph) = glyph {
                for (dy, line) in glyph.raster().iter().enumerate() {
                    for (dx, &coverage) in line.iter().enumerate() {
                        if coverage == 0 {
                            continue;
                        }
                        let left = x + dx * scale;
                        let top = y.saturating_add(dy * scale);
                        for py in top..top.saturating_add(scale).min(self.height) {
                            for px in left..left.saturating_add(scale).min(self.width) {
                                let index = py * self.width + px;
                                self.pixels[index] = blend(color, self.pixels[index], coverage);
                            }
                        }
                    }
                }
            }
            x += advance;
        }
    }

    /// Submits one complete frame without writing to the console.
    pub fn present(&mut self) -> uefi::Result {
        self.gop.blt(BltOp::BufferToVideo {
            buffer: &self.pixels,
            src: BltRegion::Full,
            dest: (0, 0),
            dims: (self.width, self.height),
        })
    }
}

fn blend(foreground: BltPixel, background: BltPixel, coverage: u8) -> BltPixel {
    let mix = |front: u8, back: u8| {
        let value =
            (u16::from(front) * u16::from(coverage) + u16::from(back) * (255 - u16::from(coverage))) / 255;
        value.to_le_bytes()[0]
    };
    BltPixel::new(
        mix(foreground.red, background.red),
        mix(foreground.green, background.green),
        mix(foreground.blue, background.blue),
    )
}
