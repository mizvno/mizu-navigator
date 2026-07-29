//! History sidebar panel: a left-docked, scrollable list of visited pages
//! grouped by day, toggled with **Ctrl+H** or the chrome bar's "≡" button.
//!
//! ## Placement
//!
//! The panel is docked to the **left** edge, below the chrome, and is
//! window-level (shared across tabs) because [`HistoryLog`] is — matching
//! Firefox's history model, where the sidebar shows where you have been in
//! this window, not in one tab.
//!
//! It *overlays* the document rather than reserving width from it (the
//! inspector, being right-docked, can shrink the viewport without moving
//! anything; a left dock would have to offset every paint and hit test by
//! the panel width). Overlaying is also what Chrome and Edge do with their
//! side panels, so nothing about the page reflows when history opens.
//!
//! ## Styling
//!
//! Every color comes from the same [`ChromePalette`] the tab strip and URL
//! bar paint with, so the panel follows light/dark/high-contrast exactly as
//! the rest of the chrome does — no palette of its own to drift.
//!
//! ## Layout
//!
//! [`layout_rows`] is the single source of truth for the panel's vertical
//! geometry; both [`paint_history_sidebar`] and [`history_sidebar_hit`]
//! consume it, so what is painted and what is clickable cannot disagree.

#![forbid(unsafe_code)]

use parley::style::{FontFamily, FontFamilyName, GenericFamily, LineHeight, StyleProperty};
use vello::{
    Scene,
    kurbo::{Affine, Rect, RoundedRect},
    peniko::{BlendMode, Color, Compose, Fill, Mix},
};

use crate::render::preferences::ChromePalette;
use crate::render::window::history::HistoryLog;

// ── Geometry ──────────────────────────────────────────────────────────────────

/// Logical width of the docked sidebar panel, including its right divider.
pub const SIDEBAR_WIDTH: f32 = 272.0;

/// Height of the sidebar's header band (title + "Clear" button).
const HEADER_HEIGHT: f32 = 30.0;

/// Height of a day-group label row.
const GROUP_ROW_H: f32 = 26.0;

/// Height of one visit row (two lines of text plus breathing room).
const ROW_H: f32 = 38.0;

/// Horizontal padding inside the panel.
const HPAD: f32 = 12.0;

/// Width of the "Clear" button in the header.
const CLEAR_BTN_W: f32 = 62.0;

/// Vertical inset of the "Clear" button within the header band.
const CLEAR_BTN_INSET: f32 = 5.0;

/// Width of the accent bar marking the hovered row.
const ACCENT_W: f32 = 2.0;

/// Width of the scrollbar thumb, and its inset from the panel's right edge.
const SCROLLBAR_W: f32 = 3.0;
const SCROLLBAR_INSET: f32 = 3.0;

/// Font size of a row's title line.
const TITLE_FONT_SIZE: f32 = 12.0;

/// Font size of a row's URL line and of the group labels.
const SMALL_FONT_SIZE: f32 = 10.5;

/// Scroll distance, in logical pixels, per unit of wheel delta.
const SCROLL_STEP: f32 = 2.0;

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the X coordinate of the panel's right edge: the panel occupies
/// `[0, panel_right()]`, so this is what callers test a cursor against.
pub fn panel_right() -> f32 {
    SIDEBAR_WIDTH
}

/// Whether a logical X coordinate falls inside the docked panel.
pub fn contains_x(x: f32) -> bool {
    (0.0..panel_right()).contains(&x)
}

/// Result of a hit test against the history sidebar panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySidebarHit {
    /// Outside the panel — the caller should keep routing the event.
    None,
    /// Panel chrome with no action of its own: consume the event, do nothing.
    Background,
    /// The "Clear" button.
    Clear,
    /// A visit row, carrying its index in the log's newest-first order.
    Entry(usize),
}

