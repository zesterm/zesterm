//! The glyph atlas.
//!
//! # Two textures, one bind group
//!
//! Coverage masks (`R8Unorm`) and colour glyphs (`Rgba8UnormSrgb`) live in
//! separate texture arrays, and an instance flag bit selects which one the
//! fragment shader samples. Splitting emoji into a *second draw call* would be
//! the obvious alternative and it is worse: it either forces a sort by glyph
//! kind or breaks the single-pass ordering the renderer depends on.
//!
//! The colour texture stores **unpremultiplied** values. Premultiplying before
//! an sRGB encode does not survive the round trip — the encode is nonlinear, so
//! `encode(c·a) ≠ encode(c)·a`. The sampler linearizes, and the shader
//! premultiplies afterwards.
//!
//! # Eviction is deliberately boring
//!
//! No per-glyph LRU. Glyphs are immortal within a *generation*, where a
//! generation is (font stack, pixel size, DPI scale). On zoom, a DPI change or a
//! config reload, the generation is bumped and the whole atlas is cleared.
//! Re-rastering the visible working set is a few milliseconds and those events
//! are rare and user-initiated.
//!
//! Per-glyph LRU is where this normally gets over-engineered, and it causes
//! atlas churn on exactly the CJK-heavy workload it was meant to help.

use etagere::{size2, Allocation, AtlasAllocator};
use rustc_hash::FxHashMap;
use zest_font::{GlyphImage, GlyphKey};

/// Side length of one atlas layer, in texels.
///
/// 2048 holds several thousand glyphs at typical sizes, which covers ASCII plus
/// a working set of CJK without ever adding a layer.
const LAYER_SIZE: u32 = 2048;

/// Layers to allocate up front. Growing means recreating the texture and
/// copying, so a little headroom is cheaper than the copy.
const INITIAL_LAYERS: u32 = 1;

/// Padding around each glyph, in texels.
///
/// Without it, linear sampling at a glyph's edge bleeds in its neighbour, which
/// shows up as faint smears on adjacent characters. One texel is enough because
/// sampling is never magnified.
const PADDING: i32 = 1;

/// Where a glyph landed in the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasEntry {
    /// Texel coordinates of the glyph's top-left within its layer.
    pub uv: [u16; 2],
    pub size: [u16; 2],
    pub layer: u16,
    /// Offset from the pen position to the bitmap's left edge.
    pub left: i16,
    /// Offset from the baseline *up* to the bitmap's top edge.
    pub top: i16,
    /// Sample the colour texture rather than the coverage mask.
    pub is_color: bool,
}

/// A glyph that occupies no pixels — a space, most commonly.
///
/// Cached distinctly from "not present" so a space is not re-rasterized on
/// every frame, which at 300x100 is most of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cached {
    Entry(AtlasEntry),
    Empty,
}

pub struct Atlas {
    mask_texture: wgpu::Texture,
    mask_view: wgpu::TextureView,
    color_texture: wgpu::Texture,
    color_view: wgpu::TextureView,
    sampler: wgpu::Sampler,

    /// One allocator per layer, per texture kind.
    mask_allocators: Vec<AtlasAllocator>,
    color_allocators: Vec<AtlasAllocator>,

    entries: FxHashMap<GlyphKey, Cached>,
    /// Bumped to invalidate everything at once.
    generation: u64,
    layers: u32,
}

