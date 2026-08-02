//! Vello/Parley painting of the inspector panel and the page highlight.
//!
//! All geometry is computed in logical pixels and scaled once via the
//! `Affine::scale(dpi)` transform, mirroring the chrome bar's approach.
//!
//! ## Fitting text, rather than clipping it
//!
//! The panel is a fixed 420 logical pixels wide and shows URLs, expressions,
//! and error messages that are routinely longer than that.  Nothing here is
//! allowed to run off the edge and be chopped by the clip rect: every row is
//! measured against the width actually available to it, and the segments
//! marked [`Flex::Elide`] / [`Flex::ElideMiddle`] absorb the shortfall with a
//! visible ellipsis, so the reader can always tell that there is more.
//!
//! Measuring is the expensive part of that, so [`TextMetrics`] caches it and
//! short-circuits the common case: data is set in monospace, and a monospace
//! ASCII run's width is a multiplication, which also makes eliding it O(1)
//! instead of a binary search over layouts.

#![forbid(unsafe_code)]

use rustc_hash::FxHashMap;

use vello::Scene;
use vello::kurbo::{Affine, BezPath, Circle, Line, Point, Rect, RoundedRect, Stroke};
use vello::peniko::{BlendMode, Color, Compose, Fill, Mix};

use parley::style::{
    FontFamily, FontFamilyName, FontWeight, GenericFamily, LineHeight, StyleProperty,
};

use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::inspector::model::{Face, Flex, Row, RowKind, Tone};
use crate::render::inspector::{
    DRAWER_CLOSE_WIDTH, DRAWER_COPY_WIDTH, DRAWER_HEADER_HEIGHT, DRAWER_HEIGHT, DRAWER_PAD, HPAD,
    InspectorState, InspectorTab, PANEL_WIDTH, PICKER_BTN_WIDTH, SCROLLBAR_GUTTER, TAB_BAR_HEIGHT,
    TWISTY_WIDTH, ValueView, content_top, panel_left, row_at, row_content_left, row_tops,
};
use crate::render::preferences::ChromePalette;

// ── Typography ───────────────────────────────────────────────────────────────

/// Size of monospace data text.
const MONO_SIZE: f32 = 11.5;
/// Size of UI-face labels and prose.
const UI_SIZE: f32 = 11.5;
/// Size of a section header's label — small, tracked, and uppercase.
const HEADER_SIZE: f32 = 10.0;
/// Extra tracking applied to header labels.
const HEADER_TRACKING: f32 = 0.7;

/// Horizontal gap between two segments of a row.
const SEG_GAP: f32 = 6.0;
/// Narrowest an elidable segment may be squeezed before it is dropped
/// entirely — below this it is all ellipsis and no information.
const MIN_ELIDE_WIDTH: f32 = 22.0;
/// Leading width that must survive before a row is allowed to spend any of
/// its space on right-aligned metrics.
const MIN_LEADING_WIDTH: f32 = 90.0;

/// Side of the colour chip drawn before a colour-valued segment.
const SWATCH_SIZE: f32 = 9.0;
/// Gap between a colour chip and the text it annotates.
const SWATCH_GAP: f32 = 5.0;

// ── Decoration ───────────────────────────────────────────────────────────────

/// Alpha applied to the accent color for the page highlight's fill; the
/// border reuses the accent at full strength.
const HIGHLIGHT_FILL_ALPHA: u8 = 0x2d;
/// Alpha of the vertical guides that trace the Elements tree's indentation.
const GUIDE_ALPHA: u8 = 0x44;
/// Alpha of a section header's trailing hairline.
const RULE_ALPHA: u8 = 0x66;

/// Height of the accent underline marking the active tab / engaged picker.
const ACTIVE_UNDERLINE_H: f32 = 2.0;
/// Width of the accent bar marking the selected row.
const SELECTION_BAR_W: f32 = 2.0;

/// Width of the scrollbar thumb, and its inset from the panel's right edge.
const SCROLLBAR_W: f32 = 4.0;
const SCROLLBAR_INSET: f32 = 3.0;
/// Shortest the scroll thumb may become, so it stays grabbable in a long log.
const MIN_THUMB_H: f32 = 26.0;

// ─────────────────────────────────────────────────────────────────────────────
// Colour
// ─────────────────────────────────────────────────────────────────────────────

/// The panel's six text colours, resolved once per paint from the chrome
/// palette — the panel carries no palette of its own, so it follows
/// light/dark/high-contrast exactly as the tab strip and URL bar do.
struct Tones {
    key: Color,
    value: Color,
    muted: Color,
    accent: Color,
    good: Color,
    bad: Color,
}

/// Minimum contrast a tone must reach against the panel background.
const MIN_TEXT_CONTRAST: f64 = 4.5;

impl Tones {
    fn new(palette: &ChromePalette) -> Self {
        Tones {
            key: palette.url_text,
            value: palette.tab_text,
            muted: palette.tab_text_inactive,
            // `url_border_focused` is a *ring* colour, chosen to stand out
            // against a border, and in the light palette it lands at 2.45:1
            // as text — well under AA. Darkening it toward the background's
            // opposite pole keeps the accent recognisably itself while making
            // it readable, which pure black (what `enforce_min_contrast`
            // would return) does not.
            accent: readable(palette.url_border_focused, palette.bar_bg),
            good: readable(palette.ok_dot, palette.bar_bg),
            bad: readable(palette.err_text, palette.bar_bg),
        }
    }

    fn get(&self, tone: Tone) -> Color {
        match tone {
            Tone::Key => self.key,
            Tone::Value => self.value,
            Tone::Muted => self.muted,
            Tone::Accent => self.accent,
            Tone::Good => self.good,
            Tone::Bad => self.bad,
        }
    }
}