/// Hit-tests a logical `(x, y)` point against the panel.
///
/// Returns [`HistorySidebarHit::None`] for any point outside the panel's
/// column, so the caller can route the event onward without the panel having
/// to know what else is on screen.
pub fn history_sidebar_hit(
    x: f32,
    y: f32,
    log: &HistoryLog,
    scroll_offset: f32,
    chrome_height: f32,
) -> HistorySidebarHit {
    if !contains_x(x) || y < chrome_height {
        return HistorySidebarHit::None;
    }

    let content_y = y - chrome_height;
    if content_y < HEADER_HEIGHT {
        let clear = clear_button_rect(chrome_height);
        if (clear.x0..clear.x1).contains(&(x as f64)) {
            return HistorySidebarHit::Clear;
        }
        return HistorySidebarHit::Background;
    }

    // Below the header: document coordinates, i.e. scrolled content space.
    let doc_y = content_y - HEADER_HEIGHT + scroll_offset;
    for row in layout_rows(log) {
        if let Row::Entry { index, top } = row
            && (top..top + ROW_H).contains(&doc_y)
        {
            return HistorySidebarHit::Entry(index);
        }
    }
    // Panel chrome, a group header, or empty space past the last row: all
    // consume the click so it never reaches the page underneath.
    HistorySidebarHit::Background
}

/// Returns the index of the visit row under `(x, y)`, if any.
///
/// The hover highlight only ever tracks rows, so this saves callers from
/// matching on a [`HistorySidebarHit`] they would immediately discard.
pub fn hovered_entry(
    x: f32,
    y: f32,
    log: &HistoryLog,
    scroll_offset: f32,
    chrome_height: f32,
) -> Option<usize> {
    match history_sidebar_hit(x, y, log, scroll_offset, chrome_height) {
        HistorySidebarHit::Entry(index) => Some(index),
        _ => None,
    }
}

/// Everything [`paint_history_sidebar`] needs besides the [`Scene`] it draws
/// into, mirroring the inspector's `PanelPaintContext` convention.
pub struct SidebarPaintContext<'a> {
    /// The visits to display.
    pub log: &'a HistoryLog,
    /// Current vertical scroll offset, in logical pixels.
    pub scroll_offset: f32,
    /// Index of the row under the cursor, if any.
    pub hovered: Option<usize>,
    /// The chrome palette — the panel's only source of color.
    pub palette: &'a ChromePalette,
    /// Parley contexts, threaded through like every other paint site.
    pub font_cx: &'a mut parley::FontContext,
    /// Layout context paired with `font_cx`.
    pub layout_cx: &'a mut parley::LayoutContext<Color>,
    /// DPI transform applied to the logical coordinates computed here.
    pub transform: Affine,
    /// Window height in logical pixels; the panel runs to the bottom of it.
    pub window_height: f32,
    /// Height of the browser chrome, i.e. the panel's top edge.
    pub chrome_height: f32,
}

/// Paints the history sidebar. Call only while the panel is open.
pub fn paint_history_sidebar(scene: &mut Scene, ctx: &mut SidebarPaintContext<'_>) {
    let right = panel_right() as f64;
    let top = ctx.chrome_height as f64;
    let bottom = ctx.window_height.max(ctx.chrome_height) as f64;
    let content_top = ctx.chrome_height + HEADER_HEIGHT;

    // ── Panel body and its edge ───────────────────────────────────────────────
    scene.fill(
        Fill::NonZero,
        ctx.transform,
        ctx.palette.bar_bg,
        None,
        &Rect::new(0.0, top, right, bottom),
    );

    paint_header(scene, ctx);

    // ── Scrollable rows, clipped to the area below the header ─────────────────
    let clip = Rect::new(0.0, content_top as f64, right, bottom);
    scene.push_layer(
        BlendMode::new(Mix::Normal, Compose::SrcOver),
        1.0,
        ctx.transform,
        &clip,
    );

    if ctx.log.is_empty() {
        let empty = build_text(
            "No pages visited yet.",
            SMALL_FONT_SIZE,
            false,
            ctx.font_cx,
            ctx.layout_cx,
        );
        draw_text(
            scene,
            &empty,
            HPAD,
            content_top + HPAD,
            ctx.palette.tab_text_inactive,
            ctx.transform,
        );
    }

    // Rows are laid out in document space; `origin` maps that to the window.
    let origin = content_top - ctx.scroll_offset;
    for row in layout_rows(ctx.log) {
        let y = origin + row.top();
        // Skip rows scrolled out of view — the log holds up to 5 000 visits,
        // and laying out glyphs for all of them every frame would be waste.
        if y + row.height() < content_top {
            continue;
        }
        if y > ctx.window_height {
            break;
        }
        match row {
            Row::Group { label, .. } => paint_group_row(scene, ctx, &label, y),
            Row::Entry { index, .. } => paint_entry_row(scene, ctx, index, y),
        }
    }

    scene.pop_layer();

    paint_scrollbar(scene, ctx);

    // The divider is painted last so neither the rows nor the scrollbar can
    // sit on top of the panel's edge.
    scene.fill(
        Fill::NonZero,
        ctx.transform,
        ctx.palette.url_border_idle,
        None,
        &Rect::new(right - 1.0, top, right, bottom),
    );
}

