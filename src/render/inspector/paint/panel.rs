//! The panel painters: [`PanelPaintContext`]/`paint_panel` (background, tab
//! bar, picker button, rows, scrollbar), `paint_tab_bar`, `paint_row` (plus
//! its `RowPaint` scratch struct), `paint_twisty`, `paint_value_drawer`,
//! `paint_close_icon`, `paint_scrollbar`, `paint_picker_icon`, and the
//! standalone `paint_node_highlight` used for the page-element overlay.

use vello::Scene;
use vello::kurbo::{Affine, BezPath, Circle, Line, Point, Rect, RoundedRect, Stroke};
use vello::peniko::{BlendMode, Color, Compose, Fill, Mix};

use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::inspector::model::{Face, Row, RowKind, Tone};
use crate::render::inspector::{
    DRAWER_CLOSE_WIDTH, DRAWER_COPY_WIDTH, DRAWER_HEADER_HEIGHT, DRAWER_HEIGHT, DRAWER_PAD, HPAD,
    InspectorState, InspectorTab, PANEL_WIDTH, PICKER_BTN_WIDTH, SCROLLBAR_GUTTER, TAB_BAR_HEIGHT,
    TWISTY_WIDTH, ValueView, content_top, panel_left, row_at, row_content_left, row_tops,
};
use crate::render::preferences::ChromePalette;

use super::color::{Tones, faded};
use super::constants::*;
use super::segments::place_segs;
use super::text::TextCtx;

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
pub(super) const COPIED_FLASH: std::time::Duration = std::time::Duration::from_millis(1400);

/// Paints the value-inspection drawer docked to the panel's bottom edge:
/// header (title, Copy, Close) and a word-wrapped, independently scrollable
/// body holding the row's full, untruncated text.
///
/// `hover` is the same panel-local cursor point the tab bar and rows use, so
/// the drawer's buttons light up under the cursor with no extra hit-testing
/// plumbing in the event loop.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_value_drawer(
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
