//! The winit application: window, surface, and the frame loop.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use zest_font::{Fonts, Typography};
use zest_pty::{CommandSpec, PtySize};
use zest_render_wgpu::{Chrome, Renderer, Scene, Viewport};

use crate::input;
use crate::session::{Session, Wakeup};

/// Padding between the window edge and the grid, in logical pixels.
const PADDING: u32 = 8;

pub struct Config {
    pub font_families: Vec<String>,
    pub typography: Typography,
    pub theme: String,
    pub scrollback: usize,
    pub opacity: f32,
    pub shell: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_families: ["Cascadia Mono", "Consolas", "DejaVu Sans Mono", "monospace"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            typography: Typography { size_pt: 13.0, line_height: 1.25, ..Default::default() },
            theme: zest_theme::builtin::DEFAULT_DARK.to_string(),
            scrollback: 10_000,
            opacity: 1.0,
            shell: None,
        }
    }
}

/// The live GPU state, created once the window exists.
struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

pub struct App {
    config: Config,
    proxy: EventLoopProxy<Wakeup>,

    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    session: Option<Session>,
    fonts: Option<Fonts>,
    palette: zest_core::PaletteSnapshot,

    scene: Scene,
    modifiers: ModifiersState,
    focused: bool,
    /// Accumulated fractional wheel lines, so trackpads do not lose precision.
    scroll_accum: f32,
}

impl App {
    pub fn new(config: Config, proxy: EventLoopProxy<Wakeup>) -> Self {
        let theme = zest_theme::builtin::get(&config.theme)
            .unwrap_or_else(zest_theme::builtin::obsidian);
        let palette = to_core_palette(&zest_theme::resolve(&theme));

        Self {
            config,
            proxy,
            window: None,
            gpu: None,
            session: None,
            fonts: None,
            palette,
            scene: Scene::default(),
            modifiers: ModifiersState::empty(),
            focused: true,
            scroll_accum: 0.0,
        }
    }


    /// Rasterize printable ASCII in all four styles before the first frame.
    ///
    /// Roughly 380 glyphs and a couple of milliseconds. Without it, the first
    /// frame containing a prompt pays to rasterize every character in it, which
    /// lands as a visible hitch immediately after the window appears — and then
    /// again the first time anything bold or italic shows up.
    fn prewarm_atlas(&mut self) {
        let (Some(gpu), Some(fonts)) = (self.gpu.as_mut(), self.fonts.as_mut()) else {
            return;
        };
        let started = std::time::Instant::now();

        for style in [
            zest_font::Style::new(false, false),
            zest_font::Style::new(true, false),
            zest_font::Style::new(false, true),
            zest_font::Style::new(true, true),
        ] {
            for ch in ' '..='~' {
                let Some((font, glyph)) = fonts.glyph_for(ch, style) else { continue };
                let key = fonts.key(font, glyph);
                if gpu.renderer.atlas.get(&key).is_some() {
                    continue;
                }
                if let Some(image) = fonts.rasterize(key) {
                    gpu.renderer
                        .atlas
                        .insert(&gpu.device, &gpu.queue, key, &image);
                }
            }
        }

        tracing::debug!(
            glyphs = gpu.renderer.atlas.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "atlas pre-warmed"
        );
    }

