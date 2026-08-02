//! Text building, measuring, and eliding: `face_size`/`build_text[_ex]`/
//! `draw_layout`, the [`TextMetrics`] measurement cache, and the
//! `TextCtx` segment-drawing helper.

use vello::Scene;
use vello::kurbo::Affine;
use vello::peniko::{Color, Fill};

use parley::style::{
    FontFamily, FontFamilyName, FontWeight, GenericFamily, LineHeight, StyleProperty,
};

use rustc_hash::FxHashMap;

use crate::render::inspector::model::Face;

use super::constants::*;

// ─────────────────────────────────────────────────────────────────────────────

/// Font size used for a face.
pub(super) fn face_size(face: Face) -> f32 {
    match face {
        Face::Mono => MONO_SIZE,
        Face::Ui => UI_SIZE,
        Face::UiStrong => HEADER_SIZE,
    }
}

/// Builds a layout for `text` in `face`, wrapped to `wrap` when given and
/// left as a single line otherwise.
///
/// Every row segment in the panel is single-line (`wrap: None`); the value
/// drawer is the one caller that wraps, since it is the one place showing
/// text long enough that word-wrapping beats a horizontal scrollbar.
pub(super) fn build_text_ex(
    text: &str,
    face: Face,
    wrap: Option<f32>,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<Color>,
) -> parley::Layout<Color> {
    let fallbacks = match face {
        Face::Mono => vec![
            FontFamilyName::named("Consolas"),
            FontFamilyName::named("Cascadia Code"),
            FontFamilyName::named("Courier New"),
            FontFamilyName::Generic(GenericFamily::Monospace),
            FontFamilyName::Generic(GenericFamily::SansSerif),
        ],
        Face::Ui | Face::UiStrong => vec![
            FontFamilyName::named("Segoe UI"),
            FontFamilyName::named("Arial"),
            FontFamilyName::Generic(GenericFamily::SansSerif),
        ],
    };
    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::FontFamily(FontFamily::List(
        std::borrow::Cow::Owned(fallbacks),
    )));
    builder.push_default(StyleProperty::FontSize(face_size(face)));
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(1.0)));
    if face == Face::UiStrong {
        builder.push_default(StyleProperty::FontWeight(FontWeight::new(600.0)));
        builder.push_default(StyleProperty::LetterSpacing(HEADER_TRACKING));
    }
    let mut layout = builder.build(text);
    layout.break_all_lines(wrap);
    layout
}

/// Builds a single-line layout for `text` in `face`.
pub(super) fn build_text(
    text: &str,
    face: Face,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<Color>,
) -> parley::Layout<Color> {
    build_text_ex(text, face, None, font_cx, layout_cx)
}

/// Draws every glyph of an already-built layout with its top-left corner at
/// logical `(x, y)`. Shared by single-line rows and the drawer's wrapped
/// paragraph — a `parley::Layout` already carries every wrapped line's
/// correct relative position, so drawing it is the same loop either way.
pub(super) fn draw_layout(
    scene: &mut Scene,
    layout: &parley::Layout<Color>,
    x: f32,
    y: f32,
    color: Color,
    transform: Affine,
) {
    let Some(first) = layout.lines().next() else {
        return;
    };
    let metrics = first.metrics();
    let y_offset = metrics.ascent - metrics.baseline;

    for line in layout.lines() {
        for item in line.items() {
            if let parley::layout::PositionedLayoutItem::GlyphRun(run) = item {
                let font_data = run.run().font();
                let (arc_data, id) = font_data.data.clone().into_raw_parts();
                let blob = vello::peniko::Blob::from_raw_parts(arc_data, id);
                let font = vello::peniko::Font::new(blob, font_data.index);
                let glyphs = run.positioned_glyphs().map(|g| vello::glyph::Glyph {
                    id: g.id,
                    x: g.x,
                    y: g.y,
                });
                scene
                    .draw_glyphs(&font)
                    .font_size(run.run().font_size())
                    .brush(color)
                    .transform(transform * Affine::translate((x as f64, (y + y_offset) as f64)))
                    .draw(Fill::NonZero, glyphs);
            }
        }
    }
}

