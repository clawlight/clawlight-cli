//! Windows tray readout: draws the usage figure into the tray icon itself.
//!
//! macOS shows the readout as title text beside the menu bar icon. A Windows
//! notification-area icon is a small square with no text slot, so the figure
//! has to be painted into the bitmap: the mascot shrinks into the top of the
//! icon and the readout sits on a dark strip along the bottom (dark so white
//! digits stay legible on both the light and the dark taskbar).
//!
//! Everything here is plain pixel work on an `RgbaImage` — no Win32 text
//! APIs — so it is unit-tested on every platform even though only the Windows
//! tray uses it. Glyphs come from two embedded pixel fonts; anti-aliased
//! system text is mush at the 16px a 100%-scale tray icon gets.

use image::imageops::{self, FilterType};
use image::{Rgba, RgbaImage};

/// One pixel font: glyph rows are strings where `#` is an inked pixel. A
/// glyph's width is its row length, so `.` can be narrower than a digit.
struct Font {
    height: u32,
    glyphs: &'static [(char, &'static [&'static str])],
}

/// 3x5 — the only size at which four characters fit a 16px icon.
const FONT_3X5: Font = Font {
    height: 5,
    glyphs: &[
        ('0', &["###", "#.#", "#.#", "#.#", "###"]),
        ('1', &[".#.", "##.", ".#.", ".#.", "###"]),
        ('2', &["###", "..#", "###", "#..", "###"]),
        ('3', &["###", "..#", "###", "..#", "###"]),
        ('4', &["#.#", "#.#", "###", "..#", "..#"]),
        ('5', &["###", "#..", "###", "..#", "###"]),
        ('6', &["###", "#..", "###", "#.#", "###"]),
        ('7', &["###", "..#", "..#", "..#", "..#"]),
        ('8', &["###", "#.#", "###", "#.#", "###"]),
        ('9', &["###", "#.#", "###", "..#", "###"]),
        ('$', &[".##", "##.", ".#.", ".##", "##."]),
        ('%', &["#.#", "..#", ".#.", "#..", "#.#"]),
        ('.', &[".", ".", ".", ".", "#"]),
        ('k', &["#..", "#.#", "##.", "#.#", "#.#"]),
    ],
};

/// 5x7 — used where the icon is big enough (e.g. 24px at 150% scaling).
const FONT_5X7: Font = Font {
    height: 7,
    glyphs: &[
        (
            '0',
            &[
                ".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###.",
            ],
        ),
        (
            '1',
            &[
                "..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###.",
            ],
        ),
        (
            '2',
            &[
                ".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####",
            ],
        ),
        (
            '3',
            &[
                "#####", "...#.", "..#..", "...#.", "....#", "#...#", ".###.",
            ],
        ),
        (
            '4',
            &[
                "...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#.",
            ],
        ),
        (
            '5',
            &[
                "#####", "#....", "####.", "....#", "....#", "#...#", ".###.",
            ],
        ),
        (
            '6',
            &[
                "..##.", ".#...", "#....", "####.", "#...#", "#...#", ".###.",
            ],
        ),
        (
            '7',
            &[
                "#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#...",
            ],
        ),
        (
            '8',
            &[
                ".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###.",
            ],
        ),
        (
            '9',
            &[
                ".###.", "#...#", "#...#", ".####", "....#", "...#.", ".##..",
            ],
        ),
        (
            '$',
            &[
                "..#..", ".####", "#.#..", ".###.", "..#.#", "####.", "..#..",
            ],
        ),
        (
            '%',
            &[
                "##...", "##..#", "...#.", "..#..", ".#...", "#..##", "...##",
            ],
        ),
        ('.', &["..", "..", "..", "..", "..", "##", "##"]),
        (
            'k',
            &[
                "#....", "#....", "#..#.", "#.#..", "##...", "#.#..", "#..#.",
            ],
        ),
    ],
};

const FONTS: &[&Font] = &[&FONT_3X5, &FONT_5X7];

/// Strip behind the digits, and the digits themselves.
const STRIP: Rgba<u8> = Rgba([24, 24, 27, 240]);
const INK: Rgba<u8> = Rgba([255, 255, 255, 255]);

impl Font {
    fn glyph(&self, c: char) -> Option<&'static [&'static str]> {
        self.glyphs.iter().find(|(g, _)| *g == c).map(|(_, r)| *r)
    }

