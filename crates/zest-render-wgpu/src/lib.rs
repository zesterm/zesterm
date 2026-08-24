//! The GPU renderer.
//!
//! Four pipelines in one render pass, then a resolve:
//!
//! 1. **SDF rects** — cell backgrounds, selection, cursor, *and* all window
//!    chrome. One pipeline for everything rectangular.
//! 2. **Glyphs** — instanced quads from a dual atlas, serving the grid and UI
//!    text alike.
//! 3. **Decorations** — underline, undercurl, strikethrough.
//! 4. **Background pictures** — one textured quad per pane. Built lazily and
//!    drawn first, because it *is* the window background for the panes that
//!    have one rather than a layer over it (`shaders/image.wgsl`).
//!
//! Everything renders into an `Rgba16Float` offscreen target and is resolved to
//! the surface by a fullscreen triangle. See `shaders/resolve.wgsl` for why that
//! indirection earns its place.
//!
//! Draw order is fixed and there is no depth buffer or sorting:
//!
//! ```text
//! image  background pictures, replacing the window background per pane
//! rect   window + chrome backgrounds
//! rect   cell backgrounds, selection, cursor
//! glyph  grid text
//! decor  underline / strikethrough / undercurl
//! rect   chrome shapes (tabs, buttons, borders)
//! glyph  chrome text
//! ---- resolve ----
//! ```

#![forbid(unsafe_code)]

pub mod atlas;
pub mod capture;
pub mod image;
pub mod instance;
pub mod scene;
pub mod ui_text;

pub use atlas::{Atlas, AtlasEntry, Cached};
pub use capture::read_rgba;
pub use image::{source_rect, BackgroundFit, BackgroundImage, ImageId, ImageStore};
pub use instance::{
    border_sides, glyph_flags, DecorInstance, DecorKind, GlyphInstance, Globals, ImageInstance,
    LinearRgba, RectInstance, RectShape,
};
pub use scene::{
    BlockBand, Chrome, Predicted, PredictedCell, Preedit, Scene, Viewport, RAIL_PX,
};
pub use ui_text::{emit_ui_run, measure_ui_run};

/// The offscreen format.
///
/// Float, so blending has headroom and the resolve can un-premultiply without
/// quantization damage. 8-bit would lose precision exactly where text
/// antialiasing needs it.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Text rendering knobs, applied to glyph coverage rather than baked per glyph.
///
/// Read per fragment from the shared globals, so changing either is a repaint
/// and never an atlas rebuild. They deliberately do **not** touch anything but
/// glyph coverage: a solid fill must come out of the frame as the colour that
/// went in, which [`Renderer`]'s `a_solid_fill_survives_the_frame` pins.
#[derive(Debug, Clone, Copy)]
pub struct TextTuning {
    /// Stem darkening. Light-on-dark needs meaningfully more than dark-on-light,
    /// which is why it is a knob and why themes can suggest a default.
    pub gamma: f32,
    pub contrast: f32,
}

impl TextTuning {
    /// The one built-in default.
    ///
    /// It lives here rather than in `zest-config` so there is a single number to
    /// argue with. The settings default is `None`, meaning "the theme's
    /// suggestion, or this" -- previously the config said 1.0 and the renderer
    /// said 1.3, and because nothing connected them the config's number was
    /// simply a lie.
    ///
    /// Coverage is linearized in the shader, so the two steps compose to
    /// `apparent = pow(coverage, 1/gamma)` — this knob is now exactly "how much
    /// heavier than perceptually-linear should a stroke be", and nothing else.
    ///
    /// 2.5 rather than something timid, settled by looking rather than by
    /// arithmetic: the aggregate measures barely move across 1.2 to 2.5 (41-42%
    /// of inked pixels fully saturated throughout), and the difference is
    /// plainly visible. More coverage is more contrast, and contrast is what
    /// reads as sharp.
    ///
    /// The same number suits a light background and a dark one, which the
    /// theory says it should not: dark-on-light is meant to need far less stem
    /// darkening. Tested on both and preferred on both, so the per-theme value
    /// `ThemeEffects` still has a comment proposing is not needed.
    pub const DEFAULT_GAMMA: f32 = 2.5;
    pub const DEFAULT_CONTRAST: f32 = 0.0;
}

impl Default for TextTuning {
    fn default() -> Self {
        Self { gamma: Self::DEFAULT_GAMMA, contrast: Self::DEFAULT_CONTRAST }
    }
}

/// A scene colour as a clear value for the offscreen pass.
///
/// A straight widening, deliberately: the offscreen is linear and every colour
/// reaching it is premultiplied (ADR-003), which is exactly what a `LinearRgba`
/// already holds. Converting here — encoding, or dividing the alpha back out —
/// would make the cleared pixels a different colour from an instance filled with
/// the same value, which is the one property this has to preserve.
fn clear_color(c: LinearRgba) -> wgpu::Color {
    let [r, g, b, a] = c.0;
    wgpu::Color { r: f64::from(r), g: f64::from(g), b: f64::from(b), a: f64::from(a) }
}

pub struct Renderer {
    rect_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    decor_pipeline: wgpu::RenderPipeline,
    resolve_pipeline: wgpu::RenderPipeline,

    /// The background-picture pipeline, built the first time a scene carries
    /// one. Lazily for the same reason as the subpixel glyph pipeline above:
    /// pipeline creation is a large share of a cold start, and the
    /// overwhelming majority of sessions never set a picture at all.
    image_pipeline: Option<wgpu::RenderPipeline>,

    /// The dual-source glyph pipeline, built the first time subpixel text is
    /// asked for. Lazily, because building it costs a shader module and a
    /// pipeline, and most sessions never switch mode.
    glyph_pipeline_subpixel: Option<wgpu::RenderPipeline>,
    /// Effective antialiasing, after the adapter and the destination have had
    /// their say. Not necessarily what the settings asked for.
    antialias: zest_font::TextAntialias,
    /// Whether this device can blend per-channel coverage at all.
    dual_source: bool,
    /// Set once the grayscale fallback has been reported, so a config reload
    /// does not repeat it.
    warned_no_dual_source: bool,

    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    globals_layout: wgpu::BindGroupLayout,
    atlas_bind_group_layout: wgpu::BindGroupLayout,
    atlas_bind_group: Option<wgpu::BindGroup>,
    atlas_generation: u64,

    resolve_layout: wgpu::BindGroupLayout,
    resolve_sampler: wgpu::Sampler,
    resolve_bind_group: Option<wgpu::BindGroup>,

    offscreen: Option<wgpu::Texture>,
    offscreen_view: Option<wgpu::TextureView>,
    size: (u32, u32),

    rects: GrowBuffer,
    glyphs: GrowBuffer,
    decors: GrowBuffer,
    image_instances: GrowBuffer,

