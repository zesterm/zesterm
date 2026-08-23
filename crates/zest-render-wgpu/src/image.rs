//! Background pictures: the texture store and the placement maths.
//!
//! Decoding lives one layer up, in `zest-app`. This crate takes RGBA8 bytes and
//! a size, which is what keeps a PNG/JPEG/WebP decoder — and its long tail of
//! format crates — out of the renderer's dependency list entirely.

use rustc_hash::FxHashMap;

/// A picture already uploaded to the GPU.
///
/// Minted by the caller, not by the store: `zest-app` keys its cache on
/// `(path, mtime, len)` and hashes that, so the same file resolved twice is one
/// texture and an edited file is a different id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub u64);

/// How a picture is placed inside the pane it decorates.
///
/// The three the client-UI handoff specifies (§12), and no more: a fourth
/// variant would stop the settings screen rendering this as a segmented control
/// (`select_is_segmented` draws a `Select` of three or fewer that way), which is
/// what the design asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundFit {
    /// Scale to cover the pane, cropping the overflow. The default: a photo
    /// with a letterbox around it reads as a bug rather than as a choice.
    #[default]
    Fill,
    /// Scale to fit inside the pane; the slack is the plain window background.
    Fit,
    /// Natural size, in the bottom-right corner. The design's recommendation —
    /// "a watermark in the corner reads better than a full-bleed photo behind
    /// text" — and the only mode that leaves most of the grid untouched.
    Watermark,
}

/// A picture riding one viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundImage {
    pub image: ImageId,
    /// The picture's own size in texels, for the placement maths.
    pub size: [u32; 2],
    pub fit: BackgroundFit,
    /// How far the picture is faded toward the window background. `0` shows it
    /// as it is; `1` is indistinguishable from having no picture at all.
    pub dim: f32,
}

/// Margin around a watermark, as a fraction of the pane's shorter axis.
///
/// Proportional rather than a pixel count so it survives a DPI change and a
/// split pane without a second knob.
const WATERMARK_MARGIN: f32 = 0.04;

/// Where the picture lands inside a pane: `x, y, w, h` in pixels, relative to
/// the pane's own origin.
///
/// May extend outside the pane (that is what [`BackgroundFit::Fill`] cropping
/// *is*) — the shader treats everything outside as "no picture here", so the
/// caller does not have to clamp.
#[must_use]
pub fn source_rect(fit: BackgroundFit, image_px: [u32; 2], dest_px: [f32; 2]) -> [f32; 4] {
    let (iw, ih) = (image_px[0].max(1) as f32, image_px[1].max(1) as f32);
    let (dw, dh) = (dest_px[0].max(0.0), dest_px[1].max(0.0));

    match fit {
        BackgroundFit::Fill | BackgroundFit::Fit => {
            let sx = dw / iw;
            let sy = dh / ih;
            let scale = if fit == BackgroundFit::Fill { sx.max(sy) } else { sx.min(sy) };
            let (w, h) = (iw * scale, ih * scale);
            // Centred in both modes: Fill crops equally at both edges rather
            // than always losing the same side, and Fit's letterbox is even.
            [(dw - w) * 0.5, (dh - h) * 0.5, w, h]
        }
        BackgroundFit::Watermark => {
            let margin = dw.min(dh) * WATERMARK_MARGIN;
            let room_w = (dw - margin * 2.0).max(0.0);
            let room_h = (dh - margin * 2.0).max(0.0);
            // Never upscaled — a watermark is the picture at its own size — but
            // shrunk when it would not otherwise fit, because a corner mark
            // that covers the whole pane is a full-bleed photo by another name.
            let scale = (room_w / iw).min(room_h / ih).min(1.0);
            let (w, h) = (iw * scale, ih * scale);
            [dw - margin - w, dh - margin - h, w, h]
        }
    }
}

struct Stored {
    bind_group: wgpu::BindGroup,
    size: [u32; 2],
}

/// The uploaded pictures, one texture each.
///
/// Not the glyph atlas: a wallpaper is megapixels of colour with no reason to
/// share a shelf with 40x40 coverage masks, and packing one into `etagere`
/// would evict every glyph on screen to make room.
pub struct ImageStore {
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    stored: FxHashMap<ImageId, Stored>,
}

