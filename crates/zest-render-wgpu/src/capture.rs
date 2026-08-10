//! Reading a rendered texture back to RGBA bytes.
//!
//! The renderer draws into a texture; a screenshot needs those pixels on the
//! CPU, in an order an encoder understands. Two details make that less trivial
//! than it sounds, and both used to live inline in `render_dump` where the app
//! could not reach them:
//!
//! 1. **Rows in a mapped buffer are aligned to 256 bytes.** The copy is padded
//!    and has to be compacted afterwards. Skip it and every image after the
//!    first few pixels of row one is skewed diagonally.
//! 2. **The surface format is not always RGBA.** The window's surface is chosen
//!    for the platform and is `Bgra8Unorm` on most of them, while every PNG
//!    encoder wants RGBA. Getting this wrong swaps red and blue, which looks
//!    like a theme bug rather than a readback bug — the one failure mode that
//!    sends you looking in the wrong crate.

/// RGBA8 bytes of `texture`, tightly packed, `width * height * 4` long.
///
/// `format` is the texture's own format; the channels are reordered when it is
/// BGRA. Blocks until the GPU is done, which is the point — a screenshot has
/// nothing else to do.
///
/// # Panics
///
/// If `format` is not an 8-bit four-channel one, loudly and before touching the
/// GPU. Every size here is `width * 4`, so a 16-bit format would read half the
/// image and compact the halves into noise — and [`OFFSCREEN_FORMAT`] is
/// `Rgba16Float`, two modules away, which makes that the *likely* mistake
/// rather than a hypothetical one. Failing on the signature is worth more than
/// a plausible-looking wrong picture.
///
/// Also if the copy cannot be mapped. There is no useful recovery: the caller
/// asked for pixels and there are none.
///
/// [`OFFSCREEN_FORMAT`]: crate::OFFSCREEN_FORMAT
pub fn read_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> Vec<u8> {
    let bgra = channel_swap(format).unwrap_or_else(|| {
        panic!(
            "read_rgba handles 8-bit RGBA/BGRA only, not {format:?}. Every stride \
             here is width*4, so a wider format would be read as half an image \
             and compacted into noise."
        )
    });

    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zest readback"),
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
    let out = compact_rows(&view, width, height, padded, bgra);
    drop(view);
    buffer.unmap();
    out
}

/// Whether a format needs its first and third channels swapped to become RGBA —
/// or `None` when it is not an 8-bit four-channel format at all.
///
/// The `None` arm is the load-bearing one: it is what turns "someone passed the
/// offscreen's `Rgba16Float`" from a picture that looks subtly wrong into a
/// panic naming the format.
#[must_use]
pub fn channel_swap(format: wgpu::TextureFormat) -> Option<bool> {
    use wgpu::TextureFormat as F;
    match format {
        F::Rgba8Unorm | F::Rgba8UnormSrgb => Some(false),
        F::Bgra8Unorm | F::Bgra8UnormSrgb => Some(true),
        _ => None,
    }
}

/// Drop the row padding, and swap R and B when the source is BGRA.
///
/// Split out from the mapping so the arithmetic — the part that is actually
/// easy to get wrong — is testable without a GPU, and therefore on every CI
/// runner rather than only on ones with an adapter.
#[must_use]
fn compact_rows(padded_rows: &[u8], width: u32, height: u32, stride: u32, bgra: bool) -> Vec<u8> {
    let unpadded = (width * 4) as usize;
    let mut out = Vec::with_capacity(unpadded * height as usize);
    for row in 0..height {
        let start = (row * stride) as usize;
        out.extend_from_slice(&padded_rows[start..start + unpadded]);
    }
    if bgra {
        for px in out.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_between_rows_is_dropped() {
        // 2x2 RGBA is 8 bytes a row, but the copy arrives on a 16-byte stride.
        // Carrying the padding through shifts every row after the first and the
        // image comes out sheared — which reads as a renderer bug, not a
        // readback one.
        let stride = 16;
        let mut src = vec![0u8; stride * 2];
        src[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src[16..24].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        let got = compact_rows(&src, 2, 2, stride as u32, false);
        assert_eq!(got, (1..=16).collect::<Vec<u8>>(), "rows are packed back to back");
    }

    #[test]
    fn bgra_sources_come_back_as_rgba() {
        // The window's surface is BGRA on most platforms. Without the swap the
        // screenshot has red and blue exchanged, which looks exactly like a
        // theme or palette bug and sends you hunting in the wrong crate.
        let bgra = vec![0xB0, 0x60, 0x0A, 0xFF];
        let got = compact_rows(&bgra, 1, 1, 4, true);
        assert_eq!(got, vec![0x0A, 0x60, 0xB0, 0xFF], "R and B swap, G and A stay put");
    }

    #[test]
    fn alpha_survives_the_round_trip() {
        // A translucent window (`window.opacity < 1`) is exactly the case a
        // screenshot has to show honestly, so alpha must not be forced opaque.
        let got = compact_rows(&[1, 2, 3, 0x80], 1, 1, 4, false);
        assert_eq!(got[3], 0x80);
    }

    #[test]
    fn only_bgra_formats_are_swapped() {
        assert_eq!(channel_swap(wgpu::TextureFormat::Bgra8Unorm), Some(true));
        assert_eq!(channel_swap(wgpu::TextureFormat::Bgra8UnormSrgb), Some(true));
        assert_eq!(channel_swap(wgpu::TextureFormat::Rgba8Unorm), Some(false));
        assert_eq!(channel_swap(wgpu::TextureFormat::Rgba8UnormSrgb), Some(false));
    }

    #[test]
    fn a_format_this_cannot_read_is_refused_rather_than_guessed() {
        // `OFFSCREEN_FORMAT` is `Rgba16Float` and lives two modules away, so
        // handing it to a function whose every stride is `width * 4` is the
        // mistake actually available to make. Reading half the image and
        // compacting the halves would produce a plausible-looking wrong
        // picture; `None` here is what makes it a panic naming the format.
        assert_eq!(channel_swap(crate::OFFSCREEN_FORMAT), None, "the offscreen is not readable here");
        assert_eq!(channel_swap(wgpu::TextureFormat::R8Unorm), None, "nor is a one-channel format");
    }
}
