//! The GPU surface: bringing one up, reconfiguring it, and reading it back.
//!
//! Split out of `app/mod.rs` (#554) as *code only* -- `struct Gpu` itself stays
//! on the parent beside `App`. A child module can see an ancestor's private
//! fields and a parent cannot see a child's, so leaving the state where it is
//! is what makes this a move with no visibility change anywhere in the crate.
//!
//! The ladder in `init_gpu` is the load-bearing part: a backend named there but
//! not compiled in enumerates zero adapters and reads exactly like a missing
//! driver (#468), which is why the panic names the tried set beside
//! `enabled_backend_features()` and why
//! `every_advertised_backend_is_compiled_in` holds the list against the
//! manifests rather than against a comment.

use super::*;

impl App {
    /// Make the window and its swapchain agree with the opacity settings.
    ///
    /// Three things decide whether a translucent window is actually
    /// translucent, and all three were settled once at startup and never asked
    /// again — which is why `window.opacity` was classed `SurfaceRebuild`,
    /// carried no "applies on next launch" tag, and did nothing at all until a
    /// relaunch:
    ///
    /// 1. The window's own `transparent` attribute. winit can change this after
    ///    creation on macOS, Windows **and Wayland**, where it updates the
    ///    surface's opaque region; **X11 can only take it at build time** —
    ///    `set_transparent` is an empty function there — so on X11 alone it is
    ///    logged rather than pretended.
    /// 2. The surface's `alpha_mode`. It is an ordinary field on the
    ///    configuration the app already owns, and the capability to decide it
    ///    with is now kept on [`Gpu`] rather than dropped inside `init_gpu`.
    /// 3. Antialiasing: subpixel coverage against a translucent destination is
    ///    undefined, so [`App::antialias_for`] forces grayscale below 1.0 and
    ///    the atlas has to be rebuilt when that flips.
    ///
    /// The caller re-configures the surface afterwards; this only decides.
    ///
    /// Returns immediately when the *intent* has not moved. `SurfaceRebuild` is
    /// shared with `window.backdrop`, so this runs on backdrop-only reloads
    /// too, and everything below is either a no-op or a log — which would make
    /// changing the backdrop on an adapter that cannot do alpha reprint the
    /// fallback warning every time. Gated on the remembered intent rather than
    /// on which keys changed: a key list here would be a second table to keep
    /// in sync with `invalidate::KEYS`, and this cannot fall out of step
    /// because it compares the thing it actually acts on.
    pub(super) fn apply_transparency(&mut self) {
        let want = self.config.translucent_surface();
        let Some(w) = self.window.as_ref() else { return };
        if self.gpu.as_ref().is_some_and(|g| g.transparent == want) {
            return;
        }

        // Said only when it changes, and only where it cannot work: a setting
        // that silently does nothing is the bug this whole sweep is closing,
        // but a line repeated on every unrelated reload is how a log stops
        // being read at all.
        // The session, not the `cfg`: this used to fire for every non-macOS
        // unix and say "on X11", which is wrong on Wayland -- where
        // `set_transparent` really does update the surface's opaque region and
        // the change takes effect now. On X11 winit's `set_transparent` is an
        // empty function and the ARGB visual is fixed when the window is
        // built, so there the relaunch is real.
        if self.session == crate::platform::Session::X11 {
            tracing::info!(
                "window.opacity and window.chrome_opacity apply at the next \
                 launch on X11: the visual carrying the alpha is chosen when \
                 the window is created"
            );
        }
        w.set_transparent(want);

        let Some(gpu) = self.gpu.as_mut() else { return };
        gpu.transparent = want;
        let alpha_mode = alpha_mode_for(want, &gpu.alpha_modes);
        if gpu.config.alpha_mode == alpha_mode {
            return;
        }
        gpu.config.alpha_mode = alpha_mode;
        // The atlas holds glyphs rasterized for the *old* answer: grayscale and
        // subpixel masks are different widths per texel, so keeping them would
        // be three columns of garbage rather than merely stale text.
        // `rebuild_fonts` owns that whole sequence and re-configures the
        // surface at the end, which is also what this change needs.
        self.rebuild_fonts();
    }