/// Cached text measurements, reused across frames.
///
/// The panel re-lays every visible row every frame, and fitting a row means
/// measuring each of its segments — so measuring has to be cheap or the panel
/// becomes the most expensive thing on screen.  Two things make it cheap:
/// monospace ASCII resolves to `chars × advance` with no layout at all, and
/// everything else is memoised by `(face, text)`.
#[derive(Debug, Default)]
pub struct TextMetrics {
    /// Advance width of one ASCII cell in the monospace face.
    mono_advance: Option<f32>,
    /// Measured widths of runs the fast path cannot answer.
    pub(super) cache: FxHashMap<(Face, String), f32>,
}

/// Entries retained before the measurement cache is dropped and rebuilt.
///
/// Log rows carry unbounded distinct strings, so the cache must not be an
/// unbounded map keyed by them; clearing wholesale is fine because the
/// working set of a frame is a few dozen rows.
pub(super) const METRICS_CACHE_LIMIT: usize = 4096;

impl TextMetrics {
    /// Width of `text` in `face`, in logical pixels.
    pub(super) fn width(
        &mut self,
        text: &str,
        face: Face,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<Color>,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        if let Some(advance) = self.mono_cell(face, text, font_cx, layout_cx) {
            return advance * text.len() as f32;
        }
        if let Some(&w) = self.cache.get(&(face, text.to_owned())) {
            return w;
        }
        let w = build_text(text, face, font_cx, layout_cx).width();
        if self.cache.len() >= METRICS_CACHE_LIMIT {
            self.cache.clear();
        }
        self.cache.insert((face, text.to_owned()), w);
        w
    }

    /// The monospace cell width, when `text` is a run the fast path covers.
    ///
    /// Non-ASCII is excluded deliberately: a monospace font's "cell" is only
    /// uniform across the ASCII range, and CJK or emoji in a log line would
    /// otherwise be measured as half their true width.
    fn mono_cell(
        &mut self,
        face: Face,
        text: &str,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<Color>,
    ) -> Option<f32> {
        if face != Face::Mono || !text.is_ascii() {
            return None;
        }
        Some(*self.mono_advance.get_or_insert_with(|| {
            // Measured over a run, so any per-run shaping overhead averages
            // out instead of landing entirely on a single glyph.
            const PROBE: &str = "0000000000";
            build_text(PROBE, Face::Mono, font_cx, layout_cx).width() / PROBE.len() as f32
        }))
    }
}

/// Everything needed to measure and draw text, grouped so it can be threaded
/// through the row painters as one borrow.
pub(super) struct TextCtx<'a> {
    pub(super) font_cx: &'a mut parley::FontContext,
    pub(super) layout_cx: &'a mut parley::LayoutContext<Color>,
    pub(super) metrics: &'a mut TextMetrics,
}

