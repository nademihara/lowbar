//! Software renderer: paints the bar into an ARGB8888 pixel buffer.
//!
//! No GPU, no scene graph — just bytes we write by hand and hand to the
//! compositor via wl_shm. Everything that defines the look of the bar lives
//! in the constants below, so restyling is a matter of editing numbers.

use fontdue::{Font, FontSettings};

use crate::status::Status;

// ---------------------------------------------------------------------------
// look & feel — edit me
// ---------------------------------------------------------------------------

/// Height of the bar, in logical (output) pixels.
pub const HEIGHT: u32 = 26;
/// Font size, in logical pixels.
const FONT_SIZE: f32 = 13.0;

/// Horizontal padding at the ends of the bar.
const PAD: f32 = 10.0;
/// Empty space between neighbouring items.
const GAP: f32 = 14.0;

/// Horizontal padding inside a workspace pill.
const PILL_PAD_X: f32 = 5.0;
/// Vertical padding of a workspace pill.
const PILL_PAD_Y: f32 = 4.0;
/// Space between workspace pills.
const PILL_GAP: f32 = 8.0;

/// Colors are 0xAARRGGBB.
const BG: u32 = 0xff11111b; // catppuccin "crust"
const BORDER: u32 = 0xff24243a; // thin line under the bar
const FG: u32 = 0xffcdd6f4; // regular text
const ACCENT: u32 = 0xff89b4fa; // active workspace pill
const ON_ACCENT: u32 = 0xff11111b; // text drawn on the pill
const LOW: u32 = 0xfff38ba8; // battery getting scary

/// Font files we know how to find, tried in order. Override with $LOWBAR_FONT.
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/IosevkaNerdFont/IosevkaNerdFont-Regular.ttf",
    "/usr/share/fonts/TTF/IosevkaNerdFont-Regular.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/google-noto/NotoSansMono-Regular.ttf",
];

// ---------------------------------------------------------------------------
// font loading & text drawing
// ---------------------------------------------------------------------------

pub struct Renderer {
    font: Font,
}

impl Renderer {
    /// Load the first font we can find on the system.
    pub fn load() -> Result<Renderer, String> {
        let path = match std::env::var("LOWBAR_FONT") {
            Ok(p) => p,
            Err(_) => FONT_CANDIDATES
                .iter()
                .find(|p| std::path::Path::new(p).exists())
                .ok_or("no font found (set $LOWBAR_FONT to a .ttf path)")?
                .to_string(),
        };
        let data = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let font =
            Font::from_bytes(data, FontSettings::default()).map_err(|e| format!("{path}: {e}"))?;
        Ok(Renderer { font })
    }

    /// Width of `text` in logical pixels.
    fn text_width(&self, text: &str, scale: usize) -> f32 {
        let px = FONT_SIZE * scale as f32;
        let w: f32 = text
            .chars()
            .map(|c| self.font.metrics(c, px).advance_width)
            .sum();
        w / scale as f32
    }

    /// Draw one line of text with its left edge at logical `x`.
    /// Returns the logical x just past the text.
    fn text(&self, canvas: &mut Canvas, x: f32, text: &str, color: u32) -> f32 {
        let px = FONT_SIZE * canvas.scale as f32;
        let baseline = canvas.baseline(&self.font, px);
        let mut x = x * canvas.scale as f32; // physical from here on
        for ch in text.chars() {
            let (m, coverage) = self.font.rasterize(ch, px);
            // fontdue's ymin is the bottom edge of the bitmap (negative when
            // the glyph hangs below the baseline), so the top edge sits at
            // baseline - (height + ymin).
            let top = baseline - (m.height as i32 + m.ymin);
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = coverage[row * m.width + col];
                    if a == 0 {
                        continue;
                    }
                    let gx = (x + m.xmin as f32).round() as i32 + col as i32;
                    canvas.blend(gx, top + row as i32, color, a);
                }
            }
            x += m.advance_width;
        }
        x / canvas.scale as f32
    }
}