    pub(super) fn resize_surface(&mut self, width: u32, height: u32) {
        let insets = self.insets();
        let Some(gpu) = self.gpu.as_mut() else { return };
        if self.tabs.is_empty() || width == 0 || height == 0 {
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

        // Every rectangle in the chrome depends on the window size — and on
        // macOS a fullscreen transition arrives as a resize, which is also
        // when the traffic-light inset changes.
        self.chrome_layout = None;
        self.chrome_dirty = true;

        if let Some(fonts) = self.fonts.as_ref() {
            let dims = insets.grid_dims(fonts.cell_metrics(), width, height);
            // Only the visible grid follows a drag live; background tabs
            // catch up on activation, so a resize costs one message rather
            // than one per tab per frame. A split tab resizes both panes.
            if self.tabs.active().is_some_and(Tab::is_split) {
                self.resize_split_panes();
            } else if let Some(tab) = self.tabs.active_mut() {
                tab.source().resize(dims.0, dims.1);
                tab.sized = dims;
            }
        }

        // Draw a frame at the new size, now.
        //
        // Nothing else will. A frame is only drawn when the terminal is dirty,
        // and a resize does not touch the grid -- so on a quiet session the
        // last frame stays on screen while the surface underneath it changes
        // shape, and the compositor stretches it to fit. Dragging an edge then
        // scales the text continuously instead of re-laying it out, which reads
        // as the font resizing with the window.
        //
        // Marking dirty rather than drawing inline keeps the single render path
        // intact: this is the same "something changed" signal the parser sends,
        // and it coalesces the same way when a drag produces a hundred of them.
        if let Some(session) = self.tabs.active_source() {
            session.mark_dirty();
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

/// Render `scene` into a texture of our own and write it out as a PNG.
///
/// Returns the process exit code: a screenshot that silently did not happen is
/// the failure mode worth spending an exit code on, because the caller is
/// usually a script that goes on to read the file.
///
/// The texture takes the *surface's* format rather than a convenient one — the
/// render pipelines were built for it, and matching it is what makes this the
/// same frame the window would have shown rather than a re-render under
/// different rules.
pub(super) fn capture_frame(gpu: &mut Gpu, scene: &zest_render_wgpu::Scene, path: &std::path::Path) -> u8 {
    // Checked here rather than left to `read_rgba`'s assertion, because this is
    // not a programmer error: the surface format is whatever the adapter
    // offered (the first non-sRGB entry in `caps.formats`), and an HDR or
    // 10-bit display can hand back one this cannot encode. The library keeps
    // its invariant; the app owes the user a sentence and an exit code rather
    // than a panic and a backtrace.
    if zest_render_wgpu::capture::channel_swap(gpu.config.format).is_none() {
        eprintln!(
            "[screenshot] this adapter's surface is {:?}, which is not 8-bit RGBA or BGRA \
             and cannot be written as a PNG. Nothing was captured.",
            gpu.config.format
        );
        return 1;
    }

    let (width, height) = (gpu.config.width, gpu.config.height);
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("zest screenshot"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    gpu.renderer.render(&gpu.device, &gpu.queue, &mut encoder, &view, scene);
    gpu.queue.submit([encoder.finish()]);

    let pixels = zest_render_wgpu::read_rgba(
        &gpu.device,
        &gpu.queue,
        &texture,
        width,
        height,
        gpu.config.format,
    );
    // PNG explicitly, not inferred from the extension. `save_buffer` picks the
    // encoder from the path, so `--screenshot shot.jpg` would quietly write a
    // JPEG -- lossy, which for a screenshot used to compare exact pixels is a
    // wrong answer rather than a different one -- and an extensionless path
    // would fail outright. The flag says PNG, so it writes PNG.
    match image::save_buffer_with_format(
        path,
        &pixels,
        width,
        height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    ) {
        Ok(()) => {
            println!("[screenshot] {width}x{height} -> {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("[screenshot] could not write {}: {e}", path.display());
            1
        }
    }
}

/// The backends to try, in order, on this platform.
///
/// A free function so a test can hold it against
/// [`wgpu::Instance::enabled_backend_features`]: a rung naming a backend whose
/// wgpu feature is not compiled in enumerates zero adapters no matter what
/// drivers the machine has, which is how Linux advertised a GL fallback that
/// could only ever panic (#468). The list and the manifests have to move
/// together, and nothing but a test makes them.
fn preferred_backends() -> &'static [wgpu::Backends] {
    if cfg!(target_os = "macos") {
        &[wgpu::Backends::METAL]
    } else if cfg!(windows) {
        &[wgpu::Backends::VULKAN, wgpu::Backends::DX12]
    } else {
        &[wgpu::Backends::VULKAN, wgpu::Backends::GL]
    }
}

/// How the compositor should treat this surface's alpha.
///
/// Transparency is adapter-dependent on Windows: DX12 reports `Opaque` on every
/// adapter, and Vulkan only on some (ADR-003). Never silently ignore the
/// setting — a window that stays opaque because the hardware cannot do better
/// is a fact worth logging, and one that stays opaque because nobody asked the
/// question again is a bug.
///
/// Free-standing because this decision is now made twice: once at startup, and
/// again whenever `window.opacity` changes on a live window. Two copies of it
/// would be two chances to disagree about what the adapter can do.
pub(super) fn alpha_mode_for(
    want_transparency: bool,
    supported: &[wgpu::CompositeAlphaMode],
) -> wgpu::CompositeAlphaMode {
    if !want_transparency {
        return wgpu::CompositeAlphaMode::Opaque;
    }
    if supported.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
        return wgpu::CompositeAlphaMode::PreMultiplied;
    }
    // **On macOS `PostMultiplied` is the only transparent mode there is, and it
    // does not mean what it says.** wgpu's Metal backend advertises exactly
    // `[Opaque, PostMultiplied]` and implements the second as
    // `CAMetalLayer::setOpaque(false)` and nothing else
    // (`wgpu-hal/src/metal/surface.rs`); CoreAnimation then composites the
    // layer's contents as *premultiplied*, because that is what CA has always
    // done. So it is the mode this renderer's premultiplied output wants, under
    // a name that describes another API's behaviour.
    //
    // Requiring `PreMultiplied` therefore made `window.opacity` fall back to
    // `Opaque` on every Mac, with the "this adapter cannot composite per-pixel
    // alpha" warning naming the hardware for a decision that was ours — and it
    // takes `window.backdrop`'s vibrancy down with it, since a backdrop is only
    // visible through pixels the surface leaves transparent.
    //
    // Not accepted anywhere else, deliberately. On Vulkan
    // `VK_COMPOSITE_ALPHA_POST_MULTIPLIED_BIT_KHR` means what it says — the
    // compositor multiplies by alpha itself — so handing it premultiplied
    // colour double-darkens every translucent pixel. Same word, opposite
    // requirement, which is why this is a `cfg!` and not a second `contains`.
    if cfg!(target_os = "macos")
        && supported.contains(&wgpu::CompositeAlphaMode::PostMultiplied)
    {
        return wgpu::CompositeAlphaMode::PostMultiplied;
    }
    tracing::warn!(
        available = ?supported,
        "this adapter cannot composite per-pixel alpha; window opacity ignored"
    );
    wgpu::CompositeAlphaMode::Opaque
}

/// The GPU every window of this process draws with (#505): one instance,
/// adapter, device and queue, and one pipeline cache. What is per window —
/// the surface, its configuration, the renderer and its atlas — is made by
/// [`GpuHost::surface_for`]. The renderer stays per window because `Fonts`
/// is per scale factor and antialias-coupled (`sync_antialias`), and an
/// atlas shared across windows on different monitors would be cleared by
/// either one's DPI change.
pub(crate) struct GpuHost {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    cache: Option<wgpu::PipelineCache>,
    info: wgpu::AdapterInfo,
    max_dim: u32,
    /// How much of the cache was on disk last time it was written, so a
    /// window that compiled nothing new does not rewrite the file.
    cache_len: std::cell::Cell<usize>,
}

impl GpuHost {
    /// Bring the device up against the first window, whose surface chose
    /// the adapter — returned so that window does not create it twice.
    /// `None` is the ladder finding no adapter at all, which the private
    /// path then reports with its full diagnosis.
    pub(super) async fn new(window: &Arc<Window>) -> Option<(Self, wgpu::Surface<'static>)> {
        let t = std::time::Instant::now();
        let (instance, surface, adapter) = pick_adapter(window).await?;
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "adapter");
        tracing::info!(adapter = %adapter.get_info().name, backend = ?adapter.get_info().backend, "gpu");

        let want_cache = adapter.features().contains(wgpu::Features::PIPELINE_CACHE);
        let mut features = wgpu::Features::empty();
        if want_cache {
            features |= wgpu::Features::PIPELINE_CACHE;
        }
        if adapter.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING) {
            features |= wgpu::Features::DUAL_SOURCE_BLENDING;
        }
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
                required_features: features,
                required_limits: limits,
                ..Default::default()
            })
            .await
            .expect("request device");
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "device");