    fn redraw(&mut self) {
        let (Some(gpu), Some(fonts), Some(session), Some(window)) = (
            self.gpu.as_mut(),
            self.fonts.as_mut(),
            self.session.as_ref(),
            self.window.as_ref(),
        ) else {
            return;
        };

        let metrics = fonts.cell_metrics();

        // Build the frame FIRST, and only then acquire the swapchain texture.
        //
        // `get_current_texture` blocks until the presentation engine hands one
        // over. Acquiring first would spend that wait doing nothing and then run
        // all the CPU work afterwards, pushing past the vblank deadline. This
        // ordering overlaps the CPU work with the wait and is the single
        // highest-leverage latency trick in the renderer.
        {
            let term = session.terminal.lock();
            self.scene.build(
                &gpu.device,
                &gpu.queue,
                &mut gpu.renderer.atlas,
                fonts,
                metrics,
                &[Viewport {
                    rect: [
                        PADDING as f32,
                        PADDING as f32,
                        (gpu.config.width.saturating_sub(PADDING * 2)) as f32,
                        (gpu.config.height.saturating_sub(PADDING * 2)) as f32,
                    ],
                    grid: term.grid(),
                    palette: term.palette(),
                    scroll_px: 0.0,
                    focused: self.focused,
                    opacity: self.config.opacity,
                }],
                &Chrome::default(),
            );
        } // lock released before any GPU work

        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                window.request_redraw();
                return;
            }
            other => {
                tracing::debug!(?other, "skipping frame");
                return;
            }
        };

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        gpu.renderer
            .render(&gpu.device, &gpu.queue, &mut encoder, &view, &self.scene);
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
    }

    fn resize_surface(&mut self, width: u32, height: u32) {
        let (Some(gpu), Some(session)) = (self.gpu.as_mut(), self.session.as_ref()) else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }

        // Clamp rather than fail. An oversized surface is a validation error
        // that would abort the process mid-drag; a clamped one just draws a
        // little short on an implausibly large window.
        let max = gpu.device.limits().max_texture_dimension_2d;
        let (width, height) = (width.min(max), height.min(max));

        gpu.config.width = width;
        gpu.config.height = height;
        gpu.surface.configure(&gpu.device, &gpu.config);
        gpu.renderer.resize(&gpu.device, width, height);

        if let Some(fonts) = self.fonts.as_ref() {
            let (cols, rows) = fonts.cell_metrics().grid_size(width, height, PADDING);
            session.resize(cols, rows);
        }
    }
}