impl TextCtx<'_> {
    pub(super) fn width(&mut self, text: &str, face: Face) -> f32 {
        self.metrics.width(text, face, self.font_cx, self.layout_cx)
    }

    /// Draws `text` with its top-left corner at logical `at`.
    pub(super) fn draw(
        &mut self,
        scene: &mut Scene,
        text: &str,
        face: Face,
        at: (f32, f32),
        color: Color,
        transform: Affine,
    ) {
        if text.is_empty() {
            return;
        }
        let layout = build_text(text, face, self.font_cx, self.layout_cx);
        draw_layout(scene, &layout, at.0, at.1, color, transform);
    }

    /// Draws `text` word-wrapped to `max_width`, and returns the wrapped
    /// layout's total height — the only text in the panel that spans more
    /// than one line, used by the value drawer.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_wrapped(
        &mut self,
        scene: &mut Scene,
        text: &str,
        face: Face,
        at: (f32, f32),
        max_width: f32,
        color: Color,
        transform: Affine,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let layout = build_text_ex(text, face, Some(max_width), self.font_cx, self.layout_cx);
        draw_layout(scene, &layout, at.0, at.1, color, transform);
        layout.height()
    }

    /// Height a wrapped layout of `text` would occupy at `max_width`, without
    /// drawing it — used to size the drawer's scroll range before painting.
    pub(super) fn wrapped_height(&mut self, text: &str, face: Face, max_width: f32) -> f32 {
        build_text_ex(text, face, Some(max_width), self.font_cx, self.layout_cx).height()
    }

    /// Height of one line in `face`.
    pub(super) fn line_height(&mut self, face: Face) -> f32 {
        build_text("Xg", face, self.font_cx, self.layout_cx).height()
    }

    /// Distance from the top of a line in `face` to its baseline.
    ///
    /// A row mixes faces — a UI label beside a monospace value — and the two
    /// have different ascents, so aligning their *tops* leaves the text
    /// visibly stepped. Aligning baselines is the only thing that reads as
    /// one line.
    pub(super) fn ascent(&mut self, face: Face) -> f32 {
        build_text("Xg", face, self.font_cx, self.layout_cx)
            .lines()
            .next()
            .map(|l| l.metrics().ascent)
            .unwrap_or(0.0)
    }

    /// Truncates `text` from the tail so it fits `max_w`, appending `…`.
    pub(super) fn elide_tail(&mut self, text: &str, face: Face, max_w: f32) -> String {
        if self.width(text, face) <= max_w {
            return text.to_owned();
        }
        let ell_w = self.width("…", face);
        let budget = max_w - ell_w;
        if budget <= 0.0 {
            return String::new();
        }
        let kept = self.fit_prefix(text, face, budget);
        if kept == 0 {
            return String::new();
        }
        let cut = byte_at(text, kept);
        format!("{}…", &text[..cut])
    }

    /// Truncates `text` in the middle so it fits `max_w`, keeping both ends.
    ///
    /// URLs and file paths carry as much meaning in the tail (the resource)
    /// as in the head (the host), so cutting only the tail is the wrong
    /// trade for them.
    pub(super) fn elide_middle(&mut self, text: &str, face: Face, max_w: f32) -> String {
        if self.width(text, face) <= max_w {
            return text.to_owned();
        }
        let ell_w = self.width("…", face);
        let budget = max_w - ell_w;
        if budget <= 0.0 {
            return String::new();
        }
        // The head keeps slightly more than the tail: the scheme and host
        // are usually the coarser filter when scanning a list.
        let head_budget = budget * 0.55;
        let head = self.fit_prefix(text, face, head_budget);
        let tail_budget = budget - self.prefix_width(text, face, head);
        let tail = self.fit_suffix(text, face, tail_budget);
        let total = text.chars().count();
        if head + tail >= total {
            return text.to_owned();
        }
        if head == 0 && tail == 0 {
            return String::new();
        }
        let head_end = byte_at(text, head);
        let tail_start = byte_at(text, total - tail);
        format!("{}…{}", &text[..head_end], &text[tail_start..])
    }

    /// Number of leading characters of `text` that fit in `max_w`.
    fn fit_prefix(&mut self, text: &str, face: Face, max_w: f32) -> usize {
        let total = text.chars().count();
        if max_w <= 0.0 || total == 0 {
            return 0;
        }
        // Monospace ASCII needs no search: the width is linear in the count.
        if let Some(cell) = self.mono_cell(face, text) {
            return ((max_w / cell).floor() as usize).min(total);
        }
        let mut lo = 0usize;
        let mut hi = total;
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.prefix_width(text, face, mid) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    /// Number of trailing characters of `text` that fit in `max_w`.
    fn fit_suffix(&mut self, text: &str, face: Face, max_w: f32) -> usize {
        let total = text.chars().count();
        if max_w <= 0.0 || total == 0 {
            return 0;
        }
        if let Some(cell) = self.mono_cell(face, text) {
            return ((max_w / cell).floor() as usize).min(total);
        }
        let mut lo = 0usize;
        let mut hi = total;
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let start = byte_at(text, total - mid);
            if self.width(&text[start..], face) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    fn prefix_width(&mut self, text: &str, face: Face, chars: usize) -> f32 {
        let end = byte_at(text, chars);
        self.width(&text[..end], face)
    }

    fn mono_cell(&mut self, face: Face, text: &str) -> Option<f32> {
        self.metrics
            .mono_cell(face, text, self.font_cx, self.layout_cx)
    }
}

/// Byte offset of the `n`-th character boundary of `text`, saturating at its
/// length.
pub(super) fn byte_at(text: &str, n: usize) -> usize {
    text.char_indices()
        .nth(n)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}