        let info = adapter.get_info();
        let cached = pipeline_cache::load(&info);
        let cache_len = std::cell::Cell::new(cached.as_ref().map_or(0, Vec::len));
        let cache = pipeline_cache::create(&device, want_cache, cached.as_deref());
        Some((Self { instance, adapter, device, queue, cache, info, max_dim, cache_len }, surface))
    }

    /// A surface, its configuration and a renderer for `window` on the
    /// shared device. `None` when the shared adapter cannot present to this
    /// window — the caller then opens a private device instead.
    pub(super) fn surface_for(
        &self,
        window: &Arc<Window>,
        surface: Option<wgpu::Surface<'static>>,
        want_transparency: bool,
        clear_color: wgpu::Color,
        antialias: zest_font::TextAntialias,
    ) -> Option<Gpu> {
        let t = std::time::Instant::now();
        let surface = match surface {
            Some(s) => s,
            None => self.instance.create_surface(Arc::clone(window)).ok()?,
        };
        if !self.adapter.is_surface_supported(&surface) {
            tracing::info!("this window's surface is not on the shared adapter; opening a private device");
            return None;
        }
        let caps = surface.get_capabilities(&self.adapter);
        let config = surface_config(&caps, want_transparency, window.inner_size(), self.max_dim);
        surface.configure(&self.device, &config);
        clear_to(&self.device, &self.queue, &surface, clear_color);
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "surface painted");

        let mut renderer =
            Renderer::with_cache(&self.device, config.format, self.cache.as_ref(), antialias);
        renderer.resize(&self.device, config.width, config.height);
        if let Some(cache) = self.cache.as_ref() {
            // Saved by whichever window compiled something new; `save`
            // itself skips a cache that did not grow.
            self.cache_len.set(pipeline_cache::save(cache, &self.info, self.cache_len.get()));
        }
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "pipelines");

        Some(Gpu {
            surface,
            device: self.device.clone(),
            queue: self.queue.clone(),
            config,
            renderer,
            transparent: want_transparency,
            alpha_modes: caps.alpha_modes.clone(),
        })
    }
}