    pub atlas: Atlas,
    /// The uploaded background pictures. Filled by the embedder, which owns the
    /// decoder — this crate never reads a file.
    pub images: ImageStore,
    pub tuning: TextTuning,
}

/// The grid module: rects, glyphs and decorations in one, for the reason given
/// where it is created.
const GRID_WGSL: &str = concat!(
    include_str!("shaders/common.wgsl"),
    "\n",
    include_str!("shaders/rect.wgsl"),
    "\n",
    include_str!("shaders/glyph.wgsl"),
    "\n",
    include_str!("shaders/decor.wgsl"),
);

/// The subpixel variant: the same glyph vertex stage plus a dual-source
/// fragment stage.
///
/// A separate module rather than a second entry point in `GRID_WGSL`, because
/// naga checks `@blend_src` during **type** validation — a module that merely
/// contains the output struct is rejected by `create_shader_module` on a device
/// without `DUAL_SOURCE_BLENDING`, whether or not anything uses it. One module
/// for everyone would therefore fail to start on exactly the machines the
/// fallback exists to serve.
///
/// `enable` must precede every declaration, so it is prepended here rather than
/// written at the top of `glyph_subpixel.wgsl`. Rect and decor are left out:
/// this module exists only to supply the glyph pipeline.
/// The background-picture module: `common.wgsl` plus its own stages.
const IMAGE_WGSL: &str = concat!(
    include_str!("shaders/common.wgsl"),
    "\n",
    include_str!("shaders/image.wgsl"),
);

const GRID_WGSL_SUBPIXEL: &str = concat!(
    "enable dual_source_blending;\n",
    include_str!("shaders/common.wgsl"),
    "\n",
    include_str!("shaders/glyph.wgsl"),
    "\n",
    include_str!("shaders/glyph_subpixel.wgsl"),
);