/// Total logical height of the panel's scrollable content.
pub fn total_content_height(log: &HistoryLog) -> f32 {
    layout_rows(log)
        .last()
        .map_or(0.0, |row| row.top() + row.height())
}

/// Clamps a scroll offset to the panel's scrollable range.
pub fn clamp_scroll(offset: f32, log: &HistoryLog, window_height: f32, chrome_height: f32) -> f32 {
    let visible = visible_height(window_height, chrome_height);
    offset.clamp(0.0, (total_content_height(log) - visible).max(0.0))
}

/// Applies a wheel `delta_y` (in logical pixels) to `offset`, clamped.
pub fn scroll_by(
    offset: f32,
    delta_y: f32,
    log: &HistoryLog,
    window_height: f32,
    chrome_height: f32,
) -> f32 {
    clamp_scroll(offset + delta_y * SCROLL_STEP, log, window_height, chrome_height)
}

// ── Row layout ────────────────────────────────────────────────────────────────

/// One laid-out row in the panel's scrollable content, positioned in
/// document space (0 = first row, before any scrolling).
#[derive(Debug, Clone, PartialEq)]
enum Row {
    /// A day-group header, e.g. "Today".
    Group { label: String, top: f32 },
    /// A visit, identified by its newest-first index in the log.
    Entry { index: usize, top: f32 },
}

impl Row {
    /// This row's height in logical pixels.
    fn height(&self) -> f32 {
        match self {
            Row::Group { .. } => GROUP_ROW_H,
            Row::Entry { .. } => ROW_H,
        }
    }

    /// This row's top edge in document space.
    fn top(&self) -> f32 {
        match self {
            Row::Group { top, .. } | Row::Entry { top, .. } => *top,
        }
    }
}

/// Lays out every row of the panel, top to bottom.
///
/// Both painting and hit testing walk this list, which is what keeps what is
/// on screen and what is clickable from drifting apart as metrics change.
fn layout_rows(log: &HistoryLog) -> Vec<Row> {
    let groups = log.grouped_by_day();
    let mut rows = Vec::with_capacity(log.len() + groups.len());
    let mut top = 0.0f32;
    let mut index = 0usize;

    for (label, records) in groups {
        rows.push(Row::Group { label, top });
        top += GROUP_ROW_H;
        for _ in records {
            rows.push(Row::Entry { index, top });
            top += ROW_H;
            index += 1;
        }
    }
    rows
}

// ── Painting ──────────────────────────────────────────────────────────────────

/// The header band's "Clear" button rectangle.
fn clear_button_rect(chrome_height: f32) -> Rect {
    let right = panel_right() - HPAD;
    Rect::new(
        (right - CLEAR_BTN_W) as f64,
        (chrome_height + CLEAR_BTN_INSET) as f64,
        right as f64,
        (chrome_height + HEADER_HEIGHT - CLEAR_BTN_INSET) as f64,
    )
}

/// Height of the panel's visible scroll viewport.
fn visible_height(window_height: f32, chrome_height: f32) -> f32 {
    (window_height - chrome_height - HEADER_HEIGHT).max(0.0)
}