/// Walk the backend ladder until one produces an adapter for `window`.
///
/// One backend at a time, preferred first. Probing several costs real
/// startup latency -- initializing a Vulkan *and* a DX12 instance, then
/// enumerating adapters on both, was ~670ms of the ~1.9s launch.
/// `Backends::all()` is worse still, since it also spins up an OpenGL stack
/// we will never use. Vulkan leads on Windows because it is the only backend
/// that reports `PreMultiplied` alpha there (ADR-003); DX12 reports `Opaque`
/// on every adapter, so preferring it would silently cost transparency.
async fn pick_adapter(
    window: &Arc<Window>,
) -> Option<(wgpu::Instance, wgpu::Surface<'static>, wgpu::Adapter)> {
    let preferred: &[wgpu::Backends] = preferred_backends();
    // `ZESTERM_BACKEND=dx12|vulkan|gl` forces one, for measuring.
    let forced = std::env::var("ZESTERM_BACKEND").ok().and_then(|s| {
        match s.to_ascii_lowercase().as_str() {
            "dx12" => Some(wgpu::Backends::DX12),
            "vulkan" => Some(wgpu::Backends::VULKAN),
            "gl" => Some(wgpu::Backends::GL),
            _ => None,
        }
    });
    let forced_list = forced.map(|b| vec![b]);
    let preferred: &[wgpu::Backends] = forced_list.as_deref().unwrap_or(preferred);

    for &backends in preferred {
        let t_inst = std::time::Instant::now();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        tracing::debug!(?backends, ms = t_inst.elapsed().as_millis(), "instance created");
        let Ok(surface) = instance.create_surface(Arc::clone(window)) else { continue };
        tracing::debug!(ms = t_inst.elapsed().as_millis(), "surface created");
        if let Ok(adapter) = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
        {
            return Some((instance, surface, adapter));
        }
        tracing::debug!(?backends, "no adapter; trying the next backend");
    }
    None
}