impl Renderer {
    /// Build the renderer for a given surface format.
    ///
    /// `target_format` **must not be an sRGB format**. The resolve shader
    /// performs the sRGB encode itself, because premultiplying after a hardware
    /// encode is wrong (see `shaders/resolve.wgsl`). Handing it an `*Srgb`
    /// format would encode twice and wash everything out.
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        antialias: zest_font::TextAntialias,
    ) -> Self {
        Self::with_cache(device, target_format, None, antialias)
    }

    /// As [`Renderer::new`], reusing a driver pipeline cache.
    ///
    /// Pipeline creation is ~400-500ms of a cold start and is entirely
    /// re-derivable, so a warm cache is the single biggest remaining startup
    /// win. The cache itself is created by the caller because loading one from
    /// disk is `unsafe` -- bad data can crash a driver -- and this crate is
    /// `forbid(unsafe_code)`. The unsafety belongs with whoever owns the file.
    pub fn with_cache(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        cache: Option<&wgpu::PipelineCache>,
        antialias: zest_font::TextAntialias,
    ) -> Self {
        debug_assert!(
            !target_format.is_srgb(),
            "the resolve pass encodes sRGB itself; an sRGB target would encode twice"
        );

        // One module for all three grid pipelines, not three.
        //
        // Shader module creation is parse + validate + backend codegen, and it
        // was ~450ms of a ~1.9s cold start. Three modules meant parsing
        // common.wgsl three times and paying the fixed per-module cost three
        // times, for shaders that share every helper. Entry points are named
        // per-stage (`vs_rect`, `fs_glyph`, ...) so they can coexist.
        //
        // Resolve stays separate: it has a different bind group layout and no
        // use for any of the shared code.
        let t_shader = std::time::Instant::now();
        let grid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zest grid"),
            source: wgpu::ShaderSource::Wgsl(GRID_WGSL.into()),
        });
        let resolve_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zest resolve"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/resolve.wgsl").into()),
        });

        tracing::debug!(ms = t_shader.elapsed().as_millis(), "shader modules");
        let t_pipe = std::time::Instant::now();

        // --- globals ---
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zest globals layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zest globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zest globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        let atlas_bind_group_layout = Atlas::bind_group_layout(device);

        // --- pipelines ---
        let rect_pipeline = make_pipeline(
            device,
            "zest rect",
            &grid_shader,
            ("vs_rect", "fs_rect"),
            &[&globals_layout],
            rect_vertex_layout(),
            Some(PREMULTIPLIED_OVER),
            cache,
        );
        let glyph_pipeline = make_pipeline(
            device,
            "zest glyph",
            &grid_shader,
            ("vs_glyph", "fs_glyph"),
            &[&globals_layout, &atlas_bind_group_layout],
            glyph_vertex_layout(),
            Some(PREMULTIPLIED_OVER),
            cache,
        );
        let decor_pipeline = make_pipeline(
            device,
            "zest decor",
            &grid_shader,
            ("vs_decor", "fs_decor"),
            &[&globals_layout],
            decor_vertex_layout(),
            Some(PREMULTIPLIED_OVER),
            cache,
        );

        // --- resolve ---
        let resolve_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zest resolve layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let resolve_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("zest resolve"),
                bind_group_layouts: &[Some(&resolve_layout)],
                immediate_size: 0,
            });
        let resolve_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("zest resolve"),
            layout: Some(&resolve_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &resolve_shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &resolve_shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // The resolve writes the final composite; nothing blends
                    // with it.
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache,
        });

        let resolve_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("zest resolve sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        tracing::debug!(ms = t_pipe.elapsed().as_millis(), "pipeline objects");

        let dual_source = device.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING);

        let mut me = Self {
            rect_pipeline,
            glyph_pipeline,
            decor_pipeline,
            resolve_pipeline,
            image_pipeline: None,
            glyph_pipeline_subpixel: None,
            // Starts grayscale and is raised by `set_text_antialias`, so there
            // is exactly one place that decides -- and one place that logs when
            // the answer is not what was asked for.
            antialias: zest_font::TextAntialias::Grayscale,
            dual_source,
            warned_no_dual_source: false,
            globals_buffer,
            globals_bind_group,
            globals_layout,
            atlas_bind_group_layout,
            atlas_bind_group: None,
            atlas_generation: u64::MAX,
            resolve_layout,
            resolve_sampler,
            resolve_bind_group: None,
            offscreen: None,
            offscreen_view: None,
            size: (0, 0),
            rects: GrowBuffer::new("zest rects"),
            glyphs: GrowBuffer::new("zest glyphs"),
            decors: GrowBuffer::new("zest decors"),
            image_instances: GrowBuffer::new("zest images"),
            atlas: Atlas::new(device, zest_font::TextAntialias::Grayscale),
            images: ImageStore::new(device),
            tuning: TextTuning::default(),
        };
        me.set_text_antialias(device, antialias);
        me
    }

    /// The antialiasing mode actually in use.
    ///
    /// May be [`TextAntialias::Grayscale`] when subpixel was requested — see
    /// [`Renderer::set_text_antialias`] for the ladder.
    #[must_use]
    pub fn text_antialias(&self) -> zest_font::TextAntialias {
        self.antialias
    }

    /// Whether this device can blend per-channel coverage.
    #[must_use]
    pub fn supports_subpixel_text(&self) -> bool {
        self.dual_source
    }

    /// Ask for an antialiasing mode, and get the one that is actually possible.
    ///
    /// Subpixel coverage needs a per-channel destination blend factor, which is
    /// `Features::DUAL_SOURCE_BLENDING` — DX12 (including WARP), Vulkan where
    /// the adapter reports `dualSrcBlend`, and Metal. Where it is missing this
    /// falls back to grayscale and says so once, rather than silently rendering
    /// a third of every glyph.
    ///
    /// The caller must also refuse subpixel over a *translucent* destination:
    /// three coverages cannot be composited by a compositor holding one alpha.
    /// That is a property of the scene, not of the device, so it is decided
    /// where opacity is known and arrives here as a plain `Grayscale`.
    ///
    /// Switching clears the atlas: the two modes store a different number of
    /// bytes per texel.
    pub fn set_text_antialias(&mut self, device: &wgpu::Device, mode: zest_font::TextAntialias) {
        let want_subpixel = mode == zest_font::TextAntialias::Subpixel;
        if want_subpixel && !self.dual_source {
            if self.antialias != zest_font::TextAntialias::Grayscale {
                self.antialias = zest_font::TextAntialias::Grayscale;
                self.atlas.set_text_antialias(device, self.antialias);
            }
            // Once per process, not once per call: a config reload asks again
            // every time, and a capability that cannot change is not news twice.
            if !self.warned_no_dual_source {
                self.warned_no_dual_source = true;
                tracing::warn!(
                    "this adapter cannot do dual-source blending, so subpixel text is \
                     unavailable; falling back to grayscale antialiasing"
                );
            }
            return;
        }
        if mode == self.antialias {
            return;
        }

        if want_subpixel && self.glyph_pipeline_subpixel.is_none() {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("zest grid (subpixel)"),
                source: wgpu::ShaderSource::Wgsl(GRID_WGSL_SUBPIXEL.into()),
            });
            self.glyph_pipeline_subpixel = Some(make_pipeline(
                device,
                "zest glyph (subpixel)",
                &module,
                ("vs_glyph", "fs_glyph_subpixel"),
                &[&self.globals_layout, &self.atlas_bind_group_layout],
                glyph_vertex_layout(),
                Some(PREMULTIPLIED_OVER_DUAL),
                None,
            ));
        }

        self.antialias = mode;
        self.atlas.set_text_antialias(device, mode);
    }

    /// The glyph pipeline for the mode in force.
    fn glyphs_pipeline(&self) -> &wgpu::RenderPipeline {
        match self.antialias {
            zest_font::TextAntialias::Subpixel => {
                self.glyph_pipeline_subpixel.as_ref().unwrap_or(&self.glyph_pipeline)
            }
            zest_font::TextAntialias::Grayscale => &self.glyph_pipeline,
        }
    }

    /// Resize the offscreen target. Cheap and idempotent when unchanged.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if self.size == (w, h) && self.offscreen.is_some() {
            return;
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zest offscreen"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());

        self.resolve_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zest resolve"),
            layout: &self.resolve_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.resolve_sampler) },
            ],
        }));

        self.offscreen = Some(texture);
        self.offscreen_view = Some(view);
        self.size = (w, h);
    }

    /// Draw a scene into `target`.
    ///
    /// `target`'s format must match the one passed to [`Renderer::new`].
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        scene: &Scene,
    ) {
        let (w, h) = self.size;
        let Some(offscreen_view) = self.offscreen_view.as_ref() else {
            tracing::error!("render called before resize");
            return;
        };

        queue.write_buffer(
            &self.globals_buffer,
            0,
            bytemuck::bytes_of(&Globals {
                target_size: [w as f32, h as f32],
                grid_origin: scene.grid_origin,
                text_gamma: self.tuning.gamma,
                text_contrast: self.tuning.contrast,
                _pad: [0.0; 2],
            }),
        );
        self.rects.upload(device, queue, bytemuck::cast_slice(&scene.rects));
        self.glyphs.upload(device, queue, bytemuck::cast_slice(&scene.glyphs));
        self.decors.upload(device, queue, bytemuck::cast_slice(&scene.decors));
        self.image_instances.upload(device, queue, bytemuck::cast_slice(&scene.images));

        if !scene.images.is_empty() && self.image_pipeline.is_none() {
            self.image_pipeline = Some(self.build_image_pipeline(device));
        }

        // Rebuild the atlas bind group only when the atlas actually changed --
        // it is recreated on growth, so the view can go stale.
        if self.atlas_bind_group.is_none() || self.atlas_generation != self.atlas.generation() {
            self.atlas_bind_group =
                Some(self.atlas.bind_group(device, &self.atlas_bind_group_layout));
            self.atlas_generation = self.atlas.generation();
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("zest main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: offscreen_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The window's backdrop, not an empty target: the
                        // padding, the gaps around the chrome bars and the
                        // split gutter are covered by no instance at all, and
                        // a transparent clear leaves them black on an opaque
                        // surface. Opacity is still expressed by the colour
                        // rather than by the pass — `scene.backdrop` carries
                        // it, premultiplied like every instance (ADR-003).
                        load: wgpu::LoadOp::Clear(clear_color(scene.backdrop)),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            pass.set_bind_group(0, &self.globals_bind_group, &[]);

            // The pictures first: for a pane that has one this quad *is* the
            // window background, so it must land after nothing but the clear
            // and before any cell fill. One draw each, because each binds its
            // own texture -- there are at most a handful, one per pane.
            if let Some(pipeline) = self.image_pipeline.as_ref() {
                let mut bound = false;
                for (i, inst) in scene.images.iter().enumerate() {
                    // A picture the embedder has not uploaded yet (or has
                    // evicted) draws nothing at all rather than a black hole:
                    // the pane then looks exactly as it did before the setting
                    // was touched, which is the honest thing for one frame.
                    let Some((group, _)) = self.images.get(inst.id()) else { continue };
                    if !bound {
                        pass.set_pipeline(pipeline);
                        pass.set_vertex_buffer(0, self.image_instances.slice());
                        bound = true;
                    }
                    pass.set_bind_group(1, group, &[]);
                    pass.draw(0..4, i as u32..i as u32 + 1);
                }
            }

            // The documented order, honoured with split instance ranges: the
            // buffers hold grid and chrome together, but a single whole-buffer
            // draw per pipeline puts every grid glyph *after* the chrome's
            // rects — which is a fleet picker with the shell's prompt shining
            // through its panel. Grid first, decorations, then the chrome
            // ranges on top.
            // Two chrome layers: base (bars, tabs, screens, block headers)
            // and overlay (picker/palette/settings). Without the second
            // split, every base-chrome glyph paints *after* the overlay's
            // panel — the fleet screen's text bled straight through the
            // palette floating over it.
            let overlay_r = scene.overlay_rects_at.clamp(scene.chrome_rects_at, scene.rects.len());
            let overlay_g =
                scene.overlay_glyphs_at.clamp(scene.chrome_glyphs_at, scene.glyphs.len());
            let base_rects = scene.chrome_rects_at as u32..overlay_r as u32;
            let base_glyphs = scene.chrome_glyphs_at as u32..overlay_g as u32;
            let overlay_rects = overlay_r as u32..scene.rects.len() as u32;
            let overlay_glyphs = overlay_g as u32..scene.glyphs.len() as u32;

            if scene.chrome_rects_at > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, self.rects.slice());
                pass.draw(0..4, 0..scene.chrome_rects_at as u32);
            }

            if scene.chrome_glyphs_at > 0 {
                if let Some(bg) = self.atlas_bind_group.as_ref() {
                    pass.set_pipeline(self.glyphs_pipeline());
                    pass.set_bind_group(1, bg, &[]);
                    pass.set_vertex_buffer(0, self.glyphs.slice());
                    pass.draw(0..4, 0..scene.chrome_glyphs_at as u32);
                }
            }

            if !scene.decors.is_empty() {
                pass.set_pipeline(&self.decor_pipeline);
                pass.set_vertex_buffer(0, self.decors.slice());
                pass.draw(0..4, 0..scene.decors.len() as u32);
            }

            for (rects, glyphs) in
                [(base_rects, base_glyphs), (overlay_rects, overlay_glyphs)]
            {
                if !rects.is_empty() {
                    pass.set_pipeline(&self.rect_pipeline);
                    pass.set_vertex_buffer(0, self.rects.slice());
                    pass.draw(0..4, rects);
                }
                if !glyphs.is_empty() {
                    if let Some(bg) = self.atlas_bind_group.as_ref() {
                        pass.set_pipeline(self.glyphs_pipeline());
                        pass.set_bind_group(1, bg, &[]);
                        pass.set_vertex_buffer(0, self.glyphs.slice());
                        pass.draw(0..4, glyphs);
                    }
                }
            }
        }

        self.resolve(encoder, target);
    }

    /// Build the background-picture pipeline.
    ///
    /// Its own shader module rather than an entry point in `GRID_WGSL`: the
    /// glyph shader already claims `@group(1)` there for the atlas, and this
    /// one needs that slot for the picture.
    fn build_image_pipeline(&self, device: &wgpu::Device) -> wgpu::RenderPipeline {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zest image"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_WGSL.into()),
        });
        make_pipeline(
            device,
            "zest image",
            &module,
            ("vs_image", "fs_image"),
            &[&self.globals_layout, self.images.layout()],
            image_vertex_layout(),
            // Replace, not source-over. See `shaders/image.wgsl`.
            None,
            None,
        )
    }

    /// Run only the resolve pass.
    ///
    /// This is what makes an OS-driven repaint nearly free: when the compositor
    /// demands a redraw and nothing is dirty, the retained offscreen is simply
    /// re-resolved without rebuilding a single instance.
    pub fn resolve(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
        let Some(bind_group) = self.resolve_bind_group.as_ref() else { return };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("zest resolve"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.resolve_pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Drop every cached glyph.
    ///
    /// Called when the font stack, size, or DPI changes — anything that makes
    /// the cached rasterizations wrong. Bulk, not per-glyph: re-rastering the
    /// visible set costs a few milliseconds and only happens on a deliberate
    /// user action, whereas a per-glyph LRU is where terminals over-engineer
    /// themselves into cache thrash.
    pub fn clear_atlas(&mut self) {
        self.atlas.clear();
    }

    /// The current atlas generation, which changes on every clear.
    #[must_use]
    pub fn atlas_generation(&self) -> u64 {
        self.atlas.generation()
    }
}

/// An instance buffer that grows by reallocation.
///
/// Power-of-two growth so a steadily busier screen does not reallocate every
/// frame. Shrinking is deliberately never done: a window that was once large
/// is likely to be again, and the memory is small.
struct GrowBuffer {
    label: &'static str,
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    len: u64,
}

impl GrowBuffer {
    fn new(label: &'static str) -> Self {
        Self { label, buffer: None, capacity: 0, len: 0 }
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[u8]) {
        self.len = data.len() as u64;
        if data.is_empty() {
            return;
        }

        if self.capacity < self.len || self.buffer.is_none() {
            let capacity = self.len.next_power_of_two().max(4096);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = capacity;
        }

        if let Some(buffer) = self.buffer.as_ref() {
            queue.write_buffer(buffer, 0, data);
        }
    }

    fn slice(&self) -> wgpu::BufferSlice<'_> {
        self.buffer.as_ref().expect("slice on an empty buffer").slice(..self.len)
    }
}

