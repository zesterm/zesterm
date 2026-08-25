//! Render a real terminal grid to a PNG, offscreen, with no window.
//!
//! The same discipline as `zest-font`'s `font_dump`: find bugs at the cheapest
//! layer. A wrong baseline, a mis-sampled atlas, inverted alpha or bad gamma are
//! all *visible*, and diagnosing them through a live window means first ruling
//! out winit, surface configuration and present modes.
//!
//! It doubles as the golden-image harness — this runs on a fallback adapter
//! (WARP on Windows, lavapipe on Linux), so it works in CI.
//!
//! ```text
//! cargo run -p zest-render-wgpu --example render_dump
//! cargo run -p zest-render-wgpu --example render_dump -- --preedit にほんご
//! cargo run -p zest-render-wgpu --example render_dump -- --padding 16
//! ```
//!
//! `--preedit` draws composing text over the cursor, which is otherwise only
//! reachable by holding down an input method in a live window — the one part of
//! IME that is a rendering question rather than an event-plumbing one.
//!
//! `--padding` insets the viewport inside a larger target the way the window's
//! `window.padding` does. Without it every pixel of the dump belongs to the
//! grid, so the band around it — the one place a missing backdrop shows up as a
//! black frame — is the one thing this tool could not reproduce.

use zest_core::Terminal;
use zest_font::{Fonts, Typography};
use zest_render_wgpu::{Chrome, LinearRgba, Preedit, Renderer, Scene, Viewport};

/// Non-sRGB on purpose: the resolve pass performs the sRGB encode itself, so an
/// sRGB target would encode twice and wash everything out.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const SAMPLE: &str = concat!(
    "\x1b[1mzesterm\x1b[0m — GPU terminal\r\n",
    "\r\n",
    "\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[33myellow\x1b[0m ",
    "\x1b[34mblue\x1b[0m \x1b[35mmagenta\x1b[0m \x1b[36mcyan\x1b[0m\r\n",
    "\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m ",
    "\x1b[9mstrike\x1b[0m \x1b[2mdim\x1b[0m \x1b[7minverse\x1b[0m\r\n",
    "\x1b[4:3mundercurl\x1b[0m \x1b[21mdouble\x1b[0m\r\n",
    "\x1b[38;2;255;128;0mtruecolor\x1b[0m \x1b[48;5;24m 256-indexed bg \x1b[0m\r\n",
    "CJK 世界 emoji 🚀 combining e\u{0301}\r\n",
    "$ cargo build --release\r\n",
);

fn main() {
    pollster::block_on(run());
}

async fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let preedit = args
        .iter()
        .position(|a| a == "--preedit")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    // Any adapter will do, including a software one -- which is what makes this
    // runnable in CI.
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .expect("no GPU adapter, not even a fallback");
    eprintln!("[render_dump] adapter: {:?}", adapter.get_info().name);

    // Asked for only when offered, so the dumper still runs on a fallback
    // adapter that has no dual-source blending.
    let dual = adapter.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING);
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("zest render_dump"),
            required_features: if dual {
                wgpu::Features::DUAL_SOURCE_BLENDING
            } else {
                wgpu::Features::empty()
            },
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        })
        .await
        .expect("request device");

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        argv.iter().position(|a| a == name).and_then(|i| argv.get(i + 1)).cloned()
    };

    // --- fonts ---
    let families: Vec<String> = match opt("--family") {
        Some(f) => vec![f],
        None => ["Cascadia Mono", "Consolas", "DejaVu Sans Mono", "monospace"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let size_pt = opt("--size").map_or(16.0, |v| v.parse::<f32>().expect("--size takes points"));
    let typo = Typography { size_pt, line_height: 1.25, ..Default::default() };
    let mut fonts = Fonts::new(&families, typo).expect("no usable font");
    let metrics = fonts.cell_metrics();

    // --- terminal ---
    //
    // `--replay` takes a raw capture from `pty_dump` or a fixture, which is how
    // a bug seen in the live window gets reproduced without one. Sized wider,
    // because real captures are written for a real terminal's width.
    let replay = opt("--replay").map(|p| std::fs::read(&p).expect("read --replay file"));
    let (cols, rows) = if replay.is_some() { (100usize, 6usize) } else { (46usize, 10usize) };
    let mut term = Terminal::new(cols, rows, 100);
    match &replay {
        Some(bytes) => term.advance(bytes),
        None => term.advance(SAMPLE.as_bytes()),
    }

    // Seed the palette from a real theme, so this exercises the same path the
    // app will use rather than the terminal's built-in defaults.
    let theme = zest_theme::builtin::obsidian();
    let resolved = zest_theme::resolve(&theme);
    term.set_palette(to_core_palette(&resolved));

    let padding = opt("--padding").map_or(0, |p| p.parse::<u32>().expect("--padding takes pixels"));
    let width = cols as u32 * metrics.cell_w + padding * 2;
    let height = rows as u32 * metrics.cell_h + padding * 2;
    eprintln!(
        "[render_dump] cell {}x{}, padding {padding}, target {width}x{height}",
        metrics.cell_w, metrics.cell_h
    );

    // --- render ---
    let antialias = if argv.iter().any(|a| a == "--grayscale") {
        zest_font::TextAntialias::Grayscale
    } else {
        zest_font::TextAntialias::Subpixel
    };
    let mut renderer = Renderer::new(&device, TARGET_FORMAT, antialias);
    // The renderer decides, not the flag: a fallback adapter may have no
    // dual-source blending. Left unsynced, the rasterizer would emit four-byte
    // subpixel masks into a one-byte texture.
    fonts.set_text_antialias(renderer.text_antialias());
    eprintln!("[render_dump] antialias: {:?}", renderer.text_antialias());
    renderer.resize(&device, width, height);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("render target"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&Default::default());

    let mut scene = Scene::default();
    scene.build(
        &device,
        &queue,
        &mut renderer.atlas,
        &mut fonts,
        metrics,
        // The same background the app would clear the window to, so a dump is
        // comparable with a screenshot rather than merely similar to one.
        {
            let bg = term.palette().background;
            LinearRgba::from_srgb(bg.r, bg.g, bg.b, 1.0)
        },
        &[Viewport {
            features: &[],
            ligatures: false,
            cursor_shape: zest_core::CursorShape::Block,
            cursor_offset: [0.0, 0.0],
            rect: [
                padding as f32,
                padding as f32,
                (width - padding * 2) as f32,
                (height - padding * 2) as f32,
            ],
            grid: term.grid(),
            palette: term.palette(),
            scroll_px: 0.0,
            focused: true,
            opacity: 1.0,
            // No picture either: a wallpaper is a window-level choice, and this
            // dump exists to answer questions about the grid without one.
            background: None,
            // No block decoration: this dump answers "is the *renderer* wrong",
            // one layer below anything that knows what a command block is.
            blocks: &[],
            gutter: 0.0,
            scale: 1.0,
            // Select part of the SGR line so the highlight is exercised too.
            selection: term.abs_pos(3, 0).zip(term.abs_pos(3, 18)).map(|(a, b)| {
                zest_core::Selection { anchor: a, head: b, mode: zest_core::SelectionMode::Simple }
            }),
            selection_bg: to_core_palette(&resolved).colors[8],
            preedit: preedit.as_deref().map(|text| Preedit {
                // Mid-string, so the caret is drawn between characters rather
                // than only at an end -- the case that is easy to get wrong.
                cursor: Some((text.len() / 2, text.len() / 2)),
                text,
            }),
            predicted: None,
            cursor_on: true,
            row_map: None,
        }],
        &Chrome::default(),
    );
    eprintln!(
        "[render_dump] {} rects, {} glyphs, {} decorations, {} atlas entries",
        scene.rects.len(),
        scene.glyphs.len(),
        scene.decors.len(),
        renderer.atlas.len()
    );

    let mut encoder = device.create_command_encoder(&Default::default());
    renderer.render(&device, &queue, &mut encoder, &target_view, &scene);
    queue.submit([encoder.finish()]);

    let pixels =
        zest_render_wgpu::read_rgba(&device, &queue, &target, width, height, TARGET_FORMAT);
    let out = std::env::args()
        .skip_while(|a| a != "--out")
        .nth(1)
        .unwrap_or_else(|| "render_dump.png".into());
    match image::save_buffer(&out, &pixels, width, height, image::ColorType::Rgba8) {
        Ok(()) => eprintln!("[render_dump] wrote {out}"),
        Err(e) => eprintln!("[render_dump] could not write {out}: {e}"),
    }
}

fn to_core_palette(r: &zest_theme::ResolvedPalette) -> zest_core::PaletteSnapshot {
    // The two crates deliberately do not depend on each other, so the app owns
    // this conversion. It is a dozen lines and keeps both independently usable.
    let conv = |c: zest_theme::Rgba8| zest_core::Rgb::new(c.r, c.g, c.b);
    let mut colors = [zest_core::Rgb::default(); 256];
    for (i, c) in r.colors.iter().enumerate() {
        colors[i] = conv(*c);
    }
    zest_core::PaletteSnapshot {
        colors,
        foreground: conv(r.foreground),
        background: conv(r.background),
        cursor: conv(r.cursor),
    }
}