/// The one surface configuration rule, for the shared and the private path.
fn surface_config(
    caps: &wgpu::SurfaceCapabilities,
    want_transparency: bool,
    size: winit::dpi::PhysicalSize<u32>,
    max_dim: u32,
) -> wgpu::SurfaceConfiguration {
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
    let alpha_mode = alpha_mode_for(want_transparency, &caps.alpha_modes);
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: size.width.clamp(1, max_dim),
        height: size.height.clamp(1, max_dim),
        present_mode: if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        },
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: vec![],
    }
}

/// A device of this window's own: the fallback when the shared adapter
/// cannot present to it, and the path that reports "no adapter" in full.
pub(super) async fn init_gpu(
    window: &Arc<Window>,
    want_transparency: bool,
    clear_color: wgpu::Color,
    antialias: zest_font::TextAntialias,
) -> Gpu {
    let t = std::time::Instant::now();

    let found = pick_adapter(window).await.map(|(_, surface, adapter)| (surface, adapter));

    let Some((surface, adapter)) = found else {
        let preferred = preferred_backends();
        // Not `expect`: "no suitable GPU adapter" is the most common way for
        // this to fail on Linux and the bare message told the user nothing
        // they could act on -- not which backends were tried, and above all
        // not that a backend can be *listed and absent*, which is exactly what
        // #468 was. Naming the compiled set beside the tried set is what makes
        // a missing driver tell itself apart from a missing cargo feature.
        let compiled = wgpu::Instance::enabled_backend_features();
        // The driver advice is per-platform because the panic is not: a
        // headless CI runner reaches it on every OS, and Arch package names
        // are noise on a Mac.
        let advice = if cfg!(target_os = "macos") {
            "Metal is the only backend here; a machine that cannot provide it \
             is usually a VM or a session with no window server."
        } else if cfg!(windows) {
            "Install or update the GPU driver; `ZESTERM_BACKEND=vulkan|dx12` \
             forces a single backend."
        } else {
            "Install a Vulkan ICD (mesa's `vulkan-radeon` / `vulkan-intel` / \
             `nvidia-utils`, or `vulkan-swrast` for a software one), or a GL \
             driver for the GL rung; `ZESTERM_BACKEND=vulkan|gl` forces a \
             single backend."
        };
        panic!(
            "no suitable GPU adapter.\n\
             tried, in order: {preferred:?}\n\
             compiled into this binary: {compiled:?}\n\
             A backend listed above but missing from the compiled set can never \
             produce an adapter -- that is a build configuration bug, not a \
             driver problem. Otherwise this machine has no driver for any of \
             them. {advice}"
        );
    };
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "adapter");
    tracing::info!(adapter = %adapter.get_info().name, backend = ?adapter.get_info().backend, "gpu");

    // Conservative limits everywhere except texture size.
    //
    // `downlevel_defaults` caps 2D textures at 2048, which is smaller than an
    // ordinary window on a modern display -- configuring the surface then fails
    // validation outright. Raise only the dimension limits to what the adapter
    // actually offers, and keep the conservative values for everything else so
    // the renderer stays runnable on weak hardware.
    // Pipeline caching removes most of the ~450ms spent creating pipelines on a
    // cold start. Requested only when the adapter offers it, so a machine
    // without it simply pays the old cost.
    let want_cache = adapter.features().contains(wgpu::Features::PIPELINE_CACHE);
    let mut features = wgpu::Features::empty();
    if want_cache {
        features |= wgpu::Features::PIPELINE_CACHE;
    }
    // Subpixel text blends three coverages against the destination, which needs
    // a per-channel destination factor. DX12 (including WARP), Vulkan where the
    // adapter reports dualSrcBlend, and Metal all have it; asked for only when
    // offered, so a device without it starts normally and the renderer falls
    // back to grayscale.
    if adapter.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING) {
        features |= wgpu::Features::DUAL_SOURCE_BLENDING;
    }

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
            required_features: features,
            required_limits: limits,
            ..Default::default()
        })
        .await
        .expect("request device");
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "device");

    let caps = surface.get_capabilities(&adapter);
    let config = surface_config(&caps, want_transparency, window.inner_size(), max_dim);
    surface.configure(&device, &config);
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "surface configured");

    // Paint the surface the theme colour before the pipelines exist, so the
    // handover from the OS-painted background is seamless. A clear needs no
    // pipeline, so this costs nothing and avoids a flicker at the moment the
    // swapchain starts covering the window.
    clear_to(&device, &queue, &surface, clear_color);
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "first gpu paint");

    let info = adapter.get_info();
    let cached = pipeline_cache::load(&info);
    let previous_len = cached.as_ref().map_or(0, Vec::len);
    let cache = pipeline_cache::create(&device, want_cache, cached.as_deref());

    let mut renderer =
        Renderer::with_cache(&device, config.format, cache.as_ref(), antialias);
    renderer.resize(&device, config.width, config.height);
    tracing::debug!(
        elapsed_ms = t.elapsed().as_millis(),
        cached = cache.is_some(),
        "pipelines"
    );

    // Saved after the pipelines exist, so the blob contains what was just
    // compiled. Only writes when something new was added.
    if let Some(cache) = cache.as_ref() {
        let _ = pipeline_cache::save(cache, &info, previous_len);
    }

    Gpu {
        surface,
        device,
        queue,
        config,
        renderer,
        transparent: want_transparency,
        alpha_modes: caps.alpha_modes.clone(),
    }
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

