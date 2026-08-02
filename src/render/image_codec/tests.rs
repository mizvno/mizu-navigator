//! Tests for the image_codec module.

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

/// Builds a minimal simple-lossy (non-extended, `"VP8 "` chunk) WebP file
/// that *declares* the given dimensions in its 10-byte VP8 keyframe
/// header, without carrying any real VP8 bitstream payload — so a
/// decoder must read only the header to learn the (here, maximal) size.
/// `width`/`height` are masked to 14 bits by the format itself (max
/// 16383), matching `image-webp`'s own `w & 0x3FFF` / `h & 0x3FFF`.
fn webp_lossy_header_with_dims(width: u16, height: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(30);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&22u32.to_le_bytes()); // riff size (len - 8)
    buf.extend_from_slice(b"WEBP");
    buf.extend_from_slice(b"VP8 ");
    buf.extend_from_slice(&10u32.to_le_bytes()); // chunk size (header only)
    buf.extend_from_slice(&[0x00, 0x00, 0x00]); // tag (bit0 = 0 => keyframe)
    buf.extend_from_slice(&[0x9d, 0x01, 0x2a]); // VP8 start code
    buf.extend_from_slice(&(width & 0x3FFF).to_le_bytes());
    buf.extend_from_slice(&(height & 0x3FFF).to_le_bytes());
    buf
}

#[test]
fn decode_rejects_oversized_webp_dimensions() {
    // WebP's own 14-bit width/height fields cap a single dimension at
    // 16383, so use the max in both: 16383 x 16383 ~= 268M pixels, still
    // far above MAX_IMAGE_PIXELS (64M) and — at 4 bytes/pixel — over 1 GiB.
    //
    // Note on what this test does and doesn't isolate: this fixture has
    // no real VP8 bitstream payload, so even with the dimension check
    // removed from the "Try WebP Decoder" block, `decode_image_bytes`
    // would still end up returning `None` here — via the *separate*,
    // pre-existing generic fallback path (`ImageReader::into_dimensions`
    // + the same `MAX_IMAGE_PIXELS` check, further down in this
    // function), which independently rejects the same declared
    // dimensions. What only the fix below prevents is the animated-WebP
    // branch itself attempting `RgbaImage::new(16383, 16383)` (a real
    // ~1 GiB allocation, done *before* any bitstream is actually read —
    // see `image` 0.25.10's `webp/decoder.rs` `into_frames`) on a
    // *genuine*, decodable, multi-frame WebP with an oversized canvas —
    // a fixture this test does not construct, since hand-encoding a
    // valid VP8/VP8L bitstream is impractical here. That the fix closes
    // this specific case was confirmed by direct inspection of the
    // `image`/`image-webp` crate sources (documented in the code
    // comment above the WebP block). This test still pins down the
    // observable, always-true postcondition: an oversized declared WebP
    // canvas is rejected, not silently accepted.
    let bomb = webp_lossy_header_with_dims(0x3FFF, 0x3FFF);
    assert!(
        decode_image_bytes(&bomb).is_none(),
        "a WebP declaring the maximum representable canvas must be rejected before decoding"
    );
}

#[test]
fn decode_rejects_garbage_bytes() {
    // Not a recognisable image at all — must return None, not panic.
    assert!(decode_image_bytes(b"not an image at all").is_none());
}
