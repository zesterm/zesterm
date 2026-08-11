//! The GPU renderer.
//!
//! Three pipelines in one render pass, then a resolve:
//!
//! 1. **SDF rects** — cell backgrounds, selection, cursor, *and* all window
//!    chrome. One pipeline for everything rectangular.
//! 2. **Glyphs** — instanced quads from a dual atlas, serving the grid and UI
//!    text alike.
//! 3. **Decorations** — underline, undercurl, strikethrough.
//!
//! Everything renders into an `Rgba16Float` offscreen target and is resolved to
//! the surface by a fullscreen triangle. See `shaders/resolve.wgsl` for why that
//! indirection earns its place.
//!
//! Draw order is fixed and there is no depth buffer or sorting:
//!
//! ```text
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
pub mod instance;
pub mod scene;
pub mod ui_text;


pub use atlas::{Atlas, AtlasEntry, Cached};
pub use capture::read_rgba;
pub use instance::{
    glyph_flags, DecorInstance, DecorKind, GlyphInstance, Globals, LinearRgba, RectInstance,
    RectShape,
};
pub use scene::{Chrome, Preedit, Scene, Viewport};
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
    /// Whether 1.3 is *right* is still open: ROADMAP asks for a side-by-side
    /// against Windows Terminal, and that is a measurement, not a guess.
    pub const DEFAULT_GAMMA: f32 = 1.3;
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

    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
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

    pub atlas: Atlas,
    pub tuning: TextTuning,
}

impl Renderer {
    /// Build the renderer for a given surface format.
    ///
    /// `target_format` **must not be an sRGB format**. The resolve shader
    /// performs the sRGB encode itself, because premultiplying after a hardware
    /// encode is wrong (see `shaders/resolve.wgsl`). Handing it an `*Srgb`
    /// format would encode twice and wash everything out.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        Self::with_cache(device, target_format, None)
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
            source: wgpu::ShaderSource::Wgsl(
                concat!(
                    include_str!("shaders/common.wgsl"),
                    "\n",
                    include_str!("shaders/rect.wgsl"),
                    "\n",
                    include_str!("shaders/glyph.wgsl"),
                    "\n",
                    include_str!("shaders/decor.wgsl"),
                )
                .into(),
            ),
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
            cache,
        );
        let glyph_pipeline = make_pipeline(
            device,
            "zest glyph",
            &grid_shader,
            ("vs_glyph", "fs_glyph"),
            &[&globals_layout, &atlas_bind_group_layout],
            glyph_vertex_layout(),
            cache,
        );
        let decor_pipeline = make_pipeline(
            device,
            "zest decor",
            &grid_shader,
            ("vs_decor", "fs_decor"),
            &[&globals_layout],
            decor_vertex_layout(),
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

        Self {
            rect_pipeline,
            glyph_pipeline,
            decor_pipeline,
            resolve_pipeline,
            globals_buffer,
            globals_bind_group,
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
            atlas: Atlas::new(device),
            tuning: TextTuning::default(),
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
                    pass.set_pipeline(&self.glyph_pipeline);
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
                        pass.set_pipeline(&self.glyph_pipeline);
                        pass.set_bind_group(1, bg, &[]);
                        pass.set_vertex_buffer(0, self.glyphs.slice());
                        pass.draw(0..4, glyphs);
                    }
                }
            }
        }

        self.resolve(encoder, target);
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

fn make_pipeline(
    device: &wgpu::Device,
    label: &str,
    shader: &wgpu::ShaderModule,
    entry_points: (&str, &str),
    bind_group_layouts: &[&wgpu::BindGroupLayout],
    vertex_layout: wgpu::VertexBufferLayout<'_>,
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
                // Premultiplied source-over, in every pipeline without
                // exception. Mixing this with SrcAlpha/OneMinusSrcAlpha is what
                // produces dark halos around text. -> ADR-003.
                blend: Some(wgpu::BlendState {
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
                }),
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

fn rect_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 10] = wgpu::vertex_attr_array![
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
    use super::{LinearRgba, Renderer, Scene, TextTuning};

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
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("zest test"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        }))
        .ok()
    }

    /// Render a scene with nothing but a backdrop and read one pixel back.
    fn backdrop_pixel(tuning: TextTuning, color: LinearRgba) -> Option<[u8; 4]> {
        let (device, queue) = headless()?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(&device, format);
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
}
