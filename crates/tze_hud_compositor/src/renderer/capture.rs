//! On-demand readback of the HUD's own frame (`GET /admin/screenshot`).
//!
//! The windowed swapchain is `RENDER_ATTACHMENT` only, so a capture re-encodes
//! an already-built frame into an offscreen texture in the swapchain's format
//! and copies it to a buffer once. Nothing here runs unless an operator asks:
//! there is no per-frame readback, and the swapchain is never touched.

use super::frame::WindowedFrameBuild;
use super::*;

/// Largest width or height a capture will allocate for.
pub const MAX_CAPTURE_DIM: u32 = 8192;

/// A captured frame: tightly packed straight-from-the-GPU RGBA8 (row-major,
/// no padding). In sRGB formats the bytes are sRGB-encoded, as on screen; in
/// overlay mode alpha is as composited (premultiplied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("surface {width}x{height} exceeds the {MAX_CAPTURE_DIM}px capture limit")]
    TooLarge { width: u32, height: u32 },
    #[error("surface format {0:?} cannot be captured as RGBA8")]
    UnsupportedFormat(wgpu::TextureFormat),
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

/// Whether `format` stores bytes as B,G,R,A (`Ok(true)`) or R,G,B,A
/// (`Ok(false)`); anything else is not capturable.
pub fn format_is_bgra(format: wgpu::TextureFormat) -> Result<bool, CaptureError> {
    use wgpu::TextureFormat::*;
    match format {
        Bgra8Unorm | Bgra8UnormSrgb => Ok(true),
        Rgba8Unorm | Rgba8UnormSrgb => Ok(false),
        other => Err(CaptureError::UnsupportedFormat(other)),
    }
}

/// Drop the per-row padding wgpu requires and, for BGRA, swap to RGBA.
pub fn unpad_to_rgba(
    padded: &[u8],
    width: u32,
    height: u32,
    bytes_per_row: u32,
    bgra: bool,
) -> Vec<u8> {
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * bytes_per_row as usize;
        out.extend_from_slice(&padded[start..start + row]);
    }
    if bgra {
        for px in out.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    out
}

impl Compositor {
    /// Render a built frame offscreen in `format` and read it back.
    ///
    /// Blocks the calling (compositor) thread for one submit and map. Does not
    /// acquire or present a swapchain image and does not advance any render
    /// gate; the caller owns that decision.
    pub fn capture_windowed_frame(
        &mut self,
        build: WindowedFrameBuild,
        format: wgpu::TextureFormat,
    ) -> Result<CapturedFrame, CaptureError> {
        let (width, height) = build.size();
        if width > MAX_CAPTURE_DIM || height > MAX_CAPTURE_DIM {
            return Err(CaptureError::TooLarge { width, height });
        }
        let bgra = format_is_bgra(format)?;

        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("admin_capture_target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let (mut encoder, _) = self.encode_windowed_passes(&build, &view);

        let bytes_per_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("admin_capture_readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|e| CaptureError::Readback(e.to_string()))?
            .map_err(|e| CaptureError::Readback(e.to_string()))?;
        let rgba = unpad_to_rgba(
            &slice.get_mapped_range(),
            width,
            height,
            bytes_per_row,
            bgra,
        );
        buffer.unmap();
        Ok(CapturedFrame {
            width,
            height,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpad_drops_row_padding_and_swizzles_bgra() {
        // 2x2 BGRA, rows padded to 12 bytes with 0xEE.
        let padded = [
            1, 2, 3, 4, 5, 6, 7, 8, 0xEE, 0xEE, 0xEE, 0xEE, //
            9, 10, 11, 12, 13, 14, 15, 16, 0xEE, 0xEE, 0xEE, 0xEE,
        ];
        assert_eq!(
            unpad_to_rgba(&padded, 2, 2, 12, true),
            [3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );
        assert_eq!(
            unpad_to_rgba(&padded, 2, 2, 12, false),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn only_8_bit_four_channel_formats_are_capturable() {
        use wgpu::TextureFormat as F;
        assert_eq!(format_is_bgra(F::Bgra8UnormSrgb), Ok(true));
        assert_eq!(format_is_bgra(F::Rgba8Unorm), Ok(false));
        assert!(format_is_bgra(F::Rgba16Float).is_err());
    }
}