impl ImageStore {
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zest image layout"),
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("zest image sampler"),
            // Linear, unlike the atlas's Nearest: a picture is resampled to an
            // arbitrary size, where a glyph is blitted at the size it was
            // rasterized for.
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        Self { layout, sampler, stored: FxHashMap::default() }
    }

    #[must_use]
    pub fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    #[must_use]
    pub fn contains(&self, id: ImageId) -> bool {
        self.stored.contains_key(&id)
    }

    /// The bind group for a picture, and its size in texels.
    #[must_use]
    pub fn get(&self, id: ImageId) -> Option<(&wgpu::BindGroup, [u32; 2])> {
        self.stored.get(&id).map(|s| (&s.bind_group, s.size))
    }

    /// Upload one picture, replacing any previous texture under the same id.
    ///
    /// `rgba` is straight-alpha RGBA8 in **sRGB**, exactly what an image decoder
    /// produces. The texture is created as `Rgba8UnormSrgb` so the sampler
    /// linearizes for free — the offscreen is linear `Rgba16Float`, and a
    /// photograph is colour rather than coverage, which is the distinction
    /// ADR-010 draws when it insists the atlas's masks are *not* sRGB views.
    ///
    /// Returns `false` and uploads nothing when the byte count does not match
    /// the size, which is the only way a caller can get this wrong.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: ImageId,
        size: [u32; 2],
        rgba: &[u8],
    ) -> bool {
        let (w, h) = (size[0], size[1]);
        if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
            tracing::warn!(
                width = w,
                height = h,
                bytes = rgba.len(),
                "a background picture's byte count does not match its size; not uploading"
            );
            return false;
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zest background image"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zest background image"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.stored.insert(id, Stored { bind_group, size });
        true
    }

    /// Drop every picture whose id `keep` rejects.
    ///
    /// The whole eviction policy: the set of live pictures is the set of
    /// configured ones, which is at most one per profile. An LRU here would be
    /// machinery guarding a handful of textures.
    pub fn retain(&mut self, keep: impl Fn(ImageId) -> bool) {
        self.stored.retain(|id, _| keep(*id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn about(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn fill_covers_the_pane_and_crops_the_long_axis() {
        // A wide picture in a square pane: scaled until the *short* axis
        // covers, so the width overhangs equally at both ends. Getting this
        // backwards leaves a letterbox in a mode whose whole job is not to.
        let r = source_rect(BackgroundFit::Fill, [200, 100], [100.0, 100.0]);
        assert!(about(r[2], 200.0) && about(r[3], 100.0), "scaled by height: {r:?}");
        assert!(about(r[0], -50.0), "cropped equally at both ends: {r:?}");
        assert!(about(r[1], 0.0), "the short axis fits exactly: {r:?}");
    }

    #[test]
    fn fit_never_crops_and_centres_the_slack() {
        let r = source_rect(BackgroundFit::Fit, [200, 100], [100.0, 100.0]);
        assert!(about(r[2], 100.0) && about(r[3], 50.0), "scaled by width: {r:?}");
        assert!(about(r[0], 0.0) && about(r[1], 25.0), "letterboxed evenly: {r:?}");
    }

    #[test]
    fn a_portrait_picture_is_the_mirror_of_a_landscape_one() {
        // The two branches of `min`/`max` are easy to write once and get right
        // for one aspect only, so both orientations are asserted.
        let wide = source_rect(BackgroundFit::Fit, [200, 100], [100.0, 100.0]);
        let tall = source_rect(BackgroundFit::Fit, [100, 200], [100.0, 100.0]);
        assert!(about(tall[2], wide[3]) && about(tall[3], wide[2]), "{wide:?} vs {tall:?}");
        assert!(about(tall[0], wide[1]) && about(tall[1], wide[0]), "{wide:?} vs {tall:?}");
    }

    #[test]
    fn a_watermark_keeps_its_own_size_in_the_bottom_right() {
        let r = source_rect(BackgroundFit::Watermark, [40, 20], [200.0, 200.0]);
        assert!(about(r[2], 40.0) && about(r[3], 20.0), "never upscaled: {r:?}");
        let margin = 200.0 * WATERMARK_MARGIN;
        assert!(about(r[0], 200.0 - margin - 40.0), "against the right edge: {r:?}");
        assert!(about(r[1], 200.0 - margin - 20.0), "against the bottom edge: {r:?}");
    }

    #[test]
    fn a_watermark_larger_than_the_pane_shrinks_rather_than_bleeding() {
        // The one case where "natural size" cannot be honoured. Left alone it
        // is a full-bleed photo wearing the watermark label, which is the exact
        // thing the mode exists to avoid.
        let r = source_rect(BackgroundFit::Watermark, [400, 400], [100.0, 100.0]);
        let room = 100.0 - 100.0 * WATERMARK_MARGIN * 2.0;
        assert!(about(r[2], room) && about(r[3], room), "shrunk to the margin box: {r:?}");
    }

    #[test]
    fn a_degenerate_size_places_something_finite() {
        // A zero-sized pane happens for one frame during a resize, and a
        // zero-sized picture is what a corrupt file decodes to. Neither may
        // produce a NaN: one reaching the vertex stage is a quad at infinity,
        // which on some drivers is a hang rather than a wrong pixel.
        for r in [
            source_rect(BackgroundFit::Fill, [0, 0], [100.0, 100.0]),
            source_rect(BackgroundFit::Fit, [10, 10], [0.0, 0.0]),
            source_rect(BackgroundFit::Watermark, [0, 10], [0.0, 100.0]),
        ] {
            assert!(r.iter().all(|v| v.is_finite()), "{r:?}");
        }
    }
}
