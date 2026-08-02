//! Cursor/selection movement helpers for the URL bar (Parley-aware:
//! grapheme-boundary-correct, not byte-index-blind).

use super::text_layout::*;

// ── Cursor / selection helpers (use Parley) ───────────────────────────────────

/// Returns the logical-pixel X offset corresponding to `byte_offset` in `url`.
///
/// The returned value is relative to the left edge of the URL text (i.e. after
/// the `URL_TEXT_PAD` inside the bar). Callers should add `url_text_left()` to
/// get the window-space coordinate.
pub fn url_cursor_x(
    url: &str,
    byte_offset: usize,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
) -> f32 {
    let prefix = &url[..byte_offset.min(url.len())];
    if prefix.is_empty() {
        return 0.0;
    }
    let layout = build_chrome_text_layout(prefix, font_cx, layout_cx);
    layout.width()
}

/// Returns the byte offset into `url` whose visual X position is closest to
/// `text_rel_x` (relative to the left edge of the URL text area, before padding).
pub fn url_cursor_from_x(
    url: &str,
    text_rel_x: f32,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
) -> usize {
    if url.is_empty() || text_rel_x <= 0.0 {
        return 0;
    }
    // Collect all char-boundary positions and pick the closest one.
    let mut best = 0;
    let mut best_dist = f32::MAX;
    let mut i = 0;
    while i <= url.len() {
        if url.is_char_boundary(i) {
            let x = url_cursor_x(url, i, font_cx, layout_cx);
            let dist = (x - text_rel_x).abs();
            if dist < best_dist {
                best_dist = dist;
                best = i;
            }
        }
        i += 1;
    }
    best
}