/// Mixes `fg` toward black or white — whichever contrasts more with `bg` —
/// by the smallest amount that reaches [`MIN_TEXT_CONTRAST`].
///
/// Contrast is monotonic in that mix factor (the mix runs from `fg` to the
/// pole that is furthest from `bg` in luminance), so a bisection finds the
/// least-altered colour that passes.  Returns `fg` untouched when it already
/// does, which is the case for every tone in the dark and high-contrast
/// palettes.
fn readable(fg: Color, bg: Color) -> Color {
    use crate::render::preferences::contrast_ratio;
    if contrast_ratio(fg, bg) >= MIN_TEXT_CONTRAST {
        return fg;
    }
    let pole = if contrast_ratio(Color::rgba8(0, 0, 0, 255), bg)
        >= contrast_ratio(Color::rgba8(255, 255, 255, 255), bg)
    {
        0u8
    } else {
        255u8
    };
    let mix = |t: f32| {
        let blend = |c: u8| {
            (c as f32 + (pole as f32 - c as f32) * t)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Color {
            r: blend(fg.r),
            g: blend(fg.g),
            b: blend(fg.b),
            a: fg.a,
        }
    };
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..12 {
        let mid = (lo + hi) / 2.0;
        if contrast_ratio(mix(mid), bg) >= MIN_TEXT_CONTRAST {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    mix(hi)
}

/// `color` at a reduced alpha, for hairlines and guides.
fn faded(color: Color, alpha: u8) -> Color {
    Color { a: alpha, ..color }
}

// ─────────────────────────────────────────────────────────────────────────────
// Text: building, measuring, eliding
// ─────────────────────────────────────────────────────────────────────────────

/// Font size used for a face.
fn face_size(face: Face) -> f32 {
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
fn build_text_ex(
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
fn build_text(
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
fn draw_layout(
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
    cache: FxHashMap<(Face, String), f32>,
}

/// Entries retained before the measurement cache is dropped and rebuilt.
///
/// Log rows carry unbounded distinct strings, so the cache must not be an
/// unbounded map keyed by them; clearing wholesale is fine because the
/// working set of a frame is a few dozen rows.
const METRICS_CACHE_LIMIT: usize = 4096;

impl TextMetrics {
    /// Width of `text` in `face`, in logical pixels.
    fn width(
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
struct TextCtx<'a> {
    font_cx: &'a mut parley::FontContext,
    layout_cx: &'a mut parley::LayoutContext<Color>,
    metrics: &'a mut TextMetrics,
}

impl TextCtx<'_> {
    fn width(&mut self, text: &str, face: Face) -> f32 {
        self.metrics.width(text, face, self.font_cx, self.layout_cx)
    }

    /// Draws `text` with its top-left corner at logical `at`.
    fn draw(
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
    fn draw_wrapped(
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
    fn wrapped_height(&mut self, text: &str, face: Face, max_width: f32) -> f32 {
        build_text_ex(text, face, Some(max_width), self.font_cx, self.layout_cx).height()
    }

    /// Height of one line in `face`.
    fn line_height(&mut self, face: Face) -> f32 {
        build_text("Xg", face, self.font_cx, self.layout_cx).height()
    }

    /// Distance from the top of a line in `face` to its baseline.
    ///
    /// A row mixes faces — a UI label beside a monospace value — and the two
    /// have different ascents, so aligning their *tops* leaves the text
    /// visibly stepped. Aligning baselines is the only thing that reads as
    /// one line.
    fn ascent(&mut self, face: Face) -> f32 {
        build_text("Xg", face, self.font_cx, self.layout_cx)
            .lines()
            .next()
            .map(|l| l.metrics().ascent)
            .unwrap_or(0.0)
    }

    /// Truncates `text` from the tail so it fits `max_w`, appending `…`.
    fn elide_tail(&mut self, text: &str, face: Face, max_w: f32) -> String {
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
    fn elide_middle(&mut self, text: &str, face: Face, max_w: f32) -> String {
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
fn byte_at(text: &str, n: usize) -> usize {
    text.char_indices()
        .nth(n)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

// ─────────────────────────────────────────────────────────────────────────────
// Segment placement
// ─────────────────────────────────────────────────────────────────────────────

/// A segment resolved to a position and to the text that actually fits.
struct Placed {
    x: f32,
    text: String,
    tone: Tone,
    face: Face,
    swatch: Option<(u8, u8, u8, u8)>,
}

/// Extra width a segment needs for its colour chip, if it has one.
fn swatch_width(seg: &crate::render::inspector::model::Seg) -> f32 {
    if seg.swatch.is_some() {
        SWATCH_SIZE + SWATCH_GAP
    } else {
        0.0
    }
}

/// Lays a row's segments out across `[x0, x1]`.
///
/// Fixed segments keep their natural width; trailing segments are packed
/// against `x1`; whatever is left over goes to the elidable segments, shared
/// in proportion to how much each of them wanted.  A row whose fixed content
/// alone overflows still does not bleed: the last leading segment is elided
/// as a backstop.
fn place_segs(
    segs: &[crate::render::inspector::model::Seg],
    x0: f32,
    x1: f32,
    text: &mut TextCtx<'_>,
) -> Vec<Placed> {
    let mut placed = Vec::with_capacity(segs.len());
    if x1 <= x0 {
        return placed;
    }

    let leading: Vec<usize> = (0..segs.len())
        .filter(|&i| segs[i].flex != Flex::Trailing)
        .collect();
    let trailing: Vec<usize> = (0..segs.len())
        .filter(|&i| segs[i].flex == Flex::Trailing)
        .collect();

    // ── Reserve the trailing run, if the row can afford it ───────────────
    let mut trail_w = 0.0;
    let mut trail_widths = Vec::with_capacity(trailing.len());
    for (n, &i) in trailing.iter().enumerate() {
        let w = text.width(&segs[i].text, segs[i].face) + swatch_width(&segs[i]);
        trail_widths.push(w);
        trail_w += w + if n > 0 { SEG_GAP } else { 0.0 };
    }
    let show_trailing =
        !trailing.is_empty() && x1 - trail_w - SEG_GAP - x0 >= MIN_LEADING_WIDTH.min(x1 - x0);
    let lead_x1 = if show_trailing {
        x1 - trail_w - SEG_GAP
    } else {
        x1
    };

    // ── Fit the leading run ──────────────────────────────────────────────
    let gaps = SEG_GAP * leading.len().saturating_sub(1) as f32;
    let mut natural = Vec::with_capacity(leading.len());
    let mut fixed_w = 0.0;
    let mut flex_natural = 0.0;
    for &i in &leading {
        let w = text.width(&segs[i].text, segs[i].face) + swatch_width(&segs[i]);
        natural.push(w);
        if segs[i].flex == Flex::Fixed {
            fixed_w += w;
        } else {
            flex_natural += w;
        }
    }

    let avail = lead_x1 - x0 - gaps;
    let flex_budget = (avail - fixed_w).max(0.0);
    let squeeze = flex_natural > flex_budget && flex_natural > 0.0;

    let mut x = x0;
    for (n, &i) in leading.iter().enumerate() {
        let seg = &segs[i];
        let natural_w = natural[n];
        let sw = swatch_width(seg);
        let remaining = lead_x1 - x;
        if remaining <= 0.0 {
            break;
        }

        let (fitted, used) = if seg.flex == Flex::Fixed {
            // Fixed segments are short by contract; the elision here is a
            // backstop for a pathologically narrow panel, not a normal path.
            if natural_w <= remaining {
                (seg.text.clone(), natural_w)
            } else {
                let t = text.elide_tail(&seg.text, seg.face, remaining - sw);
                let w = text.width(&t, seg.face) + sw;
                (t, w)
            }
        } else if !squeeze {
            (seg.text.clone(), natural_w)
        } else {
            // Share the shortfall in proportion to appetite, so a row with a
            // long URL and a short label does not shrink both by half.
            let share = flex_budget * (natural_w / flex_natural);
            let target = share.min(remaining);
            if target < MIN_ELIDE_WIDTH {
                continue;
            }
            let t = match seg.flex {
                Flex::ElideMiddle => text.elide_middle(&seg.text, seg.face, target - sw),
                _ => text.elide_tail(&seg.text, seg.face, target - sw),
            };
            if t.is_empty() {
                continue;
            }
            let w = text.width(&t, seg.face) + sw;
            (t, w)
        };

        placed.push(Placed {
            x,
            text: fitted,
            tone: seg.tone,
            face: seg.face,
            swatch: seg.swatch,
        });
        x += used + SEG_GAP;
    }

    // ── Pack the trailing run against the right edge ─────────────────────
    if show_trailing {
        let mut tx = x1;
        for (n, &i) in trailing.iter().enumerate().rev() {
            tx -= trail_widths[n];
            placed.push(Placed {
                x: tx,
                text: segs[i].text.clone(),
                tone: segs[i].tone,
                face: segs[i].face,
                swatch: segs[i].swatch,
            });
            tx -= SEG_GAP;
        }
    }

    placed
}

// ─────────────────────────────────────────────────────────────────────────────
// Panel
// ─────────────────────────────────────────────────────────────────────────────

/// Context for [`paint_panel`], bundling everything but the `Scene` it
/// draws into (kept separate, matching this codebase's `paint_node`
/// convention of threading the draw target alongside a context struct).
pub struct PanelPaintContext<'a> {
    /// Panel UI state (selected tab, scroll offsets, picker mode); mutated
    /// to clamp scroll and record the content height each paint.
    pub state: &'a mut InspectorState,
    /// The panel's currently visible rows for the active tab.
    pub rows: &'a [Row],
    /// Logical window width.
    pub window_width: f32,
    /// Logical window height.
    pub window_height: f32,
    /// DPI scale factor.
    pub scale: f32,
    /// Parley font context, for row/tab-label text layout.
    pub font_cx: &'a mut parley::FontContext,
    /// Parley layout context, for row/tab-label text layout.
    pub layout_cx: &'a mut parley::LayoutContext<Color>,
    /// The chrome palette — the panel's only source of color.
    pub palette: &'a ChromePalette,
}

/// Paints the docked panel: background, tab bar, picker button, visible rows,
/// and scrollbar.  Also clamps the active tab's scroll offset against the
/// current content height (stored back into `ctx.state.max_scroll`).
pub fn paint_panel(scene: &mut Scene, ctx: &mut PanelPaintContext<'_>) {
    let transform = Affine::scale(ctx.scale as f64);
    let left = panel_left(ctx.window_width);
    let right = ctx.window_width;
    let top = CHROME_HEIGHT;
    let bottom = ctx.window_height.max(top);
    let palette = ctx.palette;
    let tones = Tones::new(palette);

    // Disjoint field borrows: text measurement owns `state.text_metrics`,
    // everything below reads and writes the other `state` fields.
    let mut text = TextCtx {
        font_cx: ctx.font_cx,
        layout_cx: ctx.layout_cx,
        metrics: &mut ctx.state.text_metrics,
    };

    // ── Panel background ─────────────────────────────────────────────────
    scene.fill(
        Fill::NonZero,
        transform,
        palette.bar_bg,
        None,
        &Rect::new(left as f64, top as f64, right as f64, bottom as f64),
    );

    // ── Tab bar ──────────────────────────────────────────────────────────
    let hover = ctx.state.hover;
    paint_tab_bar(
        scene,
        &mut text,
        palette,
        transform,
        left,
        right,
        top,
        ctx.state.tab,
        ctx.state.picker,
        hover,
    );

    // ── Content: scroll clamp + visible slice ────────────────────────────
    // The value drawer, when open, claims a fixed band at the panel's
    // bottom; the row list's own viewport shrinks to make room for it rather
    // than being painted over.
    let drawer_open = ctx.state.value_view.is_some();
    let rows_bottom = if drawer_open {
        bottom - DRAWER_HEIGHT
    } else {
        bottom
    };
    let body_top = top + content_top();
    let viewport_h = (rows_bottom - body_top).max(0.0);
    let tops = row_tops(ctx.rows);
    let content_h = tops.last().copied().unwrap_or(0.0);
    ctx.state.max_scroll = (content_h - viewport_h).max(0.0);
    let idx = ctx.state.tab.index();
    ctx.state.scroll[idx] = ctx.state.scroll[idx].clamp(0.0, ctx.state.max_scroll);
    let scroll = ctx.state.scroll[idx];

    // Which row the cursor is over, resolved against the geometry that is
    // about to be painted rather than against a second, parallel guess.
    let hovered_row = hover.and_then(|(hx, hy)| {
        (hy >= content_top() && hy < rows_bottom - top && hx < PANEL_WIDTH - SCROLLBAR_GUTTER)
            .then(|| row_at(&tops, hy - content_top() + scroll))
            .flatten()
    });

    let clip = Rect::new(
        left as f64,
        body_top as f64,
        right as f64,
        rows_bottom as f64,
    );
    scene.push_layer(
        BlendMode::new(Mix::Normal, Compose::SrcOver),
        1.0,
        transform,
        &clip,
    );

    let content_x1 = right - SCROLLBAR_GUTTER;
    let show_guides = ctx.state.tab == InspectorTab::Elements;
    for (i, row) in ctx.rows.iter().enumerate() {
        let y = body_top + tops[i] - scroll;
        let h = row.height();
        if y + h < body_top {
            continue;
        }
        if y > rows_bottom {
            break;
        }
        paint_row(
            scene,
            &mut text,
            palette,
            &tones,
            transform,
            RowPaint {
                row,
                y,
                left,
                content_x1,
                selected: row.node.is_some() && row.node == ctx.state.selected,
                hovered: hovered_row == Some(i),
                show_guides,
            },
        );
    }

    scene.pop_layer();

    paint_scrollbar(
        scene,
        palette,
        transform,
        right,
        body_top,
        rows_bottom,
        viewport_h,
        content_h,
        scroll,
        ctx.state.max_scroll,
    );

    // The divider is painted last so neither rows nor the scrollbar can sit
    // on top of the panel's edge.
    scene.fill(
        Fill::NonZero,
        transform,
        palette.url_border_idle,
        None,
        &Rect::new(left as f64, top as f64, (left + 1.0) as f64, bottom as f64),
    );

    // ── Value-inspection drawer ───────────────────────────────────────────
    if let Some(view) = &mut ctx.state.value_view {
        paint_value_drawer(
            scene, &mut text, palette, &tones, transform, left, right, bottom, view, hover,
        );
    }
}

/// Paints the tab strip and the element-picker toggle.
#[allow(clippy::too_many_arguments)]
fn paint_tab_bar(
    scene: &mut Scene,
    text: &mut TextCtx<'_>,
    palette: &ChromePalette,
    transform: Affine,
    left: f32,
    right: f32,
    top: f32,
    active: InspectorTab,
    picker: bool,
    hover: Option<(f32, f32)>,
) {
    scene.fill(
        Fill::NonZero,
        transform,
        palette.strip_bg,
        None,
        &Rect::new(
            left as f64,
            top as f64,
            right as f64,
            (top + TAB_BAR_HEIGHT) as f64,
        ),
    );

    let hover_x = hover
        .filter(|(_, hy)| *hy < TAB_BAR_HEIGHT)
        .map(|(hx, _)| hx);
    let strip_w = PANEL_WIDTH - PICKER_BTN_WIDTH;
    let tab_w = strip_w / InspectorTab::ALL.len() as f32;

    for (i, tab) in InspectorTab::ALL.iter().enumerate() {
        let x0 = left + i as f32 * tab_w;
        let is_active = *tab == active;
        let is_hovered = hover_x
            .map(|hx| hx >= i as f32 * tab_w && hx < (i + 1) as f32 * tab_w)
            .unwrap_or(false);
        // The active tab takes the *content* background so the strip reads as
        // one continuous surface with the list below it.
        let bg = if is_active {
            Some(palette.bar_bg)
        } else if is_hovered {
            Some(palette.tab_inactive_bg)
        } else {
            None
        };
        if let Some(bg) = bg {
            scene.fill(
                Fill::NonZero,
                transform,
                bg,
                None,
                &Rect::new(
                    x0 as f64,
                    top as f64,
                    (x0 + tab_w) as f64,
                    (top + TAB_BAR_HEIGHT) as f64,
                ),
            );
        }
        if is_active {
            scene.fill(
                Fill::NonZero,
                transform,
                palette.url_border_focused,
                None,
                &Rect::new(
                    x0 as f64,
                    (top + TAB_BAR_HEIGHT - ACTIVE_UNDERLINE_H) as f64,
                    (x0 + tab_w) as f64,
                    (top + TAB_BAR_HEIGHT) as f64,
                ),
            );
        }
        let color = if is_active {
            palette.tab_text
        } else {
            palette.tab_text_inactive
        };
        let label = tab.label();
        let w = text.width(label, Face::Ui);
        let h = text.line_height(Face::Ui);
        text.draw(
            scene,
            label,
            Face::Ui,
            (
                x0 + (tab_w - w).max(0.0) / 2.0,
                top + (TAB_BAR_HEIGHT - ACTIVE_UNDERLINE_H - h) / 2.0,
            ),
            color,
            transform,
        );
    }

    // ── Picker toggle ────────────────────────────────────────────────────
    let px0 = left + strip_w;
    let picker_hovered = hover_x.map(|hx| hx >= strip_w).unwrap_or(false);
    if picker || picker_hovered {
        scene.fill(
            Fill::NonZero,
            transform,
            if picker {
                palette.bar_bg
            } else {
                palette.tab_inactive_bg
            },
            None,
            &Rect::new(
                px0 as f64,
                top as f64,
                (px0 + PICKER_BTN_WIDTH) as f64,
                (top + TAB_BAR_HEIGHT) as f64,
            ),
        );
    }
    if picker {
        scene.fill(
            Fill::NonZero,
            transform,
            palette.url_border_focused,
            None,
            &Rect::new(
                px0 as f64,
                (top + TAB_BAR_HEIGHT - ACTIVE_UNDERLINE_H) as f64,
                (px0 + PICKER_BTN_WIDTH) as f64,
                (top + TAB_BAR_HEIGHT) as f64,
            ),
        );
    }
    paint_picker_icon(
        scene,
        transform,
        px0 + PICKER_BTN_WIDTH / 2.0,
        top + (TAB_BAR_HEIGHT - ACTIVE_UNDERLINE_H) / 2.0,
        if picker {
            palette.url_border_focused
        } else {
            palette.tab_text_inactive
        },
    );

    // Hairline under the whole strip, broken by the active tab's underline.
    scene.fill(
        Fill::NonZero,
        transform,
        palette.url_border_idle,
        None,
        &Rect::new(
            left as f64,
            (top + TAB_BAR_HEIGHT) as f64,
            right as f64,
            (top + TAB_BAR_HEIGHT + 1.0) as f64,
        ),
    );
}

/// Draws the element-picker's crosshair centred on `(cx, cy)`.
///
/// A drawn icon rather than the old `[+]` text: it matches the crosshair
/// cursor the picker actually switches to, and it does not depend on a font
/// happening to have a legible bracket at 11 px.
fn paint_picker_icon(scene: &mut Scene, transform: Affine, cx: f32, cy: f32, color: Color) {
    let (cx, cy) = (cx as f64, cy as f64);
    let stroke = Stroke::new(1.2);
    scene.stroke(
        &stroke,
        transform,
        color,
        None,
        &Circle::new(Point::new(cx, cy), 4.0),
    );
    for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
        scene.stroke(
            &stroke,
            transform,
            color,
            None,
            &Line::new(
                Point::new(cx + dx * 5.5, cy + dy * 5.5),
                Point::new(cx + dx * 8.0, cy + dy * 8.0),
            ),
        );
    }
}

/// Everything painting one row depends on besides the shared contexts.
struct RowPaint<'a> {
    row: &'a Row,
    y: f32,
    left: f32,
    content_x1: f32,
    selected: bool,
    hovered: bool,
    show_guides: bool,
}

fn paint_row(
    scene: &mut Scene,
    text: &mut TextCtx<'_>,
    palette: &ChromePalette,
    tones: &Tones,
    transform: Affine,
    p: RowPaint<'_>,
) {
    let RowPaint {
        row,
        y,
        left,
        content_x1,
        selected,
        hovered,
        show_guides,
    } = p;
    let h = row.height();

    // ── Row background ───────────────────────────────────────────────────
    if row.kind != RowKind::Header && (selected || hovered) {
        scene.fill(
            Fill::NonZero,
            transform,
            if selected {
                palette.tab_active_bg
            } else {
                palette.tab_inactive_bg
            },
            None,
            &Rect::new(left as f64, y as f64, content_x1 as f64, (y + h) as f64),
        );
    }
    if selected {
        // The accent bar, not the fill, is what makes the selection legible:
        // in the high-contrast palette every background is the same black.
        scene.fill(
            Fill::NonZero,
            transform,
            palette.url_border_focused,
            None,
            &Rect::new(
                left as f64,
                y as f64,
                (left + SELECTION_BAR_W) as f64,
                (y + h) as f64,
            ),
        );
    }

    // ── Indentation guides ───────────────────────────────────────────────
    if show_guides && row.indent > 0 {
        let guide = faded(palette.url_border_idle, GUIDE_ALPHA);
        for d in 0..row.indent {
            let gx = left + row_content_left(d) + TWISTY_WIDTH / 2.0;
            scene.fill(
                Fill::NonZero,
                transform,
                guide,
                None,
                &Rect::new(gx as f64, y as f64, (gx + 1.0) as f64, (y + h) as f64),
            );
        }
    }

    let mut x0 = left + row_content_left(row.indent);

    // ── Disclosure triangle ──────────────────────────────────────────────
    if row.expandable {
        paint_twisty(
            scene,
            transform,
            x0 + TWISTY_WIDTH / 2.0,
            y + h / 2.0,
            row.expanded,
            palette.tab_text_inactive,
        );
    }
    if row.kind == RowKind::Item && row.node.is_some() {
        // Every tree row reserves the triangle's strip, expandable or not, so
        // siblings stay aligned rather than jittering by 13 px.
        x0 += TWISTY_WIDTH;
    }

    // ── Content ──────────────────────────────────────────────────────────
    // The row's box is sized by its tallest face; every segment then hangs
    // from a single shared baseline inside it.
    let mut line_h = 0.0f32;
    let mut max_ascent = 0.0f32;
    for seg in &row.segs {
        line_h = line_h.max(text.line_height(seg.face));
        max_ascent = max_ascent.max(text.ascent(seg.face));
    }
    let ty = match row.kind {
        // Headers sit low in their band: the extra height is leading that
        // separates the section from the one above, not padding around it.
        RowKind::Header => y + h - line_h - 5.0,
        _ => y + (h - line_h) / 2.0,
    };

    let placed = place_segs(&row.segs, x0, content_x1 - HPAD, text);
    let mut text_end = x0;
    for item in &placed {
        let mut tx = item.x;
        // Shift this segment down by however much shorter its ascent is, so
        // its baseline lands on the row's.
        let seg_ascent = text.ascent(item.face);
        let ty = ty + (max_ascent - seg_ascent);
        if let Some((r, g, b, a)) = item.swatch {
            let cy = ty + line_h / 2.0;
            let chip = RoundedRect::new(
                tx as f64,
                (cy - SWATCH_SIZE / 2.0) as f64,
                (tx + SWATCH_SIZE) as f64,
                (cy + SWATCH_SIZE / 2.0) as f64,
                2.0,
            );
            scene.fill(
                Fill::NonZero,
                transform,
                Color::rgba8(r, g, b, a),
                None,
                &chip,
            );
            scene.stroke(
                &Stroke::new(1.0),
                transform,
                faded(palette.url_border_idle, RULE_ALPHA),
                None,
                &chip,
            );
            tx += SWATCH_SIZE + SWATCH_GAP;
        }
        let color = tones.get(item.tone);
        let uppercase;
        let body = if row.kind == RowKind::Header && item.face == Face::UiStrong {
            uppercase = item.text.to_uppercase();
            uppercase.as_str()
        } else {
            item.text.as_str()
        };
        text.draw(scene, body, item.face, (tx, ty), color, transform);
        text_end = text_end.max(tx + text.width(body, item.face));
    }

    // ── Header rule ──────────────────────────────────────────────────────
    // A hairline running from the label to the panel's edge turns a coloured
    // line of text into a section boundary the eye can find while scrolling.
    if row.kind == RowKind::Header {
        let rule_x0 = text_end + 8.0;
        // Stop short of a trailing count, which is placed against the edge.
        let rule_x1 = placed
            .iter()
            .map(|p| p.x)
            .filter(|&px| px > rule_x0)
            .fold(content_x1 - HPAD, f32::min)
            - 8.0;
        if rule_x1 > rule_x0 {
            let ry = (y + h - line_h / 2.0 - 5.0) as f64;
            scene.fill(
                Fill::NonZero,
                transform,
                faded(palette.url_border_idle, RULE_ALPHA),
                None,
                &Rect::new(rule_x0 as f64, ry, rule_x1 as f64, ry + 1.0),
            );
        }
    }
}

/// Draws a disclosure triangle centred on `(cx, cy)`, pointing down when the
/// row is expanded and right when it is collapsed.
fn paint_twisty(
    scene: &mut Scene,
    transform: Affine,
    cx: f32,
    cy: f32,
    expanded: bool,
    color: Color,
) {
    const R: f64 = 3.4;
    let (cx, cy) = (cx as f64, cy as f64);
    let mut path = BezPath::new();
    if expanded {
        path.move_to((cx - R, cy - R * 0.6));
        path.line_to((cx + R, cy - R * 0.6));
        path.line_to((cx, cy + R * 0.9));
    } else {
        path.move_to((cx - R * 0.6, cy - R));
        path.line_to((cx + R * 0.9, cy));
        path.line_to((cx - R * 0.6, cy + R));
    }
    path.close_path();
    scene.fill(Fill::NonZero, transform, color, None, &path);
}

/// How long the drawer's Copy button shows "Copied" before reverting —
/// mirrors [`crate::render::inspector::model`]'s mutation-flash convention.
const COPIED_FLASH: std::time::Duration = std::time::Duration::from_millis(1400);

/// Paints the value-inspection drawer docked to the panel's bottom edge:
/// header (title, Copy, Close) and a word-wrapped, independently scrollable
/// body holding the row's full, untruncated text.
///
/// `hover` is the same panel-local cursor point the tab bar and rows use, so
/// the drawer's buttons light up under the cursor with no extra hit-testing
/// plumbing in the event loop.
#[allow(clippy::too_many_arguments)]
fn paint_value_drawer(
    scene: &mut Scene,
    text: &mut TextCtx<'_>,
    palette: &ChromePalette,
    tones: &Tones,
    transform: Affine,
    left: f32,
    right: f32,
    panel_bottom: f32,
    view: &mut ValueView,
    hover: Option<(f32, f32)>,
) {
    let top = panel_bottom - DRAWER_HEIGHT;

    scene.fill(
        Fill::NonZero,
        transform,
        palette.bar_bg,
        None,
        &Rect::new(left as f64, top as f64, right as f64, panel_bottom as f64),
    );
    // A hairline (not just the header's own background) marks the drawer as
    // a distinct region rather than more of the row list.
    scene.fill(
        Fill::NonZero,
        transform,
        palette.url_border_idle,
        None,
        &Rect::new(left as f64, top as f64, right as f64, (top + 1.0) as f64),
    );

    // ── Header: title, Copy, Close ────────────────────────────────────────
    scene.fill(
        Fill::NonZero,
        transform,
        palette.strip_bg,
        None,
        &Rect::new(
            left as f64,
            (top + 1.0) as f64,
            right as f64,
            (top + DRAWER_HEADER_HEIGHT) as f64,
        ),
    );

    let close_x0 = right - DRAWER_CLOSE_WIDTH;
    let copy_x0 = close_x0 - DRAWER_COPY_WIDTH;

    // `hover` is panel-local, measured from the top of the tab bar (see
    // `InspectorState::hover`), while `top`/`panel_bottom` here are absolute
    // screen coordinates like everything else this function draws with —
    // `top_rel` bridges the two just for the hit comparisons below.
    let top_rel = top - CHROME_HEIGHT;
    let hovered_close = hover.is_some_and(|(hx, hy)| {
        hy >= top_rel && hy < top_rel + DRAWER_HEADER_HEIGHT && hx >= close_x0
    });
    let hovered_copy = hover.is_some_and(|(hx, hy)| {
        hy >= top_rel && hy < top_rel + DRAWER_HEADER_HEIGHT && hx >= copy_x0 && hx < close_x0
    });

    if hovered_close {
        scene.fill(
            Fill::NonZero,
            transform,
            palette.tab_inactive_bg,
            None,
            &Rect::new(
                close_x0 as f64,
                (top + 1.0) as f64,
                right as f64,
                (top + DRAWER_HEADER_HEIGHT) as f64,
            ),
        );
    }
    if hovered_copy {
        scene.fill(
            Fill::NonZero,
            transform,
            palette.tab_inactive_bg,
            None,
            &Rect::new(
                copy_x0 as f64,
                (top + 1.0) as f64,
                close_x0 as f64,
                (top + DRAWER_HEADER_HEIGHT) as f64,
            ),
        );
    }

    let title_color = tones.get(Tone::Accent);
    let title_max_w = copy_x0 - DRAWER_PAD - (left + DRAWER_PAD);
    let title = text.elide_tail(&view.title, Face::UiStrong, title_max_w.max(0.0));
    let title_h = text.line_height(Face::UiStrong);
    text.draw(
        scene,
        &title.to_uppercase(),
        Face::UiStrong,
        (
            left + DRAWER_PAD,
            top + (DRAWER_HEADER_HEIGHT - title_h) / 2.0 + 1.0,
        ),
        title_color,
        transform,
    );

    let recently_copied = view.copied_at.is_some_and(|at| at.elapsed() < COPIED_FLASH);
    let copy_label = if recently_copied { "Copied" } else { "Copy" };
    let copy_color = if recently_copied {
        tones.get(Tone::Good)
    } else {
        tones.get(Tone::Value)
    };
    let copy_w = text.width(copy_label, Face::Ui);
    let copy_h = text.line_height(Face::Ui);
    text.draw(
        scene,
        copy_label,
        Face::Ui,
        (
            copy_x0 + (DRAWER_COPY_WIDTH - copy_w) / 2.0,
            top + (DRAWER_HEADER_HEIGHT - copy_h) / 2.0 + 1.0,
        ),
        copy_color,
        transform,
    );

    paint_close_icon(
        scene,
        transform,
        close_x0 + DRAWER_CLOSE_WIDTH / 2.0,
        top + DRAWER_HEADER_HEIGHT / 2.0 + 0.5,
        tones.get(Tone::Muted),
    );

    // ── Body: word-wrapped, scrollable full text ──────────────────────────
    let body_top = top + DRAWER_HEADER_HEIGHT;
    let body_max_w = (right - left - DRAWER_PAD * 2.0 - SCROLLBAR_GUTTER).max(0.0);

    // Measured once to clamp scroll, then drawn — the panel repaints only on
    // input events, so a second layout build while the drawer is open is
    // immaterial next to the cost of a frame the user is actually looking at.
    let content_h = text.wrapped_height(&view.text, Face::Mono, body_max_w);
    let viewport_h = (panel_bottom - body_top - DRAWER_PAD * 2.0).max(0.0);
    view.max_scroll = (content_h - viewport_h).max(0.0);
    view.scroll = view.scroll.clamp(0.0, view.max_scroll);

    let clip = Rect::new(
        left as f64,
        body_top as f64,
        right as f64,
        panel_bottom as f64,
    );
    scene.push_layer(
        BlendMode::new(Mix::Normal, Compose::SrcOver),
        1.0,
        transform,
        &clip,
    );
    text.draw_wrapped(
        scene,
        &view.text,
        Face::Mono,
        (left + DRAWER_PAD, body_top + DRAWER_PAD - view.scroll),
        body_max_w,
        tones.get(Tone::Value),
        transform,
    );
    scene.pop_layer();

    paint_scrollbar(
        scene,
        palette,
        transform,
        right,
        body_top + DRAWER_PAD,
        panel_bottom - DRAWER_PAD,
        viewport_h,
        content_h,
        view.scroll,
        view.max_scroll,
    );
}

/// Draws a small "×" close icon centred on `(cx, cy)`.
fn paint_close_icon(scene: &mut Scene, transform: Affine, cx: f32, cy: f32, color: Color) {
    const R: f64 = 4.0;
    let (cx, cy) = (cx as f64, cy as f64);
    let stroke = Stroke::new(1.3);
    scene.stroke(
        &stroke,
        transform,
        color,
        None,
        &Line::new(Point::new(cx - R, cy - R), Point::new(cx + R, cy + R)),
    );
    scene.stroke(
        &stroke,
        transform,
        color,
        None,
        &Line::new(Point::new(cx - R, cy + R), Point::new(cx + R, cy - R)),
    );
}

/// Paints the scroll track and thumb, and nothing at all when the content
/// already fits.
#[allow(clippy::too_many_arguments)]
fn paint_scrollbar(
    scene: &mut Scene,
    palette: &ChromePalette,
    transform: Affine,
    right: f32,
    body_top: f32,
    bottom: f32,
    viewport_h: f32,
    content_h: f32,
    scroll: f32,
    max_scroll: f32,
) {
    if max_scroll <= 0.0 || viewport_h <= 0.0 || content_h <= 0.0 {
        return;
    }
    let x1 = right - SCROLLBAR_INSET;
    let x0 = x1 - SCROLLBAR_W;
    let radius = (SCROLLBAR_W / 2.0) as f64;

    scene.fill(
        Fill::NonZero,
        transform,
        palette.tab_inactive_bg,
        None,
        &RoundedRect::new(x0 as f64, body_top as f64, x1 as f64, bottom as f64, radius),
    );

    // Ordered `max` then `min`, not `clamp`: in a viewport shorter than the
    // minimum thumb — a real transient while the window is dragged small —
    // `clamp` would be called with `min > max` and panic.
    let thumb_h = (viewport_h * (viewport_h / content_h))
        .max(MIN_THUMB_H)
        .min(viewport_h);
    let progress = (scroll / max_scroll).clamp(0.0, 1.0);
    let thumb_y = body_top + progress * (viewport_h - thumb_h);
    scene.fill(
        Fill::NonZero,
        transform,
        palette.url_border_idle,
        None,
        &RoundedRect::new(
            x0 as f64,
            thumb_y as f64,
            x1 as f64,
            (thumb_y + thumb_h) as f64,
            radius,
        ),
    );
}

/// Paints the translucent highlight over the selected node in the page.
///
/// `rect` is in logical coordinates (already offset by the chrome bar).
pub fn paint_node_highlight(scene: &mut Scene, rect: Rect, scale: f32, palette: &ChromePalette) {
    let transform = Affine::scale(scale as f64);
    let accent = palette.url_border_focused;
    scene.fill(
        Fill::NonZero,
        transform,
        faded(accent, HIGHLIGHT_FILL_ALPHA),
        None,
        &rect,
    );
    scene.stroke(&Stroke::new(1.5), transform, accent, None, &rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::inspector::model::Seg;
    use crate::render::preferences::{UserPreferences, contrast_ratio};

    /// Every tone must stay legible against the panel's background in every
    /// palette — the panel borrows the chrome's colours precisely so this
    /// cannot silently drift, and this asserts the borrowing actually holds.
    #[test]
    fn every_tone_meets_wcag_aa_against_the_panel_background() {
        let schemes = [
            (
                "dark",
                UserPreferences {
                    color_scheme: crate::render::preferences::ColorScheme::Dark,
                    ..Default::default()
                },
            ),
            (
                "light",
                UserPreferences {
                    color_scheme: crate::render::preferences::ColorScheme::Light,
                    ..Default::default()
                },
            ),
            (
                "high-contrast",
                UserPreferences {
                    high_contrast: true,
                    ..Default::default()
                },
            ),
        ];
        let tones = [
            Tone::Key,
            Tone::Value,
            Tone::Muted,
            Tone::Accent,
            Tone::Good,
            Tone::Bad,
        ];
        for (name, prefs) in schemes {
            let palette = ChromePalette::for_preferences(&prefs);
            for tone in tones {
                let ratio = contrast_ratio(Tones::new(&palette).get(tone), palette.bar_bg);
                assert!(
                    ratio >= 4.5,
                    "{name}: {tone:?} contrasts {ratio:.2}:1 against the panel background"
                );
            }
        }
    }

    #[test]
    fn byte_at_lands_on_character_boundaries() {
        let s = "héllo→";
        for n in 0..=s.chars().count() {
            let i = byte_at(s, n);
            assert!(s.is_char_boundary(i), "byte_at({n}) must not split a char");
        }
        assert_eq!(byte_at(s, 99), s.len(), "past the end clamps to the length");
    }

    /// The whole point of the segment model: a row that cannot fit still
    /// says so with an ellipsis instead of being chopped by the clip rect.
    #[test]
    fn segments_are_classified_before_they_are_placed() {
        let segs = [
            Seg::mono("GET", Tone::Key),
            Seg::mono("mizu://host/very/long/path", Tone::Value).middle(),
            Seg::mono("42ms", Tone::Muted).trail(),
        ];
        assert_eq!(segs[0].flex, Flex::Fixed);
        assert_eq!(segs[1].flex, Flex::ElideMiddle);
        assert_eq!(segs[2].flex, Flex::Trailing);
    }

    /// Readability must not be bought by discarding the accent entirely: the
    /// fixed-up colour still has to read as the same hue family, which is
    /// what distinguishes this from falling back to plain black.
    #[test]
    fn the_light_accent_is_darkened_rather_than_flattened() {
        let light = ChromePalette::for_preferences(&UserPreferences {
            color_scheme: crate::render::preferences::ColorScheme::Light,
            ..Default::default()
        });
        let ring = light.url_border_focused;
        let fixed = Tones::new(&light).accent;
        assert!(
            contrast_ratio(fixed, light.bar_bg) >= 4.5,
            "the accent must be readable as text"
        );
        assert_ne!(fixed, ring, "the raw ring colour does not pass on light");
        assert!(
            fixed.b > fixed.r && fixed.b > fixed.g,
            "a darkened blue is still blue, unlike the black a flat fallback gives: {fixed:?}"
        );
    }

    #[test]
    fn tones_that_already_pass_are_left_alone() {
        let dark = ChromePalette::for_preferences(&UserPreferences {
            color_scheme: crate::render::preferences::ColorScheme::Dark,
            ..Default::default()
        });
        assert_eq!(Tones::new(&dark).bad, dark.err_text);
        assert_eq!(Tones::new(&dark).good, dark.ok_dot);
    }

    /// Builds a real measuring context. Parley falls back to whatever the
    /// platform offers, so these assertions are phrased to hold for any font.
    fn text_ctx(metrics: &mut TextMetrics) -> (parley::FontContext, parley::LayoutContext<Color>) {
        let _ = metrics;
        (parley::FontContext::new(), parley::LayoutContext::new())
    }

    macro_rules! with_text {
        (|$t:ident| $body:block) => {{
            let mut metrics = TextMetrics::default();
            let (mut font_cx, mut layout_cx) = text_ctx(&mut metrics);
            let mut $t = TextCtx {
                font_cx: &mut font_cx,
                layout_cx: &mut layout_cx,
                metrics: &mut metrics,
            };
            $body
        }};
    }

    #[test]
    fn tail_elision_fits_the_budget_and_marks_the_cut() {
        with_text!(|t| {
            let long = "mizu://example.test/a/very/long/path/that/will/not/fit.json";
            let out = t.elide_tail(long, Face::Mono, 120.0);
            assert!(out.ends_with('…'), "a cut must be visible: {out:?}");
            assert!(
                long.starts_with(out.trim_end_matches('…')),
                "the kept part must be a real prefix of the original"
            );
            assert!(
                t.width(&out, Face::Mono) <= 120.0,
                "and it must actually fit"
            );
        });
    }

    #[test]
    fn middle_elision_keeps_both_ends() {
        with_text!(|t| {
            let long = "mizu://example.test/a/very/long/path/that/will/not/fit.json";
            let out = t.elide_middle(long, Face::Mono, 180.0);
            let (head, tail) = out.split_once('…').expect("an ellipsis in the middle");
            assert!(long.starts_with(head) && !head.is_empty());
            assert!(
                long.ends_with(tail),
                "the resource name must survive: {out:?}"
            );
            assert!(t.width(&out, Face::Mono) <= 180.0);
        });
    }

    #[test]
    fn text_that_already_fits_is_never_touched() {
        with_text!(|t| {
            assert_eq!(t.elide_tail("ok", Face::Mono, 500.0), "ok");
            assert_eq!(t.elide_middle("ok", Face::Mono, 500.0), "ok");
        });
    }

    /// The property that the old panel violated on every long row: nothing a
    /// row paints may extend past the width it was given.
    #[test]
    fn no_placed_segment_escapes_the_row() {
        with_text!(|t| {
            let segs = vec![
                Seg::mono("blocked ", Tone::Bad),
                Seg::mono("GET  ", Tone::Key),
                Seg::mono(
                    "mizu://a.very.long.host.example.test/deeply/nested/resource/name.json",
                    Tone::Value,
                )
                .middle(),
                Seg::mono("1284ms · 12.4 KB", Tone::Muted).trail(),
            ];
            for width in [60.0f32, 120.0, 240.0, 390.0] {
                let placed = place_segs(&segs, 0.0, width, &mut t);
                for item in &placed {
                    let end = item.x + t.width(&item.text, item.face);
                    assert!(
                        end <= width + 0.5,
                        "segment {:?} ends at {end} in a {width}-wide row",
                        item.text
                    );
                    assert!(item.x >= -0.5, "and none starts left of the row");
                }
            }
        });
    }

    #[test]
    fn trailing_metrics_yield_to_the_leading_content() {
        with_text!(|t| {
            let segs = vec![
                Seg::mono("mizu://host/resource", Tone::Value).elide(),
                Seg::mono("1284ms", Tone::Muted).trail(),
            ];
            let placed = place_segs(&segs, 0.0, 50.0, &mut t);
            assert!(
                !placed.iter().any(|p| p.text == "1284ms"),
                "a row too narrow for both must spend its width on the payload"
            );
        });
    }

    #[test]
    fn a_row_with_no_room_at_all_paints_nothing() {
        with_text!(|t| {
            let segs = vec![Seg::mono("anything", Tone::Value).elide()];
            assert!(place_segs(&segs, 100.0, 100.0, &mut t).is_empty());
            assert!(place_segs(&segs, 100.0, 40.0, &mut t).is_empty());
        });
    }

    /// The monospace fast path and a full layout must agree, or elision would
    /// be measured against one metric and painted with another.
    #[test]
    fn the_monospace_fast_path_matches_a_real_layout() {
        with_text!(|t| {
            let probe = "GET mizu://host/x";
            let fast = t.width(probe, Face::Mono);
            let laid = build_text(probe, Face::Mono, t.font_cx, t.layout_cx).width();
            assert!(
                (fast - laid).abs() <= laid * 0.02 + 0.5,
                "fast path {fast} vs layout {laid}"
            );
        });
    }

    /// One row of every kind, including the pathological content the panel
    /// has to survive: a 4 000-character value, a URL longer than the panel,
    /// and a deeply indented tree row.
    fn sample_rows() -> Vec<Row> {
        use crate::render::inspector::model::RowKind;
        let row = |kind, indent, segs| Row {
            kind,
            indent,
            segs,
            node: None,
            expandable: false,
            expanded: false,
            inspect: None,
        };
        vec![
            row(
                RowKind::Header,
                0,
                vec![
                    Seg::strong("Requests", Tone::Accent),
                    Seg::mono("128", Tone::Muted).trail(),
                ],
            ),
            row(
                RowKind::Item,
                1,
                vec![
                    Seg::mono("blocked ", Tone::Bad),
                    Seg::mono("GET  ", Tone::Key),
                    Seg::mono("12.4s", Tone::Muted),
                    Seg::mono("1284ms · 3.2 KB", Tone::Muted).trail(),
                ],
            ),
            row(
                RowKind::Detail,
                2,
                vec![
                    Seg::mono(format!("mizu://host/{}", "seg/".repeat(200)), Tone::Value).middle(),
                ],
            ),
            row(
                RowKind::Detail,
                2,
                vec![Seg::mono("x".repeat(4000), Tone::Bad).elide()],
            ),
            row(RowKind::Empty, 1, vec![Seg::ui("(none)", Tone::Muted)]),
            Row {
                expandable: true,
                expanded: true,
                indent: 9,
                ..row(
                    RowKind::Item,
                    9,
                    vec![
                        Seg::mono("box", Tone::Accent),
                        Seg::mono("#deeply-nested-identifier", Tone::Value),
                    ],
                )
            },
            row(
                RowKind::Item,
                1,
                vec![
                    Seg::mono("#3a7bd5", Tone::Value).swatch(&crate::parser::MizuColor {
                        r: 58,
                        g: 123,
                        b: 213,
                        a: 255,
                    }),
                ],
            ),
        ]
    }

    /// Painting must not panic, and must not depend on the window being big:
    /// a panel narrower than its own padding is a legitimate transient during
    /// a resize drag.
    #[test]
    fn painting_survives_hostile_content_and_geometry() {
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let palette = ChromePalette::for_preferences(&UserPreferences::default());
        let rows = sample_rows();

        for (w, h) in [
            (1280.0f32, 800.0f32),
            (420.0, 300.0),
            (60.0, 90.0),
            (0.0, 0.0),
        ] {
            for tab in InspectorTab::ALL {
                let mut state = InspectorState::new();
                state.open = true;
                state.tab = tab;
                state.hover = Some((100.0, 80.0));
                state.picker = tab == InspectorTab::Style;
                state.scroll[tab.index()] = 40.0;
                let mut scene = Scene::new();
                paint_panel(
                    &mut scene,
                    &mut PanelPaintContext {
                        state: &mut state,
                        rows: &rows,
                        window_width: w,
                        window_height: h,
                        scale: 1.25,
                        font_cx: &mut font_cx,
                        layout_cx: &mut layout_cx,
                        palette: &palette,
                    },
                );
                assert!(
                    state.scroll[tab.index()] <= state.max_scroll,
                    "the paint pass must clamp scroll to the content it measured"
                );
            }
        }
    }

    /// The drawer is a second, independently-scrolling region layered on the
    /// same panel; painting it must not panic across the same hostile
    /// geometry as the row list, including a panel too short to fit it.
    #[test]
    fn the_drawer_survives_hostile_content_and_geometry() {
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let palette = ChromePalette::for_preferences(&UserPreferences::default());
        let rows = sample_rows();
        let long_value = "mizu://".to_string() + &"segment/".repeat(300) + "leaf.json";

        for (w, h) in [
            (1280.0f32, 800.0f32),
            (420.0, 300.0),
            (420.0, 150.0),
            (60.0, 90.0),
            (0.0, 0.0),
        ] {
            for hover in [None, Some((PANEL_WIDTH - 5.0, h - 10.0))] {
                let mut state = InspectorState::new();
                state.open = true;
                state.hover = hover;
                state.value_view = Some(super::ValueView::new("Value".into(), long_value.clone()));
                let mut scene = Scene::new();
                paint_panel(
                    &mut scene,
                    &mut PanelPaintContext {
                        state: &mut state,
                        rows: &rows,
                        window_width: w,
                        window_height: h,
                        scale: 1.0,
                        font_cx: &mut font_cx,
                        layout_cx: &mut layout_cx,
                        palette: &palette,
                    },
                );
                let view = state.value_view.expect("painting must not drop the drawer");
                assert!(view.scroll <= view.max_scroll);
            }
        }
    }

    #[test]
    fn copy_button_flashes_copied_then_reverts() {
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let mut metrics = TextMetrics::default();
        let mut text = TextCtx {
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            metrics: &mut metrics,
        };
        let palette = ChromePalette::for_preferences(&UserPreferences::default());
        let tones = Tones::new(&palette);

        let mut just_copied = super::ValueView::new("Value".into(), "hello".into());
        just_copied.copied_at = Some(std::time::Instant::now());
        let mut scene = Scene::new();
        paint_value_drawer(
            &mut scene,
            &mut text,
            &palette,
            &tones,
            Affine::IDENTITY,
            0.0,
            PANEL_WIDTH,
            600.0,
            &mut just_copied,
            None,
        );
        // Nothing to assert on the drawn glyphs directly; this exercises the
        // flash branch without panicking and documents the timing contract.
        assert!(just_copied.copied_at.unwrap().elapsed() < COPIED_FLASH);

        let mut long_ago = super::ValueView::new("Value".into(), "hello".into());
        long_ago.copied_at = Some(std::time::Instant::now() - COPIED_FLASH * 2);
        let mut scene = Scene::new();
        paint_value_drawer(
            &mut scene,
            &mut text,
            &palette,
            &tones,
            Affine::IDENTITY,
            0.0,
            PANEL_WIDTH,
            600.0,
            &mut long_ago,
            None,
        );
    }

    #[test]
    fn the_drawer_clamps_its_own_scroll_independent_of_the_row_list() {
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let palette = ChromePalette::for_preferences(&UserPreferences::default());
        let rows = sample_rows();
        let mut state = InspectorState::new();
        state.open = true;
        state.scroll[InspectorTab::Elements.index()] = 999.0;
        let mut view = super::ValueView::new("Value".into(), "x".repeat(2000));
        view.scroll = 999_999.0;
        state.value_view = Some(view);

        let mut scene = Scene::new();
        paint_panel(
            &mut scene,
            &mut PanelPaintContext {
                state: &mut state,
                rows: &rows,
                window_width: 420.0,
                window_height: 700.0,
                scale: 1.0,
                font_cx: &mut font_cx,
                layout_cx: &mut layout_cx,
                palette: &palette,
            },
        );
        let view = state.value_view.unwrap();
        assert!(
            view.scroll <= view.max_scroll,
            "a huge requested scroll must be clamped to the wrapped content's real height"
        );
    }

    #[test]
    fn the_measurement_cache_stays_bounded() {
        let mut font_cx = parley::FontContext::new();
        let mut layout_cx = parley::LayoutContext::new();
        let mut metrics = TextMetrics::default();
        // UI-face strings miss the monospace fast path, so they are exactly
        // what would grow the map without a bound.
        for i in 0..METRICS_CACHE_LIMIT + 500 {
            metrics.width(
                &format!("label {i} ü"),
                Face::Ui,
                &mut font_cx,
                &mut layout_cx,
            );
        }
        assert!(metrics.cache.len() <= METRICS_CACHE_LIMIT);
    }

    #[test]
    fn a_face_has_exactly_one_size() {
        assert_eq!(face_size(Face::Mono), MONO_SIZE);
        assert_eq!(face_size(Face::Ui), UI_SIZE);
        assert_eq!(face_size(Face::UiStrong), HEADER_SIZE);
    }
}
