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

        let attrs = Window::default_attributes()
            .with_title("zesterm")
            .with_transparent(self.config.opacity < 1.0)
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));

        let scale = window.scale_factor() as f32;
        let typo = Typography { scale_factor: scale, ..self.config.typography };
        let fonts = Fonts::new(&self.config.font_families, typo).expect("no usable font");
        let metrics = fonts.cell_metrics();

        let gpu = pollster::block_on(init_gpu(&window, self.config.opacity < 1.0));
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

        tracing::info!(cols, rows, scale, "zesterm ready");

        self.fonts = Some(fonts);
        self.gpu = Some(gpu);
        self.session = Some(session);
        self.window = Some(window);
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

async fn init_gpu(window: &Arc<Window>, want_transparency: bool) -> Gpu {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let surface = instance
        .create_surface(Arc::clone(window))
        .expect("create surface");
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        })
        .await
        .expect("no suitable GPU adapter");
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

    let mut renderer = Renderer::new(&device, format);
    renderer.resize(&device, config.width, config.height);

    Gpu { surface, device, queue, config, renderer }
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