impl Atlas {
    pub fn new(device: &wgpu::Device) -> Self {
        let (mask_texture, mask_view) = make_texture(
            device,
            "zest atlas: coverage masks",
            wgpu::TextureFormat::R8Unorm,
            INITIAL_LAYERS,
        );
        let (color_texture, color_view) = make_texture(
            device,
            "zest atlas: colour glyphs",
            // sRGB so the sampler linearizes for us. Values stored here are
            // NOT premultiplied; see the module docs.
            wgpu::TextureFormat::Rgba8UnormSrgb,
            INITIAL_LAYERS,
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("zest atlas sampler"),
            // Glyphs are blitted at exactly 1:1, so nearest is both correct and
            // sharper. Linear would soften every edge for no benefit.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        Self {
            mask_texture,
            mask_view,
            color_texture,
            color_view,
            sampler,
            mask_allocators: (0..INITIAL_LAYERS).map(|_| new_allocator()).collect(),
            color_allocators: (0..INITIAL_LAYERS).map(|_| new_allocator()).collect(),
            entries: FxHashMap::default(),
            generation: 0,
            layers: INITIAL_LAYERS,
        }
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn get(&self, key: &GlyphKey) -> Option<Cached> {
        self.entries.get(key).copied()
    }

    /// Discard every glyph and start a new generation.
    ///
    /// Called on font size, family, DPI or hinting changes — anything that makes
    /// existing bitmaps wrong rather than merely stale.
    pub fn clear(&mut self) {
        self.entries.clear();
        for a in &mut self.mask_allocators {
            a.clear();
        }
        for a in &mut self.color_allocators {
            a.clear();
        }
        self.generation += 1;
    }

    /// Insert a rasterized glyph, uploading it to the GPU.
    ///
    /// Returns `None` only when the atlas is genuinely full and cannot grow, in
    /// which case the caller should render nothing for this glyph rather than
    /// fail the frame.
    pub fn insert(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: GlyphKey,
        image: &GlyphImage,
    ) -> Option<Cached> {
        if let Some(existing) = self.entries.get(&key) {
            return Some(*existing);
        }

        if image.is_empty() {
            self.entries.insert(key, Cached::Empty);
            return Some(Cached::Empty);
        }

        let (w, h) = (image.width, image.height);
        let padded = size2(w as i32 + PADDING * 2, h as i32 + PADDING * 2);

        let (layer, alloc) = self.allocate(device, queue, padded, image.is_color)?;
        let x = (alloc.rectangle.min.x + PADDING) as u32;
        let y = (alloc.rectangle.min.y + PADDING) as u32;

        let (texture, bytes_per_pixel) = if image.is_color {
            (&self.color_texture, 4u32)
        } else {
            (&self.mask_texture, 1u32)
        };

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: layer },
                aspect: wgpu::TextureAspect::All,
            },
            &image.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * bytes_per_pixel),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );

        let entry = Cached::Entry(AtlasEntry {
            uv: [x as u16, y as u16],
            size: [w as u16, h as u16],
            layer: layer as u16,
            left: image.left.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            top: image.top.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            is_color: image.is_color,
        });
        self.entries.insert(key, entry);
        Some(entry)
    }

    /// Find room, adding a layer if every existing one is full.
    fn allocate(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: etagere::Size,
        is_color: bool,
    ) -> Option<(u32, Allocation)> {
        // A glyph larger than a whole layer cannot ever fit. Reject rather than
        // looping forever adding layers.
        if size.width > LAYER_SIZE as i32 || size.height > LAYER_SIZE as i32 {
            tracing::warn!(?size, "glyph is larger than an atlas layer; skipping");
            return None;
        }

        let allocators =
            if is_color { &mut self.color_allocators } else { &mut self.mask_allocators };
        for (i, a) in allocators.iter_mut().enumerate() {
            if let Some(alloc) = a.allocate(size) {
                return Some((i as u32, alloc));
            }
        }

        // Full. Grow both textures together so layer indices stay in step.
        self.grow(device, queue)?;
        let allocators =
            if is_color { &mut self.color_allocators } else { &mut self.mask_allocators };
        let last = allocators.len() - 1;
        allocators[last].allocate(size).map(|a| (last as u32, a))
    }

    /// Add a layer, preserving existing content.
    fn grow(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<()> {
        // Four layers is 16M glyph texels per kind. Past that, something is
        // wrong -- more likely a leak than a genuine working set -- and
        // clearing is a better outcome than unbounded VRAM growth.
        const MAX_LAYERS: u32 = 4;
        if self.layers >= MAX_LAYERS {
            tracing::warn!("atlas is full at {MAX_LAYERS} layers; clearing");
            self.clear();
            return Some(());
        }

        let new_layers = self.layers + 1;
        let (mask_texture, mask_view) = make_texture(
            device,
            "zest atlas: coverage masks",
            wgpu::TextureFormat::R8Unorm,
            new_layers,
        );
        let (color_texture, color_view) = make_texture(
            device,
            "zest atlas: colour glyphs",
            wgpu::TextureFormat::Rgba8UnormSrgb,
            new_layers,
        );

        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zest atlas grow"),
        });
        for (src, dst) in [
            (&self.mask_texture, &mask_texture),
            (&self.color_texture, &color_texture),
        ] {
            enc.copy_texture_to_texture(
                src.as_image_copy(),
                dst.as_image_copy(),
                wgpu::Extent3d {
                    width: LAYER_SIZE,
                    height: LAYER_SIZE,
                    depth_or_array_layers: self.layers,
                },
            );
        }
        queue.submit([enc.finish()]);

        self.mask_texture = mask_texture;
        self.mask_view = mask_view;
        self.color_texture = color_texture;
        self.color_view = color_view;
        self.mask_allocators.push(new_allocator());
        self.color_allocators.push(new_allocator());
        self.layers = new_layers;
        Some(())
    }

    /// Bind group entries: mask texture, colour texture, sampler.
    pub fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zest atlas layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        })
    }

    pub fn bind_group(
        &self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zest atlas"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.color_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    /// Texels per side of a layer, for the shader's UV normalization.
    #[must_use]
    pub const fn layer_size() -> f32 {
        LAYER_SIZE as f32
    }
}

fn new_allocator() -> AtlasAllocator {
    AtlasAllocator::new(size2(LAYER_SIZE as i32, LAYER_SIZE as i32))
}

fn make_texture(
    device: &wgpu::Device,
    label: &str,
    format: wgpu::TextureFormat,
    layers: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: LAYER_SIZE,
            height: LAYER_SIZE,
            depth_or_array_layers: layers,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    (texture, view)
}