#[cfg(test)]
mod backend_ladder_tests {
    use super::preferred_backends;

    /// A rung naming a backend that was never compiled in enumerates zero
    /// adapters however many drivers the machine has -- so it is not a
    /// fallback, it is a panic with extra steps. That is what shipped on Linux
    /// (#468): `init_gpu` offered `[VULKAN, GL]` while `zest-render-wgpu`
    /// enabled only `vulkan` for unix, making `ZESTERM_BACKEND=gl` an option
    /// that could only ever fail.
    ///
    /// The list lives in this crate and the features live in
    /// `zest-render-wgpu`'s manifest, so nothing but this test makes the two
    /// move together -- and it checks whichever platform it is compiled for,
    /// which is the only way the Linux answer gets checked at all.
    #[test]
    fn every_advertised_backend_is_compiled_in() {
        let compiled = wgpu::Instance::enabled_backend_features();
        for &b in preferred_backends() {
            assert!(
                compiled.contains(b),
                "the backend ladder offers {b:?}, but this binary compiled only \
                 {compiled:?} -- add its wgpu feature in zest-render-wgpu's \
                 manifest for this target, or stop advertising the rung"
            );
        }
    }

    /// The ladder is a preference order, so a duplicate would mean silently
    /// paying for a second instance of a backend that already failed.
    #[test]
    fn the_ladder_names_each_backend_once() {
        let ladder = preferred_backends();
        for (i, a) in ladder.iter().enumerate() {
            for b in &ladder[i + 1..] {
                assert_ne!(a, b, "{a:?} appears twice in the backend ladder");
            }
        }
    }
}

