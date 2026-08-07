//! Render text to a PNG, with no GPU involved.
//!
//! Font problems — wrong baseline, clipped glyphs, tofu, emoji rendering as
//! boxes, ligatures not forming — are all *visible*, and diagnosing them
//! through a GPU renderer means guessing whether the bug is in the font layer
//! or the atlas. This puts the font layer on trial by itself.
//!
//! ```text
//! cargo run -p zest-font --example font_dump
//! cargo run -p zest-font --example font_dump -- --family "Cascadia Code" --size 24 --ligatures
//! ```

use zest_font::{Fonts, Style, Typography};

/// Lines chosen to exercise the things that actually break.
const SAMPLE: &[(&str, Style)] = &[
    ("The quick brown fox 0123456789", Style::new(false, false)),
    ("bold text ABCDEFG", Style::new(true, false)),
    ("italic text abcdefg", Style::new(false, true)),
    ("bold italic MMMWWWiii", Style::new(true, true)),
    // Ligature candidates -- these only form when shaping is on and the font
    // has the coverage.
    ("=> != >= <= --> ::: ==", Style::new(false, false)),
    // Fallback: CJK, Greek, Cyrillic, math. Any of these boxing up means the
    // fallback chain is not working.
    ("CJK 世界 日本語", Style::new(false, false)),
    ("Greek αβγδε Cyrillic бгд", Style::new(false, false)),
    ("Math ∑∏∫≈≠ Arrows ←↑→↓", Style::new(false, false)),
    // Box drawing and Powerline -- prompts depend on these.
    ("Box ┌─┬─┐ │ ├─┼─┤ └─┴─┘ █▓▒░", Style::new(false, false)),
    // Colour glyphs. These take the COLR/CBDT path, not the outline path.
    ("Emoji 👋 🚀 ✅ 🔥", Style::new(false, false)),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    let families: Vec<String> = arg("--family")
        .map(|f| vec![f])
        .unwrap_or_else(|| {
            ["Cascadia Mono", "Consolas", "JetBrains Mono", "monospace"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    let size_pt: f32 = arg("--size").and_then(|s| s.parse().ok()).unwrap_or(18.0);
    let scale: f32 = arg("--scale").and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let line_height: f32 = arg("--line-height").and_then(|s| s.parse().ok()).unwrap_or(1.2);
    let ligatures = args.iter().any(|a| a == "--ligatures");
    let out = arg("--out").unwrap_or_else(|| "font_dump.png".into());

    let typo = Typography {
        size_pt,
        line_height,
        letter_spacing: arg("--letter-spacing").and_then(|s| s.parse().ok()).unwrap_or(0.0),
        cell_width: 1.0,
        scale_factor: scale,
    };

    let mut fonts = match Fonts::new(&families, typo) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[font_dump] {e}");
            std::process::exit(1);
        }
    };

    let cm = fonts.cell_metrics();
    eprintln!("[font_dump] requested: {}", families.join(", "));
    eprintln!(
        "[font_dump] cell {}x{} baseline {} underline {}@{} faces {}",
        cm.cell_w, cm.cell_h, cm.baseline, cm.underline_y, cm.underline_thickness, fonts.face_count()
    );

    let cols = SAMPLE.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(40) + 2;
    let width = cols as u32 * cm.cell_w;
    let height = SAMPLE.len() as u32 * cm.cell_h;

    // RGBA canvas on a dark background, so light-on-dark rendering is what gets
    // inspected -- that is the case where gamma problems show up.
    let mut canvas = vec![0u8; (width * height * 4) as usize];
    for px in canvas.chunks_exact_mut(4) {
        px.copy_from_slice(&[0x0b, 0x0f, 0x1a, 0xff]);
    }

    let features: Vec<zest_font::Feature> = if ligatures {
        ["calt", "liga"].iter().filter_map(|f| zest_font::Feature::parse(f)).collect()
    } else {
        ["-calt", "-liga", "-dlig"].iter().filter_map(|f| zest_font::Feature::parse(f)).collect()
    };

    let mut missing = Vec::new();

    for (row, (text, style)) in SAMPLE.iter().enumerate() {
        let baseline_y = row as u32 * cm.cell_h + cm.baseline;

        // Ligatures need shaping; plain ASCII does not, and skipping it is
        // roughly 10x faster. The renderer makes the same choice per run.
        let glyphs = if Fonts::can_use_fast_path(text, ligatures) {
            fonts.map_ascii(text, *style)
        } else {
            fonts.shape_run(text, *style, &features)
        };

        // Map each shaped cluster back to the cell it started at. This is the
        // crux of ligatures on a grid: the shaper chooses the glyph, the grid
        // chooses the position, and the shaper's advances are discarded.
        //
        // Cells advance by *display width*, not character count, or CJK and
        // emoji overlap the character after them -- they occupy two cells. The
        // real renderer gets this from the grid, which already tracks WIDE and
        // WIDE_SPACER; here it has to be recomputed.
        let cell_of_byte: Vec<usize> = {
            use unicode_width::UnicodeWidthChar;
            let mut v = vec![0usize; text.len() + 1];
            let mut cell = 0usize;
            for (byte, ch) in text.char_indices() {
                v[byte] = cell;
                cell += ch.width().unwrap_or(0).max(1);
            }
            v
        };

        // Character at a given *cell*, for the fallback path below.
        let char_at_cell = |target: usize| -> Option<char> {
            use unicode_width::UnicodeWidthChar;
            let mut cell = 0usize;
            for ch in text.chars() {
                if cell == target {
                    return Some(ch);
                }
                cell += ch.width().unwrap_or(0).max(1);
            }
            None
        };

        for g in &glyphs {
            let cell = cell_of_byte.get(g.cluster as usize).copied().unwrap_or(0);
            let pen_x = cell as u32 * cm.cell_w;

            if g.glyph == 0 {
                // Glyph 0 is .notdef. Try system fallback for this character.
                let ch = char_at_cell(cell).unwrap_or('?');
                if let Some((fid, gid)) = fonts.glyph_for(ch, *style) {
                    let key = fonts.key(fid, gid);
                    if let Some(img) = fonts.rasterize(key) {
                        blit(&mut canvas, width, height, &img, pen_x, baseline_y);
                        continue;
                    }
                }
                missing.push(ch);
                continue;
            }

            let key = fonts.key(g.font, g.glyph);
            if let Some(img) = fonts.rasterize(key) {
                blit(&mut canvas, width, height, &img, pen_x, baseline_y);
            }
        }
    }

    if !missing.is_empty() {
        eprintln!("[font_dump] no glyph anywhere for: {missing:?}");
    }

    match image::save_buffer(&out, &canvas, width, height, image::ColorType::Rgba8) {
        Ok(()) => eprintln!("[font_dump] wrote {out} ({width}x{height})"),
        Err(e) => eprintln!("[font_dump] could not write {out}: {e}"),
    }
}

/// Composite a glyph onto the canvas at a pen position.
///
/// Coverage is treated as alpha over the existing pixel. This is *not*
/// gamma-correct — deliberately. The renderer does gamma in a shader, and doing
/// an approximation here would make the PNG disagree with the real output,
/// which defeats the purpose of a reference dump.
fn blit(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    img: &zest_font::GlyphImage,
    pen_x: u32,
    baseline_y: u32,
) {
    if img.is_empty() {
        return;
    }
    // `top` is measured up from the baseline, `left` right from the pen.
    let x0 = pen_x as i64 + i64::from(img.left);
    let y0 = i64::from(baseline_y) - i64::from(img.top);

    for gy in 0..img.height {
        let y = y0 + i64::from(gy);
        if y < 0 || y >= i64::from(canvas_h) {
            continue;
        }
        for gx in 0..img.width {
            let x = x0 + i64::from(gx);
            if x < 0 || x >= i64::from(canvas_w) {
                continue;
            }
            let dst = (((y as u32) * canvas_w + x as u32) * 4) as usize;

            let (src_rgb, alpha) = if img.is_color {
                let s = ((gy * img.width + gx) * 4) as usize;
                let Some(px) = img.data.get(s..s + 4) else { continue };
                ([px[0], px[1], px[2]], px[3])
            } else {
                let s = (gy * img.width + gx) as usize;
                let Some(&cov) = img.data.get(s) else { continue };
                ([0xd7, 0xdc, 0xea], cov)
            };
            if alpha == 0 {
                continue;
            }

            let a = f32::from(alpha) / 255.0;
            for c in 0..3 {
                let old = f32::from(canvas[dst + c]);
                let new = f32::from(src_rgb[c]);
                canvas[dst + c] = (old * (1.0 - a) + new * a) as u8;
            }
        }
    }
}

