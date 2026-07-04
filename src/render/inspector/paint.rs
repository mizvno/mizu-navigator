//! Vello/Parley painting of the inspector panel and the page highlight.
//!
//! All geometry is computed in logical pixels and scaled once via the
//! `Affine::scale(dpi)` transform, mirroring the chrome bar's approach.

#![forbid(unsafe_code)]

use vello::Scene;
use vello::kurbo::{Affine, Rect, Stroke};
use vello::peniko::{Color, Fill};

use parley::style::{FontFamily, FontFamilyName, GenericFamily, LineHeight, StyleProperty};

use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::inspector::model::{Row, RowKind};
use crate::render::inspector::{
    InspectorState, InspectorTab, PANEL_WIDTH, PICKER_BTN_WIDTH, ROW_HEIGHT, TAB_BAR_HEIGHT,
    panel_left,
};

/// Font size of panel text.
const FONT_SIZE: f32 = 11.5;

const PANEL_BG: Color = Color::rgba8(0x14, 0x16, 0x1c, 0xff);
const DIVIDER: Color = Color::rgba8(0x2a, 0x2f, 0x3a, 0xff);
const TAB_ACTIVE_BG: Color = Color::rgba8(0x1f, 0x24, 0x2e, 0xff);
const PICKER_ACTIVE_BG: Color = Color::rgba8(0x3a, 0x86, 0xff, 0xff);
const SELECTION_BG: Color = Color::rgba8(0x7c, 0xc4, 0xff, 0x26);
const SCROLLBAR: Color = Color::rgba8(0x3a, 0x40, 0x4d, 0xff);

const COL_NORMAL: Color = Color::rgba8(0xd7, 0xda, 0xe0, 0xff);
const COL_HEADER: Color = Color::rgba8(0x7c, 0xc4, 0xff, 0xff);
const COL_DIM: Color = Color::rgba8(0x6b, 0x72, 0x80, 0xff);
const COL_ACCENT: Color = Color::rgba8(0x7c, 0xc4, 0xff, 0xff);
const COL_GOOD: Color = Color::rgba8(0x6f, 0xd0, 0x8c, 0xff);
const COL_BAD: Color = Color::rgba8(0xff, 0x5c, 0x5c, 0xff);

const HIGHLIGHT_FILL: Color = Color::rgba8(0x7c, 0xc4, 0xff, 0x2d);
const HIGHLIGHT_BORDER: Color = Color::rgba8(0x7c, 0xc4, 0xff, 0xd0);

fn row_color(kind: RowKind) -> Color {
    match kind {
        RowKind::Header => COL_HEADER,
        RowKind::Normal => COL_NORMAL,
        RowKind::Dim => COL_DIM,
        RowKind::Accent => COL_ACCENT,
        RowKind::Good => COL_GOOD,
        RowKind::Bad => COL_BAD,
    }
}

/// Builds a single-line monospace text layout for a panel row.
fn build_row_layout(
    text: &str,
    color: Color,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<Color>,
) -> parley::Layout<Color> {
    let fallbacks = vec![
        FontFamilyName::named("Consolas"),
        FontFamilyName::named("Cascadia Code"),
        FontFamilyName::named("Courier New"),
        FontFamilyName::Generic(GenericFamily::Monospace),
        FontFamilyName::Generic(GenericFamily::SansSerif),
    ];
    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::FontFamily(FontFamily::List(
        std::borrow::Cow::Owned(fallbacks),
    )));
    builder.push_default(StyleProperty::FontSize(FONT_SIZE));
    builder.push_default(StyleProperty::Brush(color));
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(1.0)));
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    layout
}

/// Draws a prebuilt layout at logical `(x, y)`.
fn draw_layout(
    scene: &mut Scene,
    layout: &parley::Layout<Color>,
    x: f32,
    y: f32,
    color: Color,
    transform: Affine,
) {
    let y_offset = layout
        .lines()
        .next()
        .map(|l| l.metrics().ascent - l.metrics().baseline)
        .unwrap_or(0.0);
    for line in layout.lines() {
        for item in line.items() {
            if let parley::layout::PositionedLayoutItem::GlyphRun(run) = item {
                let font_data = run.run().font();
                let (arc_data, id) = font_data.data.clone().into_raw_parts();
                let blob = vello::peniko::Blob::from_raw_parts(arc_data, id);
                let vello_font = vello::peniko::Font::new(blob, font_data.index);
                let glyphs = run.positioned_glyphs().map(|g| vello::glyph::Glyph {
                    id: g.id,
                    x: g.x,
                    y: g.y,
                });
                scene
                    .draw_glyphs(&vello_font)
                    .font_size(FONT_SIZE)
                    .brush(color)
                    .transform(transform * Affine::translate((x as f64, (y + y_offset) as f64)))
                    .draw(Fill::NonZero, glyphs);
            }
        }
    }
}

