#![forbid(unsafe_code)]

/// Maximum number of pixels (`width * height`) accepted from an untrusted image
/// before decoding.  Guards against decompression bombs: a tiny payload that
/// declares enormous dimensions and would otherwise allocate gigabytes.
/// 64 megapixels ≈ 256 MB at 4 bytes/pixel.
const MAX_IMAGE_PIXELS: u64 = 64_000_000;

/// Maximum heap allocation an individual decoder is permitted while decoding an
/// untrusted image, enforced via [`image::Limits`].
const MAX_IMAGE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum total pixels across every frame of an animated image
/// (`width * height * frame_count`).
///
/// `MAX_IMAGE_PIXELS`/`max_alloc` only bound a *single* frame's canvas. An
/// animation with a modest, under-cap canvas but tens of thousands of frames
/// still allocates one full canvas-sized RGBA buffer per frame in the loop
/// below (for compositing and for the frame's own texture), so the
/// uncapped total is `canvas_pixels * frame_count` — unbounded by any
/// per-frame check. A small file (well under the network layer's 32 MiB
/// transfer cap) can therefore still exhaust process memory. Same budget as
/// a single static image: an animation is not allowed to cost more in total
/// than the largest static image this codec already accepts.
const MAX_ANIMATION_TOTAL_PIXELS: u64 = MAX_IMAGE_PIXELS;

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
        && {
            // Same declared-dimensions check as the WebP branch below: reject
            // an oversized canvas before any frame is decoded.
            let (w, h) = decoder.dimensions();
            if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
                tracing::error!(
                    width = w,
                    height = h,
                    "GIF declared canvas exceeds MAX_IMAGE_PIXELS; rejecting"
                );
                false
            } else {
                true
            }
        }
        && let Ok(frames_iter) = decoder.into_frames().collect::<Result<Vec<_>, _>>()
        && frames_iter.len() > 1
    {
        let mut frames = Vec::new();
        let mut total_duration_ms = 0;
        let mut total_pixels: u64 = 0;
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

            // Cumulative budget across every frame decoded so far: a
            // modest, under-cap canvas repeated over tens of thousands of
            // frames must not be allowed to allocate unbounded total
            // memory. See `MAX_ANIMATION_TOTAL_PIXELS`.
            total_pixels = total_pixels.saturating_add((width_px as u64) * (height_px as u64));
            if total_pixels > MAX_ANIMATION_TOTAL_PIXELS {
                tracing::error!(
                    frames_decoded = frames.len(),
                    "GIF animation exceeds MAX_ANIMATION_TOTAL_PIXELS; rejecting"
                );
                return None;
            }

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
    //
    // `image::codecs::webp::WebPDecoder` (image 0.25.10) does not override
    // `ImageDecoder::set_limits` — it inherits the trait's default
    // implementation, which only checks `max_image_width`/`max_image_height`
    // (both left `None` by `decode_limits()`, which only sets `max_alloc`)
    // and never inspects `max_alloc` at all. Unlike GIF/PNG (whose decoders
    // *do* override `set_limits` and enforce `max_alloc` internally via
    // `Limits::reserve_usize`/`free_usize` during frame decode), the
    // `decoder.set_limits(decode_limits())` call below is a no-op for WebP:
    // it neither rejects nor tracks anything. The generic fallback path
    // further down is safe only because `image::ImageReader::decode()`
    // separately calls `limits.reserve(decoder.total_bytes())` *before*
    // decoding — but this manual, low-level `WebPDecoder::new()` +
    // `into_frames()` usage bypasses `ImageReader` entirely, so that check
    // never runs either. The underlying `image-webp` crate caps a single
    // frame's declared dimensions at 16384x16384 (~268M pixels, over 4x
    // `MAX_IMAGE_PIXELS`), and `into_frames()` allocates a full canvas-sized
    // buffer per frame with no cumulative budget across frames — so without
    // the explicit check immediately below, a small file could still declare
    // an enormous canvas (or many frames against a large-but-under-cap
    // canvas) and exhaust memory. Hence the same declared-dimensions check
    // used for the generic static path is applied here explicitly, before
    // any frame is decoded.
    let cursor = Cursor::new(bytes);
    if let Ok(mut decoder) = image::codecs::webp::WebPDecoder::new(cursor)
        && {
            let _ = decoder.set_limits(decode_limits());
            true
        }
        && {
            let (w, h) = decoder.dimensions();
            if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
                tracing::error!(
                    width = w,
                    height = h,
                    "WebP declared canvas exceeds MAX_IMAGE_PIXELS; rejecting"
                );
                false
            } else {
                true
            }
        }
        && let Ok(frames_iter) = decoder.into_frames().collect::<Result<Vec<_>, _>>()
        && frames_iter.len() > 1
    {
        let mut frames = Vec::new();
        let mut total_duration_ms = 0;
        let mut total_pixels: u64 = 0;
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

            // Cumulative budget across every frame decoded so far — see
            // `MAX_ANIMATION_TOTAL_PIXELS`. The per-frame dimension check
            // above only bounds a single frame; WebP's decoder additionally
            // never enforces `max_alloc` at all (see the doc comment above),
            // so this is the only bound on the total for this format.
            total_pixels = total_pixels.saturating_add((width_px as u64) * (height_px as u64));
            if total_pixels > MAX_ANIMATION_TOTAL_PIXELS {
                tracing::error!(
                    frames_decoded = frames.len(),
                    "WebP animation exceeds MAX_ANIMATION_TOTAL_PIXELS; rejecting"
                );
                return None;
            }

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
        let mut total_pixels: u64 = 0;
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

            // Cumulative budget across every frame decoded so far — see
            // `MAX_ANIMATION_TOTAL_PIXELS`. PNG's decoder enforces
            // `max_alloc` per frame, but nothing bounds the total across an
            // arbitrarily long APNG frame sequence.
            total_pixels = total_pixels.saturating_add((width_px as u64) * (height_px as u64));
            if total_pixels > MAX_ANIMATION_TOTAL_PIXELS {
                tracing::error!(
                    frames_decoded = frames.len(),
                    "APNG animation exceeds MAX_ANIMATION_TOTAL_PIXELS; rejecting"
                );
                return None;
            }

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
mod tests;