/// Paints the header band: title, "Clear" button, and the hairline that
/// separates the band from the list.
fn paint_header(scene: &mut Scene, ctx: &mut SidebarPaintContext<'_>) {
    let top = ctx.chrome_height as f64;
    let right = panel_right() as f64;
    let band = Rect::new(0.0, top, right, top + HEADER_HEIGHT as f64);
    scene.fill(Fill::NonZero, ctx.transform, ctx.palette.strip_bg, None, &band);

    let title = build_text("History", TITLE_FONT_SIZE, false, ctx.font_cx, ctx.layout_cx);
    draw_text(
        scene,
        &title,
        HPAD,
        ctx.chrome_height + (HEADER_HEIGHT - title.height()) / 2.0,
        ctx.palette.tab_text,
        ctx.transform,
    );

    // Styled exactly like the chrome's Back/Reload/Forward buttons: same
    // background, same glyph color, same 3px corner radius.
    let btn = clear_button_rect(ctx.chrome_height);
    scene.fill(
        Fill::NonZero,
        ctx.transform,
        ctx.palette.btn_bg,
        None,
        &RoundedRect::from_rect(btn, 3.0),
    );
    let label = build_text("Clear", SMALL_FONT_SIZE, false, ctx.font_cx, ctx.layout_cx);
    draw_text(
        scene,
        &label,
        btn.x0 as f32 + (CLEAR_BTN_W - label.width()) / 2.0,
        btn.y0 as f32 + ((btn.height() as f32) - label.height()) / 2.0,
        ctx.palette.btn_text,
        ctx.transform,
    );

    let hairline = Rect::new(
        0.0,
        top + HEADER_HEIGHT as f64 - 1.0,
        right,
        top + HEADER_HEIGHT as f64,
    );
    scene.fill(Fill::NonZero, ctx.transform, ctx.palette.url_border_idle, None, &hairline);
}

/// Paints a day-group header: a quiet label with a rule running to the
/// panel's edge, so groups read as sections without shouting.
fn paint_group_row(scene: &mut Scene, ctx: &mut SidebarPaintContext<'_>, label: &str, y: f32) {
    let text = build_text(label, SMALL_FONT_SIZE, false, ctx.font_cx, ctx.layout_cx);
    draw_text(
        scene,
        &text,
        HPAD,
        y + (GROUP_ROW_H - text.height()) / 2.0,
        ctx.palette.tab_text_inactive,
        ctx.transform,
    );

    let rule_left = HPAD + text.width() + 8.0;
    let rule_y = (y + GROUP_ROW_H / 2.0) as f64;
    if rule_left < panel_right() - HPAD {
        scene.fill(
            Fill::NonZero,
            ctx.transform,
            ctx.palette.url_border_idle,
            None,
            &Rect::new(
                rule_left as f64,
                rule_y,
                (panel_right() - HPAD) as f64,
                rule_y + 1.0,
            ),
        );
    }
}

/// Paints one visit: title line, URL line, and the hover treatment.
fn paint_entry_row(scene: &mut Scene, ctx: &mut SidebarPaintContext<'_>, index: usize, y: f32) {
    let Some(record) = ctx.log.get(index) else {
        return;
    };
    // Borrowed out of `ctx.log` before `ctx.font_cx` is borrowed mutably.
    let label = record.display_label().to_owned();
    let url = record.url.clone();
    let show_url = !record.title.is_empty() && record.title != record.url;
    let hovered = ctx.hovered == Some(index);

    if hovered {
        scene.fill(
            Fill::NonZero,
            ctx.transform,
            ctx.palette.tab_inactive_bg,
            None,
            &Rect::new(0.0, y as f64, panel_right() as f64, (y + ROW_H) as f64),
        );
        scene.fill(
            Fill::NonZero,
            ctx.transform,
            ctx.palette.url_border_focused,
            None,
            &Rect::new(0.0, y as f64, ACCENT_W as f64, (y + ROW_H) as f64),
        );
    }

    let text_left = HPAD;
    let max_width = panel_right() - HPAD * 2.0;

    let elided = truncate_to_width(
        &label,
        max_width,
        TITLE_FONT_SIZE,
        false,
        ctx.font_cx,
        ctx.layout_cx,
    );
    let title = build_text(&elided, TITLE_FONT_SIZE, false, ctx.font_cx, ctx.layout_cx);
    draw_text(
        scene,
        &title,
        text_left,
        y + 6.0,
        ctx.palette.tab_text,
        ctx.transform,
    );

    // The URL is redundant under a row whose title *is* the URL.
    if show_url {
        let elided = truncate_to_width(
            &url,
            max_width,
            SMALL_FONT_SIZE,
            true,
            ctx.font_cx,
            ctx.layout_cx,
        );
        // Monospace, like the URL bar this line mirrors.
        let url_line = build_text(&elided, SMALL_FONT_SIZE, true, ctx.font_cx, ctx.layout_cx);
        draw_text(
            scene,
            &url_line,
            text_left,
            y + 6.0 + title.height() + 3.0,
            ctx.palette.tab_text_inactive,
            ctx.transform,
        );
    }
}

