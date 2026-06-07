//! Screenshot annotation: overlay a labeled coordinate grid onto a PNG.
//!
//! A coordinate grid is a lightweight grounding aid for vision models that
//! struggle to estimate absolute pixel coordinates from a bare screenshot
//! (notably smaller/open models). Gridlines plus axis labels let the model
//! read off a target's `(x, y)` instead of guessing. This is pure image
//! processing — no element detection, no extra system dependencies.

use std::io::Cursor;

use image::{Rgba, RgbaImage};
use rustykrab_core::Result;

fn err(msg: impl Into<String>) -> rustykrab_core::Error {
    rustykrab_core::Error::ToolExecution(msg.into().into())
}

/// 3x5 bitmap glyphs for digits 0-9. Each row uses the low 3 bits, MSB = left.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

const SCALE: u32 = 2; // glyph pixel scale
const GLYPH_W: u32 = 3 * SCALE;
const GLYPH_H: u32 = 5 * SCALE;
const GLYPH_GAP: u32 = SCALE;

const LINE: Rgba<u8> = Rgba([0, 255, 0, 255]); // grid line color (lime)
const LABEL_BG: Rgba<u8> = Rgba([0, 0, 0, 255]); // label background
const LABEL_FG: Rgba<u8> = Rgba([255, 255, 255, 255]); // label text

/// Alpha-blend `over` at 50% onto `base` (ignoring `over`'s alpha).
fn blend_half(base: Rgba<u8>, over: Rgba<u8>) -> Rgba<u8> {
    Rgba([
        ((base[0] as u16 + over[0] as u16) / 2) as u8,
        ((base[1] as u16 + over[1] as u16) / 2) as u8,
        ((base[2] as u16 + over[2] as u16) / 2) as u8,
        255,
    ])
}

fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    let (iw, ih) = img.dimensions();
    for py in y..(y + h).min(ih) {
        for px in x..(x + w).min(iw) {
            img.put_pixel(px, py, color);
        }
    }
}

fn draw_digit(img: &mut RgbaImage, ox: u32, oy: u32, d: usize) {
    let (iw, ih) = img.dimensions();
    let glyph = DIGITS[d];
    for (ry, row) in glyph.iter().enumerate() {
        for cx in 0..3u32 {
            if row & (1 << (2 - cx)) != 0 {
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let px = ox + cx * SCALE + dx;
                        let py = oy + ry as u32 * SCALE + dy;
                        if px < iw && py < ih {
                            img.put_pixel(px, py, LABEL_FG);
                        }
                    }
                }
            }
        }
    }
}

/// Draw `value` as a labeled box anchored near `(x, y)`, clamped to stay on
/// screen so edge labels remain readable.
fn draw_label(img: &mut RgbaImage, x: u32, y: u32, value: u32) {
    let (iw, ih) = img.dimensions();
    let s = value.to_string();
    let text_w = s.len() as u32 * (GLYPH_W + GLYPH_GAP);
    let box_w = text_w + 2;
    let box_h = GLYPH_H + 2;

    let bx = x.min(iw.saturating_sub(box_w));
    let by = y.min(ih.saturating_sub(box_h));

    fill_rect(img, bx, by, box_w, box_h, LABEL_BG);
    let mut cx = bx + 1;
    for ch in s.bytes() {
        draw_digit(img, cx, by + 1, (ch - b'0') as usize);
        cx += GLYPH_W + GLYPH_GAP;
    }
}

/// Overlay a labeled coordinate grid onto a PNG and return the new PNG bytes.
///
/// Vertical lines (and top-edge labels) mark `x` columns; horizontal lines
/// (and left-edge labels) mark `y` rows, every `spacing` pixels. The output
/// has the same dimensions as the input. `spacing` is clamped to a sane floor.
pub fn annotate_with_grid(png: &[u8], spacing: u32) -> Result<Vec<u8>> {
    let mut img = image::load_from_memory(png)
        .map_err(|e| err(format!("decoding screenshot PNG: {e}")))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let spacing = spacing.max(25);

    // Vertical lines + top labels.
    let mut x = spacing;
    while x < w {
        for y in 0..h {
            let p = *img.get_pixel(x, y);
            img.put_pixel(x, y, blend_half(p, LINE));
        }
        draw_label(&mut img, x + 1, 1, x);
        x += spacing;
    }
    // Horizontal lines + left labels.
    let mut y = spacing;
    while y < h {
        for x in 0..w {
            let p = *img.get_pixel(x, y);
            img.put_pixel(x, y, blend_half(p, LINE));
        }
        draw_label(&mut img, 1, y + 1, y);
        y += spacing;
    }

    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| err(format!("encoding annotated PNG: {e}")))?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba([40, 40, 40, 255]));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn grid_preserves_dimensions_and_is_valid_png() {
        let src = sample_png(300, 200);
        let out = annotate_with_grid(&src, 100).expect("annotate");
        let decoded = image::load_from_memory(&out).expect("valid png").to_rgba8();
        assert_eq!(decoded.dimensions(), (300, 200));
        assert_ne!(out, src, "grid should modify the image");
    }

    #[test]
    fn grid_draws_lines_at_spacing() {
        let src = sample_png(300, 200);
        let out = annotate_with_grid(&src, 100).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgba8();
        // A pixel on the x=100 vertical line (below the label area) should be
        // tinted toward the line color, not the original gray.
        let p = img.get_pixel(100, 150);
        assert!(
            p[1] > p[0] && p[1] > p[2],
            "expected green-tinted line pixel, got {p:?}"
        );
    }

    #[test]
    fn tiny_spacing_is_clamped_and_does_not_panic() {
        let src = sample_png(80, 60);
        let out = annotate_with_grid(&src, 1).expect("clamped");
        assert_eq!(
            image::load_from_memory(&out)
                .unwrap()
                .to_rgba8()
                .dimensions(),
            (80, 60)
        );
    }

    #[test]
    fn rejects_non_png_input() {
        assert!(annotate_with_grid(b"not a png", 100).is_err());
    }
}
