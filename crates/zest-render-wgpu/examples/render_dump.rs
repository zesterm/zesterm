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
//! ```

use zest_core::Terminal;
use zest_font::{Fonts, Typography};
use zest_render_wgpu::{Chrome, Renderer, Scene, Viewport};

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

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("zest render_dump"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        })
        .await
        .expect("request device");

    // --- fonts ---
    let families: Vec<String> = ["Cascadia Mono", "Consolas", "DejaVu Sans Mono", "monospace"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let typo = Typography { size_pt: 16.0, line_height: 1.25, ..Default::default() };
    let mut fonts = Fonts::new(&families, typo).expect("no usable font");
    let metrics = fonts.cell_metrics();

    // --- terminal ---
    let cols = 46usize;
    let rows = 10usize;
    let mut term = Terminal::new(cols, rows, 100);
    term.advance(SAMPLE.as_bytes());

    // Seed the palette from a real theme, so this exercises the same path the
    // app will use rather than the terminal's built-in defaults.
    let theme = zest_theme::builtin::obsidian();
    let resolved = zest_theme::resolve(&theme);
    term.set_palette(to_core_palette(&resolved));

    let width = cols as u32 * metrics.cell_w;
    let height = rows as u32 * metrics.cell_h;
    eprintln!(
        "[render_dump] cell {}x{}, target {width}x{height}",
        metrics.cell_w, metrics.cell_h
    );

    // --- render ---
    let mut renderer = Renderer::new(&device, TARGET_FORMAT);
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
        &[Viewport {
            rect: [0.0, 0.0, width as f32, height as f32],
            grid: term.grid(),
            palette: term.palette(),
            scroll_px: 0.0,
            focused: true,
            opacity: 1.0,
            // Select part of the SGR line so the highlight is exercised too.
            selection: term.abs_pos(3, 0).zip(term.abs_pos(3, 18)).map(|(a, b)| {
                zest_core::Selection { anchor: a, head: b, mode: zest_core::SelectionMode::Simple }
            }),
            selection_bg: to_core_palette(&resolved).colors[8],
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

    let pixels = read_back(&device, &queue, &target, width, height).await;
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

/// Copy the render target back to the CPU.
async fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    // Rows in a mapped buffer must be aligned to 256 bytes, so the readback is
    // padded and then compacted.
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .expect("poll");
    rx.recv().expect("map").expect("map failed");

    let view = slice.get_mapped_range().expect("mapped range");
    let mut out = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        out.extend_from_slice(&view[start..start + unpadded as usize]);
    }
    drop(view);
    buffer.unmap();
    out
}