/// Paints the scroll thumb, and nothing at all when everything already fits.
fn paint_scrollbar(scene: &mut Scene, ctx: &mut SidebarPaintContext<'_>) {
    let visible = visible_height(ctx.window_height, ctx.chrome_height);
    let content = total_content_height(ctx.log);
    if visible <= 0.0 || content <= visible {
        return;
    }

    let track_top = ctx.chrome_height + HEADER_HEIGHT;
    let thumb_h = (visible * (visible / content)).max(24.0);
    let progress = (ctx.scroll_offset / (content - visible)).clamp(0.0, 1.0);
    let thumb_y = track_top + progress * (visible - thumb_h);
    let right = panel_right() - SCROLLBAR_INSET;

    scene.fill(
        Fill::NonZero,
        ctx.transform,
        ctx.palette.url_border_idle,
        None,
        &RoundedRect::new(
            (right - SCROLLBAR_W) as f64,
            thumb_y as f64,
            right as f64,
            (thumb_y + thumb_h) as f64,
            (SCROLLBAR_W / 2.0) as f64,
        ),
    );
}

// ── Text helpers ──────────────────────────────────────────────────────────────

/// Builds a single-line layout at `font_size`, in the chrome's monospace
/// stack when `mono` is set and its UI sans stack otherwise.
fn build_text(
    text: &str,
    font_size: f32,
    mono: bool,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<Color>,
) -> parley::Layout<Color> {
    let fallbacks = if mono {
        vec![
            FontFamilyName::named("Consolas"),
            FontFamilyName::named("Cascadia Code"),
            FontFamilyName::named("Courier New"),
            FontFamilyName::Generic(GenericFamily::Monospace),
        ]
    } else {
        vec![
            FontFamilyName::named("Segoe UI"),
            FontFamilyName::named("Arial"),
            FontFamilyName::Generic(GenericFamily::SansSerif),
        ]
    };
    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::FontFamily(FontFamily::List(
        std::borrow::Cow::Owned(fallbacks),
    )));
    builder.push_default(StyleProperty::FontSize(font_size));
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(1.0)));
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    layout
}

/// Draws a prebuilt layout with its top-left corner at logical `(x, y)`.
fn draw_text(
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
    let font_size = metrics.ascent + metrics.descent;

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
                    .font_size(font_size)
                    .brush(color)
                    .transform(transform * Affine::translate((x as f64, (y + y_offset) as f64)))
                    .draw(Fill::NonZero, glyphs);
            }
        }
    }
}