/// Paints the docked panel: background, tab bar, picker button, visible rows,
/// and scrollbar.  Also clamps the active tab's scroll offset against the
/// current content height (stored back into `state.max_scroll`).
#[allow(clippy::too_many_arguments)]
pub fn paint_panel(
    scene: &mut Scene,
    state: &mut InspectorState,
    rows: &[Row],
    window_width: f32,
    window_height: f32,
    scale: f32,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<Color>,
) {
    let transform = Affine::scale(scale as f64);
    let left = panel_left(window_width);
    let top = CHROME_HEIGHT;

    // ── Panel background + divider ───────────────────────────────────────
    let panel_rect = Rect::new(
        left as f64,
        top as f64,
        window_width as f64,
        window_height as f64,
    );
    scene.fill(Fill::NonZero, transform, PANEL_BG, None, &panel_rect);
    let divider = Rect::new(
        left as f64,
        top as f64,
        (left + 1.0) as f64,
        window_height as f64,
    );
    scene.fill(Fill::NonZero, transform, DIVIDER, None, &divider);

    // ── Tab bar ──────────────────────────────────────────────────────────
    let tab_strip_width = PANEL_WIDTH - PICKER_BTN_WIDTH;
    let tab_width = tab_strip_width / InspectorTab::ALL.len() as f32;
    for (i, tab) in InspectorTab::ALL.iter().enumerate() {
        let x0 = left + i as f32 * tab_width;
        if *tab == state.tab {
            let r = Rect::new(
                x0 as f64,
                top as f64,
                (x0 + tab_width) as f64,
                (top + TAB_BAR_HEIGHT) as f64,
            );
            scene.fill(Fill::NonZero, transform, TAB_ACTIVE_BG, None, &r);
        }
        let color = if *tab == state.tab { COL_ACCENT } else { COL_DIM };
        let layout = build_row_layout(tab.label(), color, font_cx, layout_cx);
        let tx = x0 + (tab_width - layout.width()).max(0.0) / 2.0;
        let ty = top + (TAB_BAR_HEIGHT - layout.height()).max(0.0) / 2.0;
        draw_layout(scene, &layout, tx, ty, color, transform);
    }
    // Picker button.
    let picker_x0 = left + tab_strip_width;
    if state.picker {
        let r = Rect::new(
            picker_x0 as f64,
            top as f64,
            (picker_x0 + PICKER_BTN_WIDTH) as f64,
            (top + TAB_BAR_HEIGHT) as f64,
        );
        scene.fill(Fill::NonZero, transform, PICKER_ACTIVE_BG, None, &r);
    }
    let picker_color = if state.picker { COL_NORMAL } else { COL_DIM };
    let picker_layout = build_row_layout("[+]", picker_color, font_cx, layout_cx);
    let px = picker_x0 + (PICKER_BTN_WIDTH - picker_layout.width()).max(0.0) / 2.0;
    let py = top + (TAB_BAR_HEIGHT - picker_layout.height()).max(0.0) / 2.0;
    draw_layout(scene, &picker_layout, px, py, picker_color, transform);

    let bar_divider = Rect::new(
        left as f64,
        (top + TAB_BAR_HEIGHT) as f64,
        window_width as f64,
        (top + TAB_BAR_HEIGHT + 1.0) as f64,
    );
    scene.fill(Fill::NonZero, transform, DIVIDER, None, &bar_divider);

    // ── Content: scroll clamp + visible slice ────────────────────────────
    let content_top = top + TAB_BAR_HEIGHT + 1.0;
    let viewport_h = (window_height - content_top).max(0.0);
    let content_h = rows.len() as f32 * ROW_HEIGHT;
    state.max_scroll = (content_h - viewport_h).max(0.0);
    let idx = state.tab.index();
    state.scroll[idx] = state.scroll[idx].clamp(0.0, state.max_scroll);
    let scroll = state.scroll[idx];

    let clip = Rect::new(
        left as f64,
        content_top as f64,
        window_width as f64,
        window_height as f64,
    );
    scene.push_layer(
        vello::peniko::BlendMode::new(vello::peniko::Mix::Normal, vello::peniko::Compose::SrcOver),
        1.0,
        transform,
        &clip,
    );

    let first = (scroll / ROW_HEIGHT).floor() as usize;
    let visible = (viewport_h / ROW_HEIGHT).ceil() as usize + 1;
    let last = (first + visible).min(rows.len());
    for (i, row) in rows
        .iter()
        .enumerate()
        .skip(first)
        .take(last.saturating_sub(first))
    {
        let y = content_top + i as f32 * ROW_HEIGHT - scroll;
        // Selection background for node rows.
        if row.node.is_some() && row.node == state.selected {
            let r = Rect::new(
                left as f64,
                y as f64,
                window_width as f64,
                (y + ROW_HEIGHT) as f64,
            );
            scene.fill(Fill::NonZero, transform, SELECTION_BG, None, &r);
        }
        let color = row_color(row.kind);
        let x = left + 8.0 + row.indent as f32 * 12.0;
        let layout = build_row_layout(&row.text, color, font_cx, layout_cx);
        let ty = y + (ROW_HEIGHT - layout.height()).max(0.0) / 2.0;
        draw_layout(scene, &layout, x, ty, color, transform);
    }

    scene.pop_layer();

    // ── Scrollbar ────────────────────────────────────────────────────────
    if state.max_scroll > 0.0 && content_h > 0.0 {
        let thumb_h = (viewport_h / content_h * viewport_h).max(24.0);
        let thumb_y = content_top + (scroll / state.max_scroll) * (viewport_h - thumb_h);
        let r = Rect::new(
            (window_width - 5.0) as f64,
            thumb_y as f64,
            (window_width - 2.0) as f64,
            (thumb_y + thumb_h) as f64,
        );
        scene.fill(Fill::NonZero, transform, SCROLLBAR, None, &r);
    }
}

/// Paints the translucent highlight over the selected node in the page.
///
/// `rect` is in logical coordinates (already offset by the chrome bar).
pub fn paint_node_highlight(scene: &mut Scene, rect: Rect, scale: f32) {
    let transform = Affine::scale(scale as f64);
    scene.fill(Fill::NonZero, transform, HIGHLIGHT_FILL, None, &rect);
    scene.stroke(
        &Stroke::new(1.5),
        transform,
        HIGHLIGHT_BORDER,
        None,
        &rect,
    );
}
