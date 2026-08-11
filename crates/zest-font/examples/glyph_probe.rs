//! Is the rasterizer sampling the outline the way the hinter assumed?
//!
//! swash does not let you choose a hinting *target*: it hard-codes
//! `HintingMode::Smooth { lcd_subpixel: Some(LcdLayout::Horizontal), .. }` and
//! exposes only `hint(bool)`. So hinting always means "grid-fit for a rasterizer
//! with three times the horizontal resolution", and pairing that with grayscale
//! coverage changes glyph *shapes* rather than merely softening them — which is
//! what #100 was, and what reads as "the text looks cut off".
//!
//! This reports a verdict rather than a picture, in the mould of
//! `zest-render-wgpu`'s `alpha_probe`: two mechanical measurements that a person
//! can disagree with by re-running rather than by squinting.
//!
//! ```text
//! cargo run -p zest-font --example glyph_probe
//! cargo run -p zest-font --example glyph_probe -- --px 12.5 --png probe.png
//! ```

use zest_font::{Fonts, GlyphFormat, GlyphImage, Style, TextAntialias, Typography};

/// Coverage as ASCII, so a shape is legible in a terminal.
fn ink(img: &GlyphImage, channel: usize, label: &str) {
    let stride = img.format.bytes_per_texel() as usize;
    println!("      {label}");
    for y in 0..img.height {
        let row: String = (0..img.width)
            .map(|x| {
                let i = (y as usize * img.width as usize + x as usize) * stride + channel;
                match img.data.get(i).copied().unwrap_or(0) {
                    0 => ' ',
                    1..=85 => '.',
                    86..=170 => '+',
                    _ => '#',
                }
            })
            .collect();
        println!("        |{row}|");
    }
}

/// Rows of the bitmap below the baseline — the overshoot, exactly.
fn below_baseline(img: &GlyphImage) -> i32 {
    img.height as i32 - img.top
}

/// The largest spread between the three channels anywhere in the bitmap.
fn chroma(img: &GlyphImage) -> u8 {
    if img.format != GlyphFormat::SubpixelMask {
        return 0;
    }
    img.data
        .chunks_exact(4)
        .map(|px| px[..3].iter().max().unwrap_or(&0) - px[..3].iter().min().unwrap_or(&0))
        .max()
        .unwrap_or(0)
}