/// Premultiplied source-over, in every pipeline without exception.
///
/// Mixing this with SrcAlpha/OneMinusSrcAlpha is what produces dark halos
/// around text. -> ADR-003.
const PREMULTIPLIED_OVER: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

/// Premultiplied source-over with a *per-channel* destination factor.
///
/// Subpixel coverage is three alphas per texel, which the equation above
/// structurally cannot express: it has one `OneMinusSrcAlpha` for all three
/// channels. The second fragment output carries the per-channel coverage and
/// `OneMinusSrc1` consumes it, which is still exactly source-over -- per
/// primitive, so overlapping glyphs (combining marks sit on top of their base)
/// stay correct. Requires `Features::DUAL_SOURCE_BLENDING`.
const PREMULTIPLIED_OVER_DUAL: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrc1,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrc1Alpha,
        operation: wgpu::BlendOperation::Add,
    },
};

// Eight, because a render pipeline genuinely has eight independent parts and
// the four call sites all read better as a flat list than as a builder or a
// descriptor struct that would exist only to satisfy the lint.
#[allow(clippy::too_many_arguments)]
fn make_pipeline(
    device: &wgpu::Device,
    label: &str,
    shader: &wgpu::ShaderModule,
    entry_points: (&str, &str),
    bind_group_layouts: &[&wgpu::BindGroupLayout],
    vertex_layout: wgpu::VertexBufferLayout<'_>,
    blend: Option<wgpu::BlendState>,
    cache: Option<&wgpu::PipelineCache>,
) -> wgpu::RenderPipeline {
    let layouts: Vec<Option<&wgpu::BindGroupLayout>> =
        bind_group_layouts.iter().map(|l| Some(*l)).collect();
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &layouts,
        immediate_size: 0,
    });

    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some(entry_points.0),
            buffers: &[Some(vertex_layout)],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(entry_points.1),
            targets: &[Some(wgpu::ColorTargetState {
                format: OFFSCREEN_FORMAT,
                // `None` means replace, and exactly one pipeline wants it: the
                // background picture writes the finished window background, so
                // blending it over the clear would apply the window opacity a
                // second time.
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache,
    })
}