/// Truncates `text` with an ellipsis so it fits within `max_width`.
///
/// Binary-searches the character boundaries rather than shrinking one
/// character at a time: a long URL in a narrow panel would otherwise cost
/// hundreds of text layouts per row, per frame.
fn truncate_to_width(
    text: &str,
    max_width: f32,
    font_size: f32,
    mono: bool,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<Color>,
) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    if build_text(text, font_size, mono, font_cx, layout_cx).width() <= max_width {
        return text.to_owned();
    }

    let boundaries: Vec<usize> = (0..=text.len())
        .filter(|&i| text.is_char_boundary(i))
        .collect();
    // Largest prefix that still fits once the ellipsis is appended.
    let mut lo = 0usize;
    let mut hi = boundaries.len() - 1;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate = format!("{}…", &text[..boundaries[mid]]);
        if build_text(&candidate, font_size, mono, font_cx, layout_cx).width() <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return String::new();
    }
    format!("{}…", &text[..boundaries[lo]])
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::window::history::VisitRecord;

    const CHROME: f32 = 54.0;

    fn log_with(n: usize) -> HistoryLog {
        let mut log = HistoryLog::default();
        for i in 0..n {
            log.push(VisitRecord::new(format!("mizu://test/{i}"), format!("Page {i}")));
        }
        log
    }

    /// Y coordinate of the vertical centre of row `index`, unscrolled.
    fn row_centre_y(index: usize) -> f32 {
        CHROME + HEADER_HEIGHT + GROUP_ROW_H + index as f32 * ROW_H + ROW_H / 2.0
    }

    #[test]
    fn panel_is_docked_to_the_left_edge() {
        assert!(contains_x(0.0), "the panel starts at the window's left edge");
        assert!(contains_x(SIDEBAR_WIDTH - 1.0));
        assert!(!contains_x(SIDEBAR_WIDTH), "and ends at its own width");
    }

    #[test]
    fn points_outside_the_panel_are_not_hits() {
        let log = log_with(3);
        assert_eq!(
            history_sidebar_hit(SIDEBAR_WIDTH + 1.0, row_centre_y(0), &log, 0.0, CHROME),
            HistorySidebarHit::None,
            "content to the right of the panel must stay clickable"
        );
        assert_eq!(
            history_sidebar_hit(10.0, CHROME - 1.0, &log, 0.0, CHROME),
            HistorySidebarHit::None,
            "the chrome bar owns everything above the panel"
        );
    }

    #[test]
    fn rows_hit_test_newest_first() {
        let log = log_with(3);
        // Newest visit is "mizu://test/2", so it is row 0.
        assert_eq!(
            history_sidebar_hit(40.0, row_centre_y(0), &log, 0.0, CHROME),
            HistorySidebarHit::Entry(0)
        );
        assert_eq!(
            history_sidebar_hit(40.0, row_centre_y(2), &log, 0.0, CHROME),
            HistorySidebarHit::Entry(2)
        );
        assert_eq!(
            log.get(0).unwrap().url,
            "mizu://test/2",
            "index 0 must be the most recent visit"
        );
    }

    #[test]
    fn scrolling_moves_which_row_is_under_the_cursor() {
        let log = log_with(10);
        let y = row_centre_y(0);
        assert_eq!(
            history_sidebar_hit(40.0, y, &log, ROW_H * 3.0, CHROME),
            HistorySidebarHit::Entry(3),
            "a three-row scroll must bring row 3 under the first row's position"
        );
    }

    #[test]
    fn group_headers_are_not_clickable_as_entries() {
        let log = log_with(3);
        let group_y = CHROME + HEADER_HEIGHT + GROUP_ROW_H / 2.0;
        assert_eq!(
            history_sidebar_hit(40.0, group_y, &log, 0.0, CHROME),
            HistorySidebarHit::Background
        );
    }

    #[test]
    fn header_zone_separates_the_clear_button_from_the_title() {
        let log = log_with(1);
        let header_y = CHROME + HEADER_HEIGHT / 2.0;
        assert_eq!(
            history_sidebar_hit(HPAD, header_y, &log, 0.0, CHROME),
            HistorySidebarHit::Background,
            "the title is not a button"
        );
        assert_eq!(
            history_sidebar_hit(SIDEBAR_WIDTH - HPAD - 2.0, header_y, &log, 0.0, CHROME),
            HistorySidebarHit::Clear
        );
    }

    #[test]
    fn empty_log_has_no_content_and_no_scroll() {
        let log = HistoryLog::default();
        assert_eq!(total_content_height(&log), 0.0);
        assert_eq!(clamp_scroll(500.0, &log, 800.0, CHROME), 0.0);
        assert_eq!(
            history_sidebar_hit(40.0, 400.0, &log, 0.0, CHROME),
            HistorySidebarHit::Background,
            "an empty panel still swallows clicks rather than leaking them to the page"
        );
    }

    #[test]
    fn content_height_covers_every_row_and_its_group_header() {
        let log = log_with(4);
        // Four same-day visits under a single "Today" header.
        assert_eq!(total_content_height(&log), GROUP_ROW_H + 4.0 * ROW_H);
    }

    #[test]
    fn scroll_is_clamped_to_the_scrollable_range() {
        let log = log_with(40);
        let window_height = 400.0;
        let max = total_content_height(&log) - visible_height(window_height, CHROME);
        assert_eq!(clamp_scroll(-50.0, &log, window_height, CHROME), 0.0);
        assert_eq!(clamp_scroll(1e6, &log, window_height, CHROME), max);
        assert_eq!(
            scroll_by(max, 10.0, &log, window_height, CHROME),
            max,
            "scrolling past the end must not run off"
        );
    }

    #[test]
    fn short_lists_do_not_scroll_at_all() {
        let log = log_with(2);
        assert_eq!(
            scroll_by(0.0, 50.0, &log, 900.0, CHROME),
            0.0,
            "content that already fits has nothing to scroll"
        );
    }
}