impl ApplicationHandler<Wakeup> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let t0 = std::time::Instant::now();

        // Created HIDDEN, shown only once a real frame has been presented.
        //
        // A visible window shows the OS default background -- white on Windows --
        // for as long as startup takes, and startup is several hundred
        // milliseconds: adapter enumeration, device creation, shader
        // compilation, font resolution, then spawning a shell. Painting nothing
        // into a visible window is what produces the white flash; the fix is to
        // not be visible until there is something to show.
        let attrs = Window::default_attributes()
            .with_title("zesterm")
            .with_transparent(self.config.opacity < 1.0)
            .with_visible(false)
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));

        let scale = window.scale_factor() as f32;
        let typo = Typography { scale_factor: scale, ..self.config.typography };
        let fonts = Fonts::new(&self.config.font_families, typo).expect("no usable font");
        let metrics = fonts.cell_metrics();
        tracing::debug!(elapsed_ms = t0.elapsed().as_millis(), "fonts ready");

        // The surface is NOT sRGB (the resolve pass encodes), so the clear value
        // is written verbatim -- pass the theme background already in sRGB.
        let bg = self.palette.background;
        let clear = wgpu::Color {
            r: f64::from(bg.r) / 255.0,
            g: f64::from(bg.g) / 255.0,
            b: f64::from(bg.b) / 255.0,
            a: f64::from(self.config.opacity),
        };
        let gpu = pollster::block_on(init_gpu(&window, self.config.opacity < 1.0, clear));
        tracing::debug!(elapsed_ms = t0.elapsed().as_millis(), "gpu ready");
        let (cols, rows) = metrics.grid_size(gpu.config.width, gpu.config.height, PADDING);

        let proxy = self.proxy.clone();
        let mut spec = CommandSpec::default_shell();
        if let Some(shell) = &self.config.shell {
            spec.command_line = shell.clone();
        }
        // TERM is what programs consult to decide which escape sequences are
        // safe. Claiming xterm-256color is the conventional, widely-supported
        // answer until a `zesterm` terminfo exists.
        spec.env.push(("TERM".into(), "xterm-256color".into()));
        spec.env.push(("COLORTERM".into(), "truecolor".into()));

        let session = Session::spawn(
            &spec,
            PtySize::new(cols, rows),
            self.config.scrollback,
            move |w| {
                let _ = proxy.send_event(w);
            },
        )
        .expect("spawn shell");

        session.terminal.lock().set_palette(self.palette.clone());

        self.fonts = Some(fonts);
        self.gpu = Some(gpu);
        self.session = Some(session);
        self.window = Some(window);

        // The window is already visible and painted with the theme background
        // (see init_gpu). Present the first real frame on top of it.
        //
        // Pre-warming the atlas here matters too: without it the first frame
        // that actually contains text pays for rasterizing the whole prompt,
        // which lands as a visible hitch right after the window appears.
        self.prewarm_atlas();
        self.redraw();

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }

        tracing::info!(
            cols,
            rows,
            scale,
            startup_ms = t0.elapsed().as_millis(),
            "zesterm ready"
        );
    }

    /// A wakeup from the parser thread.
    fn user_event(&mut self, el: &ActiveEventLoop, event: Wakeup) {
        match event {
            Wakeup::Redraw => {
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Wakeup::Exited => el.exit(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::Resized(size) => self.resize_surface(size.width, size.height),

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // A DPI change invalidates every rasterized glyph, so bump the
                // atlas generation and recompute geometry. Doing this in two
                // steps would render a frame at the wrong size.
                if let Some(fonts) = self.fonts.as_mut() {
                    fonts.set_typography(Typography {
                        scale_factor: scale_factor as f32,
                        ..self.config.typography
                    });
                }
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.renderer.atlas.clear();
                }
                if let Some(w) = self.window.as_ref() {
                    let size = w.inner_size();
                    self.resize_surface(size.width, size.height);
                }
            }

            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if let Some(s) = self.session.as_ref() {
                    s.mark_dirty();
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),

            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let Some(session) = self.session.as_ref() else { return };

                let modes = session.terminal.lock().modes();
                if let Some(bytes) = input::encode(&event, self.modifiers, modes) {
                    // Written synchronously, before anything else. Deferring
                    // input to the next frame adds a whole frame of latency for
                    // nothing.
                    session.write(bytes);
                    // Typing scrolls back to the bottom, which is what every
                    // terminal does and what users expect.
                    session.terminal.lock().scroll_to_bottom();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let Some(session) = self.session.as_ref() else { return };
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // Trackpads report pixels. Convert with the cell height so
                    // the feel matches a wheel.
                    MouseScrollDelta::PixelDelta(p) => {
                        let ch = self.fonts.as_ref().map_or(20.0, |f| f.cell_metrics().cell_h as f32);
                        p.y as f32 / ch
                    }
                };
                self.scroll_accum += lines;
                let whole = self.scroll_accum.trunc();
                self.scroll_accum -= whole;
                if whole != 0.0 {
                    session.terminal.lock().scroll_display(whole as isize * 3);
                    session.mark_dirty();
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::RedrawRequested => {
                // Damage gates the frame entirely. An idle terminal must use 0%
                // GPU -- that is a hard requirement, and it is what separates a
                // real terminal from a demo.
                let dirty = self.session.as_ref().is_some_and(Session::take_dirty);
                if dirty {
                    self.redraw();
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // Wait for something to happen rather than polling. With no animations
        // yet there is nothing to schedule; the cursor blink will add a
        // `WaitUntil` here.
        el.set_control_flow(ControlFlow::Wait);
    }
}

async fn init_gpu(
    window: &Arc<Window>,
    want_transparency: bool,
    clear_color: wgpu::Color,
) -> Gpu {
    let t = std::time::Instant::now();

    // One backend at a time, preferred first.
    //
    // Probing several costs real startup latency -- initializing a Vulkan *and*
    // a DX12 instance, then enumerating adapters on both, was ~670ms of the
    // ~1.9s launch. `Backends::all()` is worse still, since it also spins up an
    // OpenGL stack we will never use.
    //
    // Vulkan leads on Windows because it is the only backend that reports
    // `PreMultiplied` alpha there (ADR-003); DX12 reports `Opaque` on every
    // adapter, so preferring it would silently cost transparency.
    let preferred: &[wgpu::Backends] = if cfg!(target_os = "macos") {
        &[wgpu::Backends::METAL]
    } else if cfg!(windows) {
        &[wgpu::Backends::VULKAN, wgpu::Backends::DX12]
    } else {
        &[wgpu::Backends::VULKAN, wgpu::Backends::GL]
    };

    let mut found = None;
    for &backends in preferred {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let Ok(surface) = instance.create_surface(Arc::clone(window)) else { continue };
        if let Ok(adapter) = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
        {
            found = Some((surface, adapter));
            break;
        }
        tracing::debug!(?backends, "no adapter; trying the next backend");
    }

    let (surface, adapter) = found.expect("no suitable GPU adapter on any backend");
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "adapter");
    tracing::info!(adapter = %adapter.get_info().name, backend = ?adapter.get_info().backend, "gpu");

    // Conservative limits everywhere except texture size.
    //
    // `downlevel_defaults` caps 2D textures at 2048, which is smaller than an
    // ordinary window on a modern display -- configuring the surface then fails
    // validation outright. Raise only the dimension limits to what the adapter
    // actually offers, and keep the conservative values for everything else so
    // the renderer stays runnable on weak hardware.
    let adapter_limits = adapter.limits();
    let limits = wgpu::Limits {
        max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
        max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
        max_texture_dimension_3d: adapter_limits.max_texture_dimension_3d,
        ..wgpu::Limits::downlevel_defaults()
    };
    let max_dim = limits.max_texture_dimension_2d;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("zesterm"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            ..Default::default()
        })
        .await
        .expect("request device");
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "device");

    let caps = surface.get_capabilities(&adapter);

    // A NON-sRGB format, deliberately. The resolve pass performs the sRGB
    // encode itself so that premultiplication happens in encoded space; an sRGB
    // surface would encode a second time and wash everything out. -> ADR-003.
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .unwrap_or_else(|| {
            tracing::warn!("no non-sRGB surface format; colours will be over-bright");
            caps.formats[0]
        });

    // Transparency is adapter-dependent on Windows: DX12 reports Opaque on every
    // adapter, and Vulkan only on some. Never silently ignore the setting.
    let alpha_mode = if want_transparency
        && caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        if want_transparency {
            tracing::warn!(
                available = ?caps.alpha_modes,
                "this adapter cannot composite per-pixel alpha; window opacity ignored"
            );
        }
        wgpu::CompositeAlphaMode::Opaque
    };

    let size = window.inner_size();
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: size.width.clamp(1, max_dim),
        height: size.height.clamp(1, max_dim),
        // Mailbox where available: no tearing, lower latency than Fifo because
        // it replaces the queued frame rather than queueing behind it.
        present_mode: if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        },
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: vec![],
    };
    surface.configure(&device, &config);
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "surface configured");

    // Show the window HERE, painted with the theme background, rather than
    // after the pipelines exist.
    //
    // Compiling four shader modules costs ~450ms, and none of it is needed to
    // put a correctly-coloured window on screen -- a render pass with a clear
    // load op needs no pipeline at all. Waiting for the pipelines delays the
    // window by that much for no visible benefit, and the alternative (showing
    // earlier without painting) is the white flash this is all fixing.
    clear_to(&device, &queue, &surface, clear_color);
    window.set_visible(true);
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "window shown");

    let mut renderer = Renderer::new(&device, format);
    renderer.resize(&device, config.width, config.height);
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "pipelines");

    Gpu { surface, device, queue, config, renderer }
}

/// Paint the surface a solid colour. Needs no pipeline, only a clear.
fn clear_to(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    surface: &wgpu::Surface<'static>,
    color: wgpu::Color,
) {
    let frame = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        _ => return,
    };
    let view = frame.texture.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("zest first paint"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(color),
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        ..Default::default()
    }));
    queue.submit([encoder.finish()]);
    queue.present(frame);
}

/// `zest-theme` and `zest-core` deliberately do not depend on each other, so the
/// app owns this conversion.
fn to_core_palette(r: &zest_theme::ResolvedPalette) -> zest_core::PaletteSnapshot {
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
