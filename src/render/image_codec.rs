#![forbid(unsafe_code)]

/// Maximum number of pixels (`width * height`) accepted from an untrusted image
/// before decoding.  Guards against decompression bombs: a tiny payload that
/// declares enormous dimensions and would otherwise allocate gigabytes.
/// 64 megapixels ≈ 256 MB at 4 bytes/pixel.
const MAX_IMAGE_PIXELS: u64 = 64_000_000;

/// Maximum heap allocation an individual decoder is permitted while decoding an
/// untrusted image, enforced via [`image::Limits`].
const MAX_IMAGE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// Builds the [`image::Limits`] applied to every untrusted decode path.
fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_ALLOC_BYTES);
    limits
}

/// A single frame of an animated image, with its texture and timing information.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The decoded texture data.
    pub texture: vello::peniko::Image,
    /// The duration of the frame in milliseconds.
    pub duration_ms: u64,
}

/// Represents the status of an asset (e.g. an image) in the loader cache.
#[derive(Debug, Clone)]
pub enum AssetSlot {
    /// The asset is currently loading asynchronously.
    Loading,
    /// The asset is loaded and ready for rendering.
    Ready(AnimatedImage),
    /// The asset failed to load.
    Failed,
}

/// Holds either a single static image or an animation sequence.
#[derive(Debug, Clone)]
pub enum AnimatedImage {
    /// A single, static image.
    Static(vello::peniko::Image),
    /// An animation consisting of multiple frames.
    Animated {
        /// The sequence of animation frames.
        frames: Vec<Frame>,
        /// The total duration of the loop in milliseconds.
        total_duration_ms: u64,
    },
}

impl AnimatedImage {
    /// Gets the width of the image.
    pub fn width(&self) -> u32 {
        match self {
            AnimatedImage::Static(img) => img.width,
            AnimatedImage::Animated { frames, .. } => {
                frames.first().map(|f| f.texture.width).unwrap_or(0)
            }
        }
    }

    /// Gets the height of the image.
    pub fn height(&self) -> u32 {
        match self {
            AnimatedImage::Static(img) => img.height,
            AnimatedImage::Animated { frames, .. } => {
                frames.first().map(|f| f.texture.height).unwrap_or(0)
            }
        }
    }
}

/// Helper to premultiply straight alpha pixels for Vello/Wgpu rendering.
pub fn premultiply_alpha(buffer: &mut [u8]) {
    for pixel in buffer.chunks_exact_mut(4) {
        let alpha = pixel[3] as f32 / 255.0;
        if alpha == 1.0 {
            continue;
        }
        if alpha == 0.0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            continue;
        }
        pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
        pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
        pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }
}

