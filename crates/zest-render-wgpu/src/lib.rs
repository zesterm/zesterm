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
pub mod instance;
pub mod scene;

use wgpu::util::DeviceExt;

pub use atlas::{Atlas, AtlasEntry, Cached};
pub use instance::{
    glyph_flags, DecorInstance, DecorKind, GlyphInstance, Globals, LinearRgba, RectInstance,
    RectShape,
};
pub use scene::{Chrome, Scene, Viewport};

/// The offscreen format.
///
/// Float, so blending has headroom and the resolve can un-premultiply without
/// quantization damage. 8-bit would lose precision exactly where text
/// antialiasing needs it.
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Text rendering knobs, applied once at resolve rather than baked per glyph.
#[derive(Debug, Clone, Copy)]
pub struct TextTuning {
    /// Stem darkening. Light-on-dark needs meaningfully more than dark-on-light,
    /// which is why it is a knob and why themes can suggest a default.
    pub gamma: f32,
    pub contrast: f32,
}

impl Default for TextTuning {
    fn default() -> Self {
        Self { gamma: 1.3, contrast: 0.0 }
    }
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
    resolve_params: wgpu::Buffer,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
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

        let resolve_params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("zest resolve params"),
            contents: bytemuck::cast_slice(&[1.3f32, 0.0, 0.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
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
            resolve_params,
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
                wgpu::BindGroupEntry { binding: 2, resource: self.resolve_params.as_entire_binding() },
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
        queue.write_buffer(
            &self.resolve_params,
            0,
            bytemuck::cast_slice(&[self.tuning.gamma, self.tuning.contrast, 0.0, 0.0]),
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
                        // Transparent clear. Opacity is expressed by the
                        // instances themselves, so the target starts empty.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            pass.set_bind_group(0, &self.globals_bind_group, &[]);

            if !scene.rects.is_empty() {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, self.rects.slice());
                pass.draw(0..4, 0..scene.rects.len() as u32);
            }

            if !scene.glyphs.is_empty() {
                if let Some(bg) = self.atlas_bind_group.as_ref() {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_bind_group(1, bg, &[]);
                    pass.set_vertex_buffer(0, self.glyphs.slice());
                    pass.draw(0..4, 0..scene.glyphs.len() as u32);
                }
            }

            if !scene.decors.is_empty() {
                pass.set_pipeline(&self.decor_pipeline);
                pass.set_vertex_buffer(0, self.decors.slice());
                pass.draw(0..4, 0..scene.decors.len() as u32);
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
}