    /// Unscaled pixel width of `text` (1px between glyphs), or `None` if the
    /// font lacks one of its characters.
    fn width(&self, text: &str) -> Option<u32> {
        let mut width = 0;
        for (i, c) in text.chars().enumerate() {
            width += self.glyph(c)?[0].len() as u32 + u32::from(i > 0);
        }
        Some(width)
    }
}

/// Shorten a tray readout to the four characters a tray icon can hold.
/// Percentages (`"42%"`, `"100%"`) already fit; dollars lose precision as
/// they grow — `$0.42` → `$0.4`, `$12.34` → `$12`, `$1234.50` → `$1k`. The
/// exact figure stays in the tooltip.
pub fn compact(readout: &str) -> String {
    let Some(dollars) = readout
        .strip_prefix('$')
        .and_then(|d| d.parse::<f64>().ok())
    else {
        return readout.to_string();
    };
    if dollars < 9.95 {
        format!("${dollars:.1}")
    } else if dollars < 999.5 {
        format!("${dollars:.0}")
    } else {
        format!("${:.0}k", dollars / 1000.0)
    }
}

/// Compose a `size`×`size` tray icon: `base` (the state-colored mascot)
/// shrunk into the top, `text` on a strip along the bottom. `None` when the
/// text can't be drawn — empty, a character outside the fonts, or too wide
/// for the icon — so the caller falls back to the plain mascot.
pub fn compose(base: &RgbaImage, text: &str, size: u32) -> Option<RgbaImage> {
    if text.is_empty() {
        return None;
    }
    // Largest integer-scaled font whose rendering of `text` fits the icon's
    // width while the strip (digits plus a `scale`-px margin above and below)
    // takes at most half its height — the mascot keeps the rest.
    let (font, scale, text_w) = FONTS
        .iter()
        .filter_map(|font| {
            let width = font.width(text)?;
            let scale = (size / width).min(size / 2 / (font.height + 2));
            (scale > 0).then_some((*font, scale, width * scale))
        })
        .max_by_key(|(font, scale, _)| font.height * scale)?;
    let pad = scale;
    let strip_h = font.height * scale + 2 * pad;
    let mascot_h = size - strip_h;

    let mut out = RgbaImage::new(size, size);

    // Mascot: trim the PNG's transparent margin so every pixel of the top
    // area goes to the mascot itself, then scale to fit, centered.
    if let Some((x, y, w, h)) = opaque_bounds(base) {
        let art = imageops::crop_imm(base, x, y, w, h).to_image();
        let fit = f64::min(
            f64::from(size) / f64::from(w),
            f64::from(mascot_h) / f64::from(h),
        );
        let (dw, dh) = (
            ((f64::from(w) * fit).round() as u32).clamp(1, size),
            ((f64::from(h) * fit).round() as u32).clamp(1, mascot_h),
        );
        let art = imageops::resize(&art, dw, dh, FilterType::Triangle);
        imageops::overlay(
            &mut out,
            &art,
            i64::from((size - dw) / 2),
            i64::from((mascot_h - dh) / 2),
        );
    }

    for y in mascot_h..size {
        for x in 0..size {
            out.put_pixel(x, y, STRIP);
        }
    }

    let mut pen_x = (size - text_w) / 2;
    let pen_y = mascot_h + pad;
    for c in text.chars() {
        let rows = font.glyph(c)?;
        for (gy, row) in rows.iter().enumerate() {
            for (gx, cell) in row.bytes().enumerate() {
                if cell != b'#' {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        out.put_pixel(
                            pen_x + gx as u32 * scale + dx,
                            pen_y + gy as u32 * scale + dy,
                            INK,
                        );
                    }
                }
            }
        }
        pen_x += (rows[0].len() as u32 + 1) * scale;
    }
    Some(out)
}