/// Decodes raw image bytes into an `AnimatedImage`, checking for animated GIF, WebP, or APNG formats.
pub fn decode_image_bytes(bytes: &[u8]) -> Option<AnimatedImage> {
    use image::{AnimationDecoder, ImageDecoder};
    use std::io::Cursor;

    // Try GIF Decoder
    let cursor = Cursor::new(bytes);
    if let Ok(mut decoder) = image::codecs::gif::GifDecoder::new(cursor)
        && {
            let _ = decoder.set_limits(decode_limits());
            true
        }
        && let Ok(frames_iter) = decoder.into_frames().collect::<Result<Vec<_>, _>>()
        && frames_iter.len() > 1
    {
        let mut frames = Vec::new();
        let mut total_duration_ms = 0;
        let mut canvas: Option<image::RgbaImage> = None;
        for frame in frames_iter {
            let (num, denom) = frame.delay().numer_denom_ms();
            let rgba_img = frame.into_buffer();

            if canvas.is_none() {
                canvas = Some(rgba_img.clone());
            } else if let Some(ref mut c) = canvas {
                image::imageops::overlay(c, &rgba_img, 0, 0);
            }
            let current_canvas = match canvas.as_ref() {
                Some(c) => c,
                None => continue,
            };
            let width_px = current_canvas.width();
            let height_px = current_canvas.height();

            let mut duration_ms = if denom > 0 {
                (num as u64) / (denom as u64)
            } else {
                100
            };
            if duration_ms == 0 {
                duration_ms = 100;
            }

            let mut raw_buf = current_canvas.clone().into_raw();
            premultiply_alpha(&mut raw_buf);

            let texture = vello::peniko::Image::new(
                vello::peniko::Blob::new(std::sync::Arc::new(raw_buf)),
                vello::peniko::Format::Rgba8,
                width_px,
                height_px,
            );

            frames.push(Frame {
                texture,
                duration_ms,
            });
            total_duration_ms += duration_ms;
        }
        if total_duration_ms > 0 && !frames.is_empty() {
            return Some(AnimatedImage::Animated {
                frames,
                total_duration_ms,
            });
        }
    }

    // Try WebP Decoder
    let cursor = Cursor::new(bytes);
    if let Ok(mut decoder) = image::codecs::webp::WebPDecoder::new(cursor)
        && {
            let _ = decoder.set_limits(decode_limits());
            true
        }
        && let Ok(frames_iter) = decoder.into_frames().collect::<Result<Vec<_>, _>>()
        && frames_iter.len() > 1
    {
        let mut frames = Vec::new();
        let mut total_duration_ms = 0;
        let mut canvas: Option<image::RgbaImage> = None;
        for frame in frames_iter {
            let (num, denom) = frame.delay().numer_denom_ms();
            let rgba_img = frame.into_buffer();

            if canvas.is_none() {
                canvas = Some(rgba_img.clone());
            } else if let Some(ref mut c) = canvas {
                image::imageops::overlay(c, &rgba_img, 0, 0);
            }
            let current_canvas = match canvas.as_ref() {
                Some(c) => c,
                None => continue,
            };
            let width_px = current_canvas.width();
            let height_px = current_canvas.height();

            let mut duration_ms = if denom > 0 {
                (num as u64) / (denom as u64)
            } else {
                100
            };
            if duration_ms == 0 {
                duration_ms = 100;
            }

            let mut raw_buf = current_canvas.clone().into_raw();
            premultiply_alpha(&mut raw_buf);

            let texture = vello::peniko::Image::new(
                vello::peniko::Blob::new(std::sync::Arc::new(raw_buf)),
                vello::peniko::Format::Rgba8,
                width_px,
                height_px,
            );

            frames.push(Frame {
                texture,
                duration_ms,
            });
            total_duration_ms += duration_ms;
        }
        if total_duration_ms > 0 && !frames.is_empty() {
            return Some(AnimatedImage::Animated {
                frames,
                total_duration_ms,
            });
        }
    }

    // Try APNG Decoder
    let cursor = Cursor::new(bytes);
    if let Ok(mut decoder) = image::codecs::png::PngDecoder::new(cursor)
        && {
            let _ = decoder.set_limits(decode_limits());
            true
        }
        && decoder.is_apng().unwrap_or(false)
        && let Ok(apng_decoder) = decoder.apng()
        && let Ok(frames_iter) = apng_decoder.into_frames().collect::<Result<Vec<_>, _>>()
        && frames_iter.len() > 1
    {
        let mut frames = Vec::new();
        let mut total_duration_ms = 0;
        let mut canvas: Option<image::RgbaImage> = None;
        for frame in frames_iter {
            let (num, denom) = frame.delay().numer_denom_ms();
            let rgba_img = frame.into_buffer();

            if canvas.is_none() {
                canvas = Some(rgba_img.clone());
            } else if let Some(ref mut c) = canvas {
                image::imageops::overlay(c, &rgba_img, 0, 0);
            }
            let current_canvas = match canvas.as_ref() {
                Some(c) => c,
                None => continue,
            };
            let width_px = current_canvas.width();
            let height_px = current_canvas.height();

            let mut duration_ms = if denom > 0 {
                (num as u64) / (denom as u64)
            } else {
                100
            };
            if duration_ms == 0 {
                duration_ms = 100;
            }

            let mut raw_buf = current_canvas.clone().into_raw();
            premultiply_alpha(&mut raw_buf);

            let texture = vello::peniko::Image::new(
                vello::peniko::Blob::new(std::sync::Arc::new(raw_buf)),
                vello::peniko::Format::Rgba8,
                width_px,
                height_px,
            );

            frames.push(Frame {
                texture,
                duration_ms,
            });
            total_duration_ms += duration_ms;
        }
        if total_duration_ms > 0 && !frames.is_empty() {
            return Some(AnimatedImage::Animated {
                frames,
                total_duration_ms,
            });
        }
    }

    // Fallback to static — guard against decompression bombs by inspecting the
    // declared dimensions *before* performing a full decode/allocation.
    match image::ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(reader) => match reader.into_dimensions() {
            Ok((w, h)) => {
                if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
                    tracing::error!(
                        width = w,
                        height = h,
                        "image exceeds MAX_IMAGE_PIXELS; rejecting"
                    );
                    return None;
                }
            }
            Err(_) => return None,
        },
        Err(_) => return None,
    }

    if let Ok(img) = image::load_from_memory(bytes) {
        let rgba_img = img.to_rgba8();
        let width_px = rgba_img.width();
        let height_px = rgba_img.height();
        let mut raw_buf = rgba_img.into_raw();
        premultiply_alpha(&mut raw_buf);
        let peniko = vello::peniko::Image::new(
            vello::peniko::Blob::new(std::sync::Arc::new(raw_buf)),
            vello::peniko::Format::Rgba8,
            width_px,
            height_px,
        );
        return Some(AnimatedImage::Static(peniko));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal 40-byte BITMAPINFOHEADER BMP that *declares* the given
    /// dimensions without carrying any pixel data, so a decoder must read the
    /// header to learn the (here, enormous) size.
    fn bmp_header_with_dims(width: i32, height: i32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(54);
        buf.extend_from_slice(b"BM"); // signature
        buf.extend_from_slice(&0u32.to_le_bytes()); // file size (ignored)
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        buf.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        buf.extend_from_slice(&40u32.to_le_bytes()); // DIB header size
        buf.extend_from_slice(&width.to_le_bytes()); // width
        buf.extend_from_slice(&height.to_le_bytes()); // height
        buf.extend_from_slice(&1u16.to_le_bytes()); // planes
        buf.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        buf.extend_from_slice(&0u32.to_le_bytes()); // compression
        buf.extend_from_slice(&0u32.to_le_bytes()); // image size
        buf.extend_from_slice(&0i32.to_le_bytes()); // x ppm
        buf.extend_from_slice(&0i32.to_le_bytes()); // y ppm
        buf.extend_from_slice(&0u32.to_le_bytes()); // colors used
        buf.extend_from_slice(&0u32.to_le_bytes()); // important colors
        buf
    }

    #[test]
    fn decode_rejects_oversized_dimensions() {
        // 100_000 x 100_000 = 1e10 pixels, far above MAX_IMAGE_PIXELS.
        let bomb = bmp_header_with_dims(100_000, 100_000);
        assert!(
            decode_image_bytes(&bomb).is_none(),
            "an image declaring billions of pixels must be rejected before decoding"
        );
    }

    #[test]
    fn decode_rejects_garbage_bytes() {
        // Not a recognisable image at all — must return None, not panic.
        assert!(decode_image_bytes(b"not an image at all").is_none());
    }
}