#[cfg(test)]
mod transparency_tests {
    use super::alpha_mode_for;
    use wgpu::CompositeAlphaMode::{Auto, Opaque, PostMultiplied, PreMultiplied};

    #[test]
    fn an_opaque_window_never_asks_for_per_pixel_alpha() {
        assert_eq!(alpha_mode_for(false, &[PreMultiplied, Opaque]), Opaque);
        assert_eq!(alpha_mode_for(false, &[Opaque]), Opaque);
    }

    #[test]
    fn a_translucent_window_takes_premultiplied_where_it_exists() {
        assert_eq!(alpha_mode_for(true, &[Opaque, PreMultiplied]), PreMultiplied);
    }

    #[test]
    fn an_adapter_without_it_falls_back_rather_than_failing() {
        // ADR-003: DX12 reports `Opaque` on every adapter, so this is the
        // ordinary Windows case, not an exotic one. It must start normally --
        // and it warns, because a window that stays opaque is a fact the user
        // needs in order to understand why their setting did nothing.
        assert_eq!(alpha_mode_for(true, &[Opaque]), Opaque);
        assert_eq!(alpha_mode_for(true, &[Auto, Opaque]), Opaque);
        assert_eq!(alpha_mode_for(true, &[]), Opaque, "an empty capability set is not a panic");
    }

    #[test]
    fn macos_takes_post_multiplied_and_no_one_else_does() {
        // Measured, not assumed: wgpu's Metal backend advertises exactly
        // `[Opaque, PostMultiplied]` on an Apple M4, so requiring
        // `PreMultiplied` made every Mac fall back to opaque -- window.opacity
        // did nothing here, and said the adapter was at fault.
        //
        // `PostMultiplied` on Metal is `CAMetalLayer::setOpaque(false)` and
        // nothing else, and CoreAnimation composites premultiplied, so it is
        // the mode this renderer wants. On Vulkan the same name means the
        // compositor multiplies by alpha itself, which would double-darken
        // premultiplied colour -- hence the platform split rather than simply
        // widening the accepted set.
        let metal = [Opaque, PostMultiplied];
        if cfg!(target_os = "macos") {
            assert_eq!(alpha_mode_for(true, &metal), PostMultiplied);
        } else {
            assert_eq!(
                alpha_mode_for(true, &metal),
                Opaque,
                "post-multiplied means straight alpha off macOS; taking it would double-darken"
            );
        }
        assert_eq!(alpha_mode_for(false, &metal), Opaque, "an opaque window is opaque everywhere");
        assert_eq!(
            alpha_mode_for(true, &[Opaque, PreMultiplied, PostMultiplied]),
            PreMultiplied,
            "where both exist the unambiguous one wins, on every platform"
        );
    }

    #[test]
    fn startup_and_reload_cannot_disagree() {
        // The whole reason this is a function rather than two copies of an
        // `if`. `init_gpu` decided it once and dropped the capability, so the
        // reload path had nothing to re-decide with -- which is what made
        // `window.opacity` restart-only while claiming to be `SurfaceRebuild`.
        let supported = [Opaque, PreMultiplied];
        for want in [true, false] {
            assert_eq!(
                alpha_mode_for(want, &supported),
                alpha_mode_for(want, &supported),
                "the same question must have one answer, whenever it is asked"
            );
        }
        assert_ne!(
            alpha_mode_for(true, &supported),
            alpha_mode_for(false, &supported),
            "and opacity must actually change it, or the reload is a no-op again"
        );
    }
}