/// Bounding box `(x, y, w, h)` of the non-transparent pixels, if any.
fn opaque_bounds(img: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None; // min x/y, max x/y
    for (x, y, px) in img.enumerate_pixels() {
        if px[3] == 0 {
            continue;
        }
        bounds = Some(match bounds {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    }
    bounds.map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mascot() -> RgbaImage {
        image::load_from_memory(crate::menubar::ICON_GREEN)
            .unwrap()
            .to_rgba8()
    }

    fn ink_pixels(img: &RgbaImage) -> usize {
        img.pixels().filter(|p| **p == INK).count()
    }

    #[test]
    fn glyph_rows_are_rectangular() {
        for font in FONTS {
            for (c, rows) in font.glyphs {
                assert_eq!(rows.len() as u32, font.height, "{c:?} height");
                assert!(
                    rows.iter().all(|r| r.len() == rows[0].len()),
                    "{c:?} has ragged rows"
                );
            }
        }
    }

    #[test]
    fn compact_keeps_readouts_within_four_chars() {
        for (input, want) in [
            ("42%", "42%"),
            ("100%", "100%"),
            ("$0.00", "$0.0"),
            ("$0.42", "$0.4"),
            ("$9.94", "$9.9"),
            ("$9.99", "$10"),
            ("$12.34", "$12"),
            ("$999.49", "$999"),
            ("$999.50", "$1k"),
            ("$1234.50", "$1k"),
            ("$12345.00", "$12k"),
        ] {
            assert_eq!(compact(input), want, "compact({input:?})");
            assert!(want.chars().count() <= 4);
        }
    }

    /// Every readout shape must render at every size Windows asks for
    /// (16px at 100% scaling up to 48px at 300%).
    #[test]
    fn every_readout_fits_every_tray_size() {
        let base = mascot();
        for size in [16, 20, 24, 28, 32, 40, 48] {
            for text in ["7%", "42%", "100%", "$0.4", "$12", "$999", "$12k"] {
                let icon = compose(&base, text, size)
                    .unwrap_or_else(|| panic!("{text:?} should fit a {size}px icon"));
                assert_eq!(icon.dimensions(), (size, size));
                assert!(ink_pixels(&icon) > 0, "{text:?}@{size} drew no digits");
            }
        }
    }

    #[test]
    fn mascot_keeps_its_state_color_above_the_strip() {
        let icon = compose(&mascot(), "42%", 16).unwrap();
        // Strip is the bottom 7 rows at 16px (5px digits + 1px margins).
        assert_eq!(*icon.get_pixel(0, 9), STRIP);
        assert_eq!(*icon.get_pixel(0, 8), Rgba([0, 0, 0, 0]));
        let green = (0..9)
            .flat_map(|y| (0..16).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let p = icon.get_pixel(x, y);
                p[3] > 200 && p[1] > p[0] && p[1] > p[2]
            })
            .count();
        assert!(green > 20, "mascot should still read as green ({green}px)");
    }

    #[test]
    fn undrawable_text_falls_back_to_none() {
        let base = mascot();
        assert!(compose(&base, "", 16).is_none());
        assert!(compose(&base, "n/a", 16).is_none()); // glyphs we don't have
        assert!(compose(&base, "$12.34", 16).is_none()); // too wide at 16px
    }

    /// Dev aid: `CLAWLIGHT_BADGE_PREVIEW=<dir> cargo test badge_preview`
    /// writes every state × size × readout to PNGs for eyeballing.
    #[test]
    fn badge_preview() {
        let Some(dir) = std::env::var_os("CLAWLIGHT_BADGE_PREVIEW") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let states = [
            ("green", crate::menubar::ICON_GREEN),
            ("yellow", crate::menubar::ICON_YELLOW),
            ("red", crate::menubar::ICON_RED),
            ("none", crate::menubar::ICON_NONE),
        ];
        for (name, bytes) in states {
            let base = image::load_from_memory(bytes).unwrap().to_rgba8();
            for size in [16, 20, 24, 32, 48] {
                for text in ["42%", "100%", "$0.4", "$12", "$999"] {
                    let file = format!("{name}-{size}-{}.png", text.replace(['$', '%'], ""));
                    compose(&base, text, size)
                        .unwrap()
                        .save(dir.join(file))
                        .unwrap();
                }
            }
        }
    }
}