fn image_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    // Five attributes for a struct of seven fields: `id` and `_pad` are CPU
    // state riding along in the padding, and `shaders/image.wgsl` declares
    // locations 0..=4 and stops. A vertex buffer may be longer than the
    // attributes that read it.
    const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        0 => Float32x4,  // rect
        1 => Float32x4,  // clip
        2 => Float32x4,  // src
        3 => Float32x4,  // base
        4 => Float32,    // dim
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ImageInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

fn rect_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 11] = wgpu::vertex_attr_array![
        0 => Float32x4,  // rect
        1 => Float32x4,  // rect_b
        2 => Float32x4,  // radii
        3 => Float32x4,  // fill
        4 => Float32x4,  // border
        5 => Float32x4,  // clip
        6 => Float32,    // border_width
        7 => Float32,    // shadow_blur
        8 => Float32,    // shadow_alpha
        9 => Uint32,     // shape
        10 => Uint32,    // border_omit
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<RectInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

fn glyph_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
        0 => Float32x2,  // pos
        1 => Float32x2,  // uv
        2 => Float32x2,  // size
        3 => Float32x4,  // color
        4 => Float32x4,  // clip
        5 => Uint32,     // layer
        6 => Uint32,     // flags
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<GlyphInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

fn decor_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
        0 => Float32x4,  // rect
        1 => Float32x4,  // color
        2 => Float32x4,  // clip
        3 => Uint32,     // kind
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<DecorInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        border_sides, ImageId, ImageInstance, LinearRgba, RectInstance, Renderer, Scene,
        TextTuning,
    };

    /// Side of the square frame the background-picture probes render into.
    const IMAGE_PROBE: u32 = 16;

    /// A headless device, or `None` where there is no adapter at all.
    ///
    /// Any adapter will do, including a software one, which is what makes this
    /// runnable in CI — the same assumption `examples/render_dump.rs` makes.
    /// Returning `None` rather than panicking keeps a machine with no GPU at all
    /// from failing the suite for a reason that is not about this code.
    fn headless() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok()?;
        let dual = adapter.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING);
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("zest test"),
            required_features: if dual {
                wgpu::Features::DUAL_SOURCE_BLENDING
            } else {
                wgpu::Features::empty()
            },
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        }))
        .ok()
    }

    /// Render a scene with nothing but a backdrop and read one pixel back.
    fn backdrop_pixel(tuning: TextTuning, color: LinearRgba) -> Option<[u8; 4]> {
        let (device, queue) = headless()?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(&device, format, zest_font::TextAntialias::Grayscale);
        renderer.tuning = tuning;
        renderer.resize(&device, 4, 4);

        let scene = Scene { backdrop: color, ..Default::default() };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zest test target"),
            size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.render(&device, &queue, &mut encoder, &view, &scene);
        queue.submit([encoder.finish()]);

        let px = crate::read_rgba(&device, &queue, &texture, 4, 4, format);
        Some([px[0], px[1], px[2], px[3]])
    }

    /// Draw one background picture over a black backdrop and read a pixel back.
    ///
    /// `texel` is one straight-alpha sRGB pixel, uploaded as a 1x1 picture and
    /// stretched over the whole target — so every pixel of the frame answers
    /// the same question and the test does not depend on the sampler's
    /// filtering.
    ///
    /// 16 square rather than 4: the pixel read back is the centre, far enough
    /// from every edge that no antialiasing reaches it. At 4 the rect this is
    /// compared against in `dimming_all_the_way_is_the_plain_background` loses
    /// five levels to its own SDF edge — a fact about that pipeline, not this
    /// one.
    fn image_pixel(texel: [u8; 4], base: LinearRgba, dim: f32) -> Option<[u8; 4]> {
        let (device, queue) = headless()?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(&device, format, zest_font::TextAntialias::Grayscale);
        renderer.resize(&device, IMAGE_PROBE, IMAGE_PROBE);

        let id = ImageId(0xdead_beef_0000_0001);
        assert!(renderer.images.upload(&device, &queue, id, [1, 1], &texel), "upload");

        let rect = [0.0, 0.0, IMAGE_PROBE as f32, IMAGE_PROBE as f32];
        let scene = Scene {
            images: vec![ImageInstance::new(rect, rect, rect, base, dim, id)],
            // Deliberately *not* the base: the picture must write the finished
            // pixel rather than blend with whatever the clear left, so a
            // contrasting clear is what would expose a blend that crept back in.
            backdrop: LinearRgba::opaque(0, 255, 0),
            ..Default::default()
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zest image probe"),
            size: wgpu::Extent3d { width: IMAGE_PROBE, height: IMAGE_PROBE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.render(&device, &queue, &mut encoder, &view, &scene);
        queue.submit([encoder.finish()]);

        let px = crate::read_rgba(&device, &queue, &texture, IMAGE_PROBE, IMAGE_PROBE, format);
        let i = image_probe_centre();
        Some([px[i], px[i + 1], px[i + 2], px[i + 3]])
    }

    /// Byte offset of the centre pixel of a `IMAGE_PROBE`-square frame.
    fn image_probe_centre() -> usize {
        ((IMAGE_PROBE / 2) * IMAGE_PROBE + IMAGE_PROBE / 2) as usize * 4
    }

    #[test]
    fn a_picture_reaches_the_frame_as_its_own_colour() {
        let Some(px) = image_pixel([255, 0, 0, 255], LinearRgba::opaque(0, 0, 255), 0.0) else {
            return;
        };
        // Red in, red out. A green frame means the clear won and the pipeline
        // never drew; blue means `dim` is inverted.
        assert!(px[0] > 240 && px[1] < 16 && px[2] < 16, "wanted the picture's red, got {px:?}");
        assert_eq!(px[3], 255, "an opaque pane stays opaque: {px:?}");
    }

    #[test]
    fn dimming_all_the_way_is_the_plain_background() {
        // The load-bearing one. `dim = 1` must be byte-identical to the rect
        // this quad replaced, or every Fit letterbox and Watermark margin is a
        // visible seam against the padding beside it -- the same property
        // `the_clear_value_matches_an_instance_of_the_same_colour` pins for the
        // clear.
        let base = LinearRgba::opaque(0, 0, 255);
        let Some(dimmed) = image_pixel([255, 0, 0, 255], base, 1.0) else { return };
        let full = [0.0, 0.0, IMAGE_PROBE as f32, IMAGE_PROBE as f32];
        let Some(plain) = rect_pixels(vec![RectInstance::filled(full, base, full)], IMAGE_PROBE) else {
            return;
        };
        let i = image_probe_centre();
        assert_eq!(
            dimmed,
            [plain[i], plain[i + 1], plain[i + 2], plain[i + 3]],
            "a fully dimmed picture must be indistinguishable from no picture"
        );
    }

    #[test]
    fn a_picture_composites_the_window_opacity_exactly_once() {
        // The bug this exists to prevent, and the reason the pipeline replaces
        // instead of blending: drawn with source-over, this quad would land on
        // a clear that already carries the same alpha and come out at
        // 1-(1-0.8)^2 = 0.96 -- a grid visibly less transparent than the
        // padding around it, which is #44 wearing a photograph.
        let base = LinearRgba::from_srgb(0, 0, 255, 0.8);
        let Some(px) = image_pixel([255, 0, 0, 255], base, 0.0) else { return };
        assert!(
            (i16::from(px[3]) - 204).abs() <= 2,
            "wanted alpha 0.8 (204), got {px:?}; 245 means the opacity blended twice"
        );
    }

    #[test]
    fn a_transparent_texel_leaves_the_background_alone() {
        // A PNG with an alpha channel is the ordinary case for a watermark, and
        // its transparent corner must read as "no picture here" rather than as
        // a black hole punched through the pane.
        let base = LinearRgba::opaque(0, 0, 255);
        let Some(px) = image_pixel([255, 0, 0, 0], base, 0.0) else { return };
        assert!(px[2] > 240 && px[0] < 16, "wanted the plain background, got {px:?}");
    }

    #[test]
    fn a_picture_with_no_texture_uploaded_draws_nothing_at_all() {
        // A path that has not finished decoding, or one the store evicted. The
        // pane must look exactly as it did before the setting was touched --
        // never a hole, and never a panic.
        let (device, queue) = match headless() {
            Some(pair) => pair,
            None => return,
        };
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(&device, format, zest_font::TextAntialias::Grayscale);
        renderer.resize(&device, IMAGE_PROBE, IMAGE_PROBE);

        let rect = [0.0, 0.0, IMAGE_PROBE as f32, IMAGE_PROBE as f32];
        let scene = Scene {
            images: vec![ImageInstance::new(
                rect,
                rect,
                rect,
                LinearRgba::opaque(255, 0, 0),
                0.0,
                ImageId(404),
            )],
            backdrop: LinearRgba::opaque(0, 0, 255),
            ..Default::default()
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zest missing image probe"),
            size: wgpu::Extent3d { width: IMAGE_PROBE, height: IMAGE_PROBE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.render(&device, &queue, &mut encoder, &view, &scene);
        queue.submit([encoder.finish()]);

        let px = crate::read_rgba(&device, &queue, &texture, IMAGE_PROBE, IMAGE_PROBE, format);
        let i = image_probe_centre();
        assert!(px[i + 2] > 240 && px[i] < 16, "wanted the untouched backdrop, got {:?}", &px[i..i + 4]);
    }

    /// Render `rects` over a black backdrop into a `size`-square target.
    fn rect_pixels(rects: Vec<RectInstance>, size: u32) -> Option<Vec<u8>> {
        let (device, queue) = headless()?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(&device, format, zest_font::TextAntialias::Grayscale);
        renderer.resize(&device, size, size);

        // Every index left at 0 puts the whole vec in the overlay range, which
        // is the only thing that matters here: it gets drawn once.
        let scene = Scene { rects, backdrop: LinearRgba::opaque(0, 0, 0), ..Default::default() };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zest rect probe"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.render(&device, &queue, &mut encoder, &view, &scene);
        queue.submit([encoder.finish()]);
        Some(crate::read_rgba(&device, &queue, &texture, size, size, format))
    }

    /// A 128px box with a fat border, so one pixel of antialiasing cannot
    /// swallow the thing being measured.
    const PROBE: u32 = 128;
    fn probe_rect(omit: u32) -> RectInstance {
        RectInstance {
            radii: [32.0; 4],
            border: LinearRgba::opaque(255, 0, 0),
            border_width: 8.0,
            border_omit: omit,
            ..RectInstance::filled(
                [16.0, 16.0, 96.0, 96.0],
                LinearRgba::opaque(0x30, 0x30, 0x30),
                [0.0, 0.0, 128.0, 128.0],
            )
        }
    }

    /// Pixels along a scanline that are unmistakably the border's red, over the
    /// left half only — a count in pixels, so a stroke's thickness is readable
    /// straight off it.
    fn red_across(px: &[u8], row: u32) -> u32 {
        (0..PROBE / 2)
            .filter(|x| {
                let i = ((row * PROBE + x) * 4) as usize;
                px[i] > 200 && px[i + 1] < 60
            })
            .count() as u32
    }

    /// The same, down a column, over the top half.
    fn red_down(px: &[u8], col: u32) -> u32 {
        (0..PROBE / 2)
            .filter(|y| {
                let i = ((y * PROBE + col) * 4) as usize;
                px[i] > 200 && px[i + 1] < 60
            })
            .count() as u32
    }

    #[test]
    fn a_one_sided_border_tapers_into_the_corners() {
        // This is the whole point of `border_omit`, and it is the one thing a
        // clipped ring cannot fake. CSS draws `border-left` on a rounded box by
        // interpolating the stroke down to the (zero-width) top border's, so the
        // 8px rail thins as the corner turns and is gone by the time the edge is
        // horizontal. Clipping a full-weight ring instead cuts it off square,
        // which reads as two stubs — the defect this replaced.
        let Some(px) = rect_pixels(vec![probe_rect(border_sides::TOP | border_sides::RIGHT | border_sides::BOTTOM)], PROBE)
        else {
            return;
        };

        let straight = red_across(&px, 64); // the vertical run, full thickness
        let mid_arc = red_across(&px, 28); // partway round the corner
        let near_top = red_across(&px, 20); // nearly horizontal, nearly gone

        assert!((7..=9).contains(&straight), "the straight run is the border's 8px, got {straight}");
        assert!(
            near_top < mid_arc && mid_arc < straight,
            "the stroke must thin monotonically into the corner, got {straight} → {mid_arc} → {near_top}"
        );
        assert!(near_top > 0, "and still be drawn there — the arc is stroked, not clipped away");
        assert_eq!(
            red_down(&px, 64),
            0,
            "the top edge is omitted: nothing is drawn along it, only the taper reaching into it"
        );
    }

    #[test]
    fn omitting_nothing_still_rings_all_four_sides() {
        // Zero is the `Zeroable` value and every existing bordered rect leaves
        // the field there. The maths changed underneath them — an inset inner
        // box rather than the SDF offset `d + bw` — and the two are only equal
        // because deflating a rounded box by `bw` also reduces every radius by
        // `bw`. If that identity is ever broken, every border in the app moves.
        let Some(px) = rect_pixels(vec![probe_rect(0)], PROBE) else { return };
        let left = red_across(&px, 64);
        let top = red_down(&px, 64);
        assert!((7..=9).contains(&left), "left border still 8px, got {left}");
        assert!((7..=9).contains(&top), "top border still 8px, got {top}");
    }

    #[test]
    fn an_opaque_rect_does_not_hide_what_is_under_its_own_edge() {
        // Not a defect to fix here — it is what antialiasing *is*, and it is
        // why rounded corners are smooth. It is recorded because a caller
        // reasoned the other way and was wrong: a full-pane screen was painted
        // over the terminal on the assumption that an opaque fill hides it, and
        // the block cursor bled through the screen's outermost row and column
        // as a stray bracket (#253, `zest-app`'s `pane_is_covered`). Anything
        // that must not be seen must not be *drawn*.
        //
        // Both rects land on integer bounds and the second exactly covers the
        // first, so nothing here depends on subpixel placement.
        let cover = RectInstance::filled(
            [16.0, 16.0, 96.0, 96.0],
            LinearRgba::opaque(0x30, 0x30, 0x30),
            [0.0, 0.0, 128.0, 128.0],
        );
        let under = RectInstance::filled(
            [16.0, 16.0, 96.0, 96.0],
            LinearRgba::opaque(255, 0, 0),
            [0.0, 0.0, 128.0, 128.0],
        );
        let Some(px) = rect_pixels(vec![under, cover], PROBE) else { return };
        let at = |x: u32, y: u32| {
            let i = ((y * PROBE + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };
        let edge = at(16, 60); // the cover's outermost column
        let inside = at(20, 60); // four pixels in, fully covered
        assert!(
            edge[0] > inside[0] + 20,
            "the covered rect's own edge column leaks what is beneath it: edge {edge:?} vs interior {inside:?}"
        );
        assert!(
            inside[0] < 80,
            "and four pixels in it does not, so this really is an edge effect: {inside:?}"
        );
    }

    #[test]
    fn a_solid_fill_survives_the_frame() {
        // The bug this pins: text_gamma was applied in the resolve pass, which
        // sees the *finished* frame, so `pow(rgb, 1/1.3)` landed on every pixel
        // with any alpha -- cell backgrounds, selection and chrome as much as
        // text. A dark theme's background arrived several shades lighter than
        // the colour the theme asked for, and no setting could turn it off.
        //
        // Stem darkening now adjusts glyph coverage instead, so a solid fill has
        // to come out of the frame as exactly the colour that went in, at every
        // tuning. There are no glyphs in this scene at all.
        let dark = LinearRgba::opaque(0x0D, 0x0D, 0x0D);
        for gamma in [1.0f32, 1.3, 2.5] {
            let tuning = TextTuning { gamma, contrast: 0.5 };
            let Some(px) = backdrop_pixel(tuning, dark) else { return };
            // One 8-bit step of slack for the linear round trip, and no more:
            // the old code moved this by roughly 0x0D at gamma 1.3.
            for (i, (got, want)) in px.iter().zip([0x0Du8, 0x0D, 0x0D, 0xFF]).enumerate() {
                assert!(
                    (i32::from(*got) - i32::from(want)).abs() <= 1,
                    "channel {i} came back {got:#04X}, wanted {want:#04X} at gamma {gamma} \
                     -- a solid fill must not be touched by a text setting"
                );
            }
        }
    }

    /// Draw one synthetic glyph and read the pixel under it.
    ///
    /// The mask is supplied directly rather than rasterized from a font, so the
    /// assertion is deterministic on any machine and says nothing about which
    /// fonts are installed.
    fn glyph_pixel(mode: zest_font::TextAntialias, mask: &zest_font::GlyphImage) -> Option<[u8; 4]> {
        let (device, queue) = headless()?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(&device, format, mode);
        // The device may not offer dual-source blending; skip loudly rather
        // than assert something the test cannot have set up.
        if mode == zest_font::TextAntialias::Subpixel
            && renderer.text_antialias() != zest_font::TextAntialias::Subpixel
        {
            eprintln!("[skip] this adapter has no DUAL_SOURCE_BLENDING");
            return None;
        }
        renderer.tuning = TextTuning { gamma: 1.0, contrast: 0.0 };
        renderer.resize(&device, 4, 4);

        let key = zest_font::GlyphKey {
            font: zest_font::FontId(0),
            glyph: 42,
            px: 13 * 256,
            subpx_x: 0,
            synthetic: 0,
            raster: if mask.format == zest_font::GlyphFormat::SubpixelMask {
                zest_font::RASTER_SUBPIXEL
            } else {
                0
            },
        };
        let cached = renderer.atlas.insert(&device, &queue, key, mask)?;
        let crate::Cached::Entry(e) = cached else { return None };

        let mut scene = Scene { backdrop: LinearRgba::opaque(0, 0, 0), ..Default::default() };
        scene.glyphs.push(crate::GlyphInstance {
            pos: [0.0, 0.0],
            uv: [f32::from(e.uv[0]), f32::from(e.uv[1])],
            size: [f32::from(e.size[0]), f32::from(e.size[1])],
            color: LinearRgba::opaque(0xFF, 0xFF, 0xFF),
            clip: [0.0, 0.0, 4.0, 4.0],
            layer: u32::from(e.layer),
            flags: crate::glyph_flags::FIXED,
        });
        scene.chrome_glyphs_at = scene.glyphs.len();

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zest glyph test target"),
            size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.render(&device, &queue, &mut encoder, &view, &scene);
        queue.submit([encoder.finish()]);

        let px = crate::read_rgba(&device, &queue, &texture, 4, 4, format);
        Some([px[0], px[1], px[2], px[3]])
    }

    #[test]
    fn subpixel_coverage_reaches_the_frame_per_channel() {
        // The property the whole mode rests on: three different coverages go
        // into the atlas and three different values come out of the frame. If
        // this collapses to one number, the mask is being sampled with `.r`
        // again, or the glyph pipeline quietly fell back to the single-alpha
        // blend -- either way the atlas is four times the size for nothing.
        let mask = zest_font::GlyphImage {
            width: 1,
            height: 1,
            left: 0,
            top: 0,
            data: vec![0xFF, 0x80, 0x20, 0x00],
            format: zest_font::GlyphFormat::SubpixelMask,
        };
        let Some(px) = glyph_pixel(zest_font::TextAntialias::Subpixel, &mask) else { return };
        assert!(
            px[0] > px[1] && px[1] > px[2],
            "wanted the frame to keep R > G > B from the mask, got {px:?}"
        );
    }

    #[test]
    fn grayscale_coverage_still_composites_as_one_alpha() {
        // The fallback has to stay exactly what it was: one coverage value,
        // applied equally to all three channels.
        let mask = zest_font::GlyphImage {
            width: 1,
            height: 1,
            left: 0,
            top: 0,
            data: vec![0x80],
            format: zest_font::GlyphFormat::Mask,
        };
        let Some(px) = glyph_pixel(zest_font::TextAntialias::Grayscale, &mask) else { return };
        assert!(px[0] == px[1] && px[1] == px[2], "grayscale must stay neutral, got {px:?}");
        assert!(px[0] > 0, "the glyph drew nothing at all");
    }

    #[test]
    fn an_opaque_backdrop_keeps_the_frame_alpha_at_one() {
        // Why `max(cov)` is a safe choice for the glyph's alpha: under the gate
        // the destination is opaque, so whatever alpha the glyph contributes,
        // the result is 1 and `resolve.wgsl`'s un-premultiply is the identity.
        // If this ever fails, that choice has become observable and the comment
        // in glyph_subpixel.wgsl needs revisiting.
        let mask = zest_font::GlyphImage {
            width: 1,
            height: 1,
            left: 0,
            top: 0,
            data: vec![0xFF, 0x80, 0x20, 0x00],
            format: zest_font::GlyphFormat::SubpixelMask,
        };
        let Some(px) = glyph_pixel(zest_font::TextAntialias::Subpixel, &mask) else { return };
        assert_eq!(px[3], 0xFF, "an opaque backdrop must stay opaque under a glyph");
    }

    #[test]
    fn a_device_without_dual_source_reports_grayscale_so_the_rasterizer_can_follow() {
        // The bug this pins, which Copilot found and no machine with the
        // feature can reproduce: callers set `Fonts` from the *config* while
        // the renderer had already refused subpixel, so four-byte masks were
        // uploaded into a one-byte texture. `text_antialias()` is the answer
        // every caller must read back, so it has to be the effective mode and
        // never the requested one.
        let Some((device, _queue)) = headless() else { return };
        let r = Renderer::new(
            &device,
            wgpu::TextureFormat::Rgba8Unorm,
            zest_font::TextAntialias::Subpixel,
        );
        if r.supports_subpixel_text() {
            assert_eq!(r.text_antialias(), zest_font::TextAntialias::Subpixel);
        } else {
            assert_eq!(
                r.text_antialias(),
                zest_font::TextAntialias::Grayscale,
                "a device without DUAL_SOURCE_BLENDING must report the mode it actually                  has, or every caller that trusts it uploads at the wrong stride"
            );
        }
        assert_eq!(
            r.atlas.text_antialias(),
            r.text_antialias(),
            "the atlas and the renderer must never disagree about coverage"
        );
    }

    #[test]
    fn the_subpixel_module_enables_the_extension_before_anything_else() {
        // `enable` must precede every declaration. If it ever drifts below one,
        // the module stops compiling on every machine at once, at startup --
        // the worst possible moment to find out, and the cheapest possible test.
        let first = super::GRID_WGSL_SUBPIXEL.lines().next().unwrap_or_default().trim();
        assert_eq!(first, "enable dual_source_blending;");
        assert!(
            !super::GRID_WGSL.contains("blend_src"),
            "the grayscale module must not mention @blend_src: naga validates it at \
             type level, so merely containing it fails create_shader_module on every \
             device without DUAL_SOURCE_BLENDING -- exactly the ones the fallback is for"
        );
    }

    #[test]
    fn shader_layer_size_matches_the_atlas() {
        // The glyph shader normalizes UVs by a hardcoded LAYER_SIZE. If the
        // atlas ever changes its layer size, every glyph would sample from the
        // wrong place -- subtly, and only for glyphs past the old bound.
        let src = include_str!("shaders/glyph.wgsl");
        let expected = format!("const LAYER_SIZE: f32 = {:.1};", super::Atlas::layer_size());
        assert!(
            src.contains(&expected),
            "glyph.wgsl LAYER_SIZE is out of sync with atlas::LAYER_SIZE; expected `{expected}`"
        );
    }

    #[test]
    fn the_clear_value_matches_an_instance_of_the_same_colour() {
        // The padding is cleared to the backdrop while the grid is an instance
        // filled with it. Any conversion here — encoding to sRGB, or dividing
        // the alpha back out — would make the two different colours, and the
        // seam would land exactly where the old black border used to be.
        let c = super::LinearRgba::from_srgb(0x0b, 0x0f, 0x1a, 0.8);
        let got = super::clear_color(c);
        let [r, g, b, a] = c.0;
        assert_eq!(
            (got.r, got.g, got.b, got.a),
            (f64::from(r), f64::from(g), f64::from(b), f64::from(a)),
            "the clear must be the instance colour verbatim, premultiplied and linear"
        );
    }

    #[test]
    fn shader_fixed_flag_matches_the_instance_flag() {
        // The vertex shader decides scroll-exemption by this bit. If the Rust
        // constant moves, chrome text silently starts scrolling with the grid
        // -- a bug that only appears once smooth scrolling ships.
        let src = include_str!("shaders/glyph.wgsl");
        let expected = format!("const FLAG_FIXED: u32 = {}u;", super::glyph_flags::FIXED);
        assert!(
            src.contains(&expected),
            "glyph.wgsl FLAG_FIXED is out of sync with glyph_flags::FIXED; expected `{expected}`"
        );
    }

    #[test]
    fn shader_side_bits_match_the_instance_bits() {
        // `border_omit` is a bitmask agreed between two files, and drift is
        // invisible: the border skips a different edge, which reads as a design
        // mistake in whoever's chrome happens to notice rather than as a
        // constant that moved. Same reasoning as FLAG_FIXED above.
        let src = include_str!("shaders/rect.wgsl");
        for (name, value) in [
            ("SIDE_TOP", border_sides::TOP),
            ("SIDE_RIGHT", border_sides::RIGHT),
            ("SIDE_BOTTOM", border_sides::BOTTOM),
            ("SIDE_LEFT", border_sides::LEFT),
        ] {
            let expected = format!("const {name}: u32 = {value}u;");
            assert!(
                src.contains(&expected),
                "rect.wgsl {name} is out of sync with border_sides::{}; expected `{expected}`",
                name.trim_start_matches("SIDE_")
            );
        }
    }
}