fn render(fonts: &mut Fonts, ch: char) -> Option<GlyphImage> {
    let (font, gid) = fonts.glyph_for(ch, Style::default())?;
    let key = fonts.key(font, gid);
    fonts.rasterize(key)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    let families: Vec<String> = arg("--family").map(|f| vec![f]).unwrap_or_else(|| {
        ["Cascadia Mono", "Consolas", "DejaVu Sans Mono", "monospace"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    });
    // 12.5 is `UI_BODY`, which is where #100 was reported.
    let px: f32 = arg("--px").and_then(|s| s.parse().ok()).unwrap_or(12.5);
    let text = arg("--text").unwrap_or_else(|| "woceaxz".to_string());

    let Ok(mut fonts) = Fonts::new(&families, Typography::default()) else {
        eprintln!("[glyph_probe] no usable font; nothing to say");
        return;
    };
    fonts.set_ui_px(Some(px));

    println!("[glyph_probe] requested: {}", families.join(", "));
    println!("[glyph_probe] {px}px, {} faces", fonts.face_count());
    println!(
        "[glyph_probe] swash pins HintingMode::Smooth {{ lcd_subpixel: Some(Horizontal) }} \n\
         [glyph_probe] and exposes only hint(bool), so hinting cannot be softened -- only \n\
         [glyph_probe] matched, by sampling per channel, or declined.\n"
    );

    let mut worst_chroma = 0u8;
    let mut flat = [0i32; 2];

    for (i, mode) in [TextAntialias::Grayscale, TextAntialias::Subpixel].iter().enumerate() {
        fonts.set_text_antialias(*mode);
        println!("-- {mode:?} (hinting {}) --", if i == 0 { "on" } else { "off" });

        for ch in text.chars() {
            let Some(img) = render(&mut fonts, ch) else { continue };
            println!(
                "  {ch:?} {}x{} left={} top={} below_baseline={} chroma={}",
                img.width,
                img.height,
                img.left,
                img.top,
                below_baseline(&img),
                chroma(&img)
            );
            if ch == 'x' {
                flat[i] = below_baseline(&img);
            }
            worst_chroma = worst_chroma.max(chroma(&img));
        }

        // One glyph in full, because a number does not show a shape.
        if let Some(img) = render(&mut fonts, 'w') {
            match img.format {
                GlyphFormat::SubpixelMask => {
                    for (c, label) in [(0, "R (-0.3px)"), (1, "G (0)"), (2, "B (+0.3px)")] {
                        ink(&img, c, label);
                    }
                }
                _ => ink(&img, 0, "coverage"),
            }
        }
        println!();
    }

    // --- verdict -----------------------------------------------------------
    println!("=== VERDICT ===");

    let chroma_ok = worst_chroma >= 32;
    println!(
        "[1] CHROMA     {}  max spread {worst_chroma} between channels.\n    \
         {}",
        if chroma_ok { "PASS" } else { "FAIL" },
        if chroma_ok {
            "Subpixel sampling recovered horizontal detail the grayscale render collapses."
        } else {
            "The three channels came back near-identical: the mode is an expensive \
             identity and something upstream has stopped emitting Format::Subpixel."
        }
    );

    // Overshoot, measured against the flat-bottomed letters of the same face so
    // this needs no particular font installed.
    fonts.set_text_antialias(TextAntialias::Grayscale);
    let hinted: Vec<i32> =
        ['o', 'c', 'e'].iter().filter_map(|&c| render(&mut fonts, c)).map(|i| below_baseline(&i)).collect();
    fonts.set_text_antialias(TextAntialias::Subpixel);
    let unhinted: Vec<i32> =
        ['o', 'c', 'e'].iter().filter_map(|&c| render(&mut fonts, c)).map(|i| below_baseline(&i)).collect();

    let flattened = hinted.iter().all(|&v| v <= flat[0]);
    let restored = unhinted.iter().all(|&v| v > flat[1]);
    println!(
        "[2] OVERSHOOT  {}  hinted {hinted:?} vs 'x' {}, unhinted {unhinted:?} vs 'x' {}.\n    \
         {}",
        if restored { "PASS" } else { "FAIL" },
        flat[0],
        flat[1],
        if restored && flattened {
            "Grid-fitting flattens round letters onto the baseline while leaving 'a' \
             alone, which is why \"Close\" reads a pixel short beside \"tab\". Not \
             hinting restores it. Note this is a VERTICAL effect: subpixel sampling is \
             horizontal and cannot fix it on its own."
        } else if restored {
            "Round letters overshoot in the shipping mode, which is what matters."
        } else {
            "Round letters still sit flat on the baseline. The 'Close tab' half of #100 \
             is not fixed by whatever is currently configured."
        }
    );

    if let Some(out) = arg("--png") {
        let mut cells = Vec::new();
        for mode in [TextAntialias::Grayscale, TextAntialias::Subpixel] {
            fonts.set_text_antialias(mode);
            for ch in text.chars() {
                if let Some(img) = render(&mut fonts, ch) {
                    cells.push(img);
                }
            }
        }
        sheet(&cells, 12, &out);
    }
}

/// Every render, magnified, as one PNG.
///
/// The shapes in question are seven pixels tall; ASCII is enough to see *that*
/// two renders differ and nowhere near enough to judge which one is the letter
/// the designer drew.
fn sheet(cells: &[GlyphImage], zoom: u32, out: &str) {
    let gap = 4u32;
    let w: u32 = cells.iter().map(|c| c.width * zoom + gap).sum::<u32>() + gap;
    let h: u32 = cells.iter().map(|c| c.height).max().unwrap_or(1) * zoom + gap * 2;
    let mut buf = vec![0u8; (w * h * 3) as usize];

    let mut x0 = gap;
    for img in cells {
        let stride = img.format.bytes_per_texel() as usize;
        for y in 0..img.height * zoom {
            for x in 0..img.width * zoom {
                let (sx, sy) = ((x / zoom) as usize, (y / zoom) as usize);
                let i = (sy * img.width as usize + sx) * stride;
                let px = if stride == 1 {
                    [img.data[i]; 3]
                } else {
                    [img.data[i], img.data[i + 1], img.data[i + 2]]
                };
                let d = (((y + gap) * w + x0 + x) * 3) as usize;
                buf[d..d + 3].copy_from_slice(&px);
            }
        }
        x0 += img.width * zoom + gap;
    }
    match image::save_buffer(out, &buf, w, h, image::ColorType::Rgb8) {
        Ok(()) => println!("\nwrote {out} ({w}x{h})"),
        Err(e) => eprintln!("\n[glyph_probe] {out}: {e}"),
    }
}