// ---------------------------------------------------------------------------
// tiny canvas
// ---------------------------------------------------------------------------

/// A view of the pixel buffer. Sizes are physical pixels; the helpers take
/// logical coordinates so the drawing code stays scale-agnostic.
struct Canvas<'a> {
    w: usize,
    h: usize,
    scale: usize,
    pixels: &'a mut [u8],
}

impl Canvas<'_> {
    /// Blend `color` over the pixel at physical (x, y) with alpha `a` (0-255).
    fn blend(&mut self, x: i32, y: i32, color: u32, a: u8) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 4;
        let dst = u32::from_ne_bytes(self.pixels[i..i + 4].try_into().unwrap());
        let a = a as u32;
        let inv = 255 - a;
        let mix = |shift: u32| {
            let s = (color >> shift) & 0xff;
            let d = (dst >> shift) & 0xff;
            ((s * a + d * inv) / 255) << shift
        };
        // The bar is opaque, so the alpha channel stays 0xff.
        let out = 0xff00_0000 | mix(16) | mix(8) | mix(0);
        self.pixels[i..i + 4].copy_from_slice(&out.to_ne_bytes());
    }

    /// Fill a rectangle given in logical pixels.
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        let s = self.scale as f32;
        let (x, y) = ((x * s) as i32, (y * s) as i32);
        let (w, h) = ((w * s).ceil() as i32, (h * s).ceil() as i32);
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, color, 255);
            }
        }
    }

    /// Y position of the text baseline that vertically centers a line.
    fn baseline(&self, font: &Font, px: f32) -> i32 {
        let m = font
            .horizontal_line_metrics(px)
            .unwrap_or(fontdue::LineMetrics {
                ascent: px * 0.8,
                descent: -px * 0.2,
                line_gap: 0.0,
                new_line_size: px,
            });
        let line = (m.ascent - m.descent) as i32; // descent is negative
        ((self.h as i32 - line) / 2).max(0) + m.ascent as i32
    }
}

// ---------------------------------------------------------------------------
// the bar itself
// ---------------------------------------------------------------------------

/// Paint the whole bar into `pixels` (ARGB8888, `w`x`h` physical pixels).
pub fn draw_bar(pixels: &mut [u8], w: usize, h: usize, scale: usize, r: &Renderer, s: &Status) {
    let mut c = Canvas {
        w,
        h,
        scale,
        pixels,
    };
    let lh = h as f32 / scale as f32; // logical height
    let lw = w as f32 / scale as f32; // logical width

    // background + a thin line at the bottom edge
    c.fill_rect(0.0, 0.0, lw, lh, BG);
    c.fill_rect(0.0, lh - 1.0, lw, 1.0, BORDER);

    // -- left: workspaces ---------------------------------------------------
    let mut x = PAD;
    for (name, active) in &s.workspaces {
        let tw = r.text_width(name, scale);
        if *active {
            c.fill_rect(
                x,
                PILL_PAD_Y,
                tw + 2.0 * PILL_PAD_X,
                lh - 2.0 * PILL_PAD_Y,
                ACCENT,
            );
            r.text(&mut c, x + PILL_PAD_X, name, ON_ACCENT);
        } else {
            r.text(&mut c, x + PILL_PAD_X, name, FG);
        }
        x += tw + 2.0 * PILL_PAD_X + PILL_GAP;
    }

    // -- right: battery + clock ---------------------------------------------
    let mut right = lw - PAD;

    // clock, flush against the right padding
    let cw = r.text_width(&s.clock, scale);
    r.text(&mut c, right - cw, &s.clock, FG);
    right -= cw + GAP;

    // battery to its left, red when nearly empty
    if let Some(b) = &s.battery {
        let text = b.text();
        let color = if b.percent <= 15 && !b.charging {
            LOW
        } else {
            FG
        };
        let bw = r.text_width(&text, scale);
        r.text(&mut c, right - bw, &text, color);
    }
}
