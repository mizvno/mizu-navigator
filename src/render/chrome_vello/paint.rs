//! The chrome bar's main paint function: tab strip, nav buttons, URL bar,
//! status indicator, and the autocomplete dropdown.

use super::cursor::*;
use super::text_layout::*;
use super::*;

// ── Main paint function ───────────────────────────────────────────────────────

/// Truncates `text` with an ellipsis so it fits `max_width` logical pixels.
///
/// Measures with the same layout builder the strip paints with, so the elision
/// point matches what is actually drawn.
pub(super) fn elide_to_width(
    text: &str,
    max_width: f32,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    if build_chrome_text_layout(text, font_cx, layout_cx).width() <= max_width {
        return text.to_owned();
    }
    let mut end = text.len();
    while end > 0 {
        // Walk back to a char boundary so multi-byte titles never panic.
        end -= 1;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = format!("{}…", &text[..end]);
        if build_chrome_text_layout(&candidate, font_cx, layout_cx).width() <= max_width {
            return candidate;
        }
    }
    String::new()
}

/// Context for [`paint_chrome`], bundling everything but the `Scene` it
/// draws into.
pub struct ChromePaintContext<'a> {
    /// URL bar text/focus/selection/loading state.
    pub state: &'a ChromeState,
    /// Logical window width.
    pub window_width: f32,
    /// `Affine::scale(dpi_scale)`, so the chrome scales on high-DPI displays.
    pub transform: Affine,
    /// Elapsed time in milliseconds, for the URL-bar cursor blink and the
    /// loading-indicator pulse animation.
    pub elapsed_ms: u64,
    /// Parley font context.
    pub font_cx: &'a mut parley::FontContext,
    /// Parley layout context.
    pub layout_cx: &'a mut parley::LayoutContext<vello::peniko::Color>,
    /// Whether the Back button is enabled (non-empty back-navigation stack).
    pub can_go_back: bool,
    /// Whether the Forward button is enabled (non-empty forward stack).
    pub can_go_forward: bool,
    /// Resolved light/dark color palette.
    pub palette: &'a ChromePalette,
    /// One entry per open tab, in strip order.
    ///
    /// A flat, borrow-free view rather than the tabs themselves, so painting
    /// stays independent of `TabState` and unit-testable.
    pub tabs: &'a [TabStripEntry],
    /// Whether the history sidebar is currently open. When `true`, the
    /// history button renders with an active/pressed background.
    pub history_sidebar_open: bool,
}

/// What the strip needs to know about one tab.
pub struct TabStripEntry {
    /// Display title, already bidi-sanitised by the caller.
    pub title: String,
    /// Whether this tab is the visible one.
    pub active: bool,
}

/// Renders the browser chrome bar into `scene`.
///
/// All coordinates are **logical pixels**. `ctx.transform` should be
/// `Affine::scale(dpi_scale)` so the chrome scales on high-DPI displays.
pub fn paint_chrome(scene: &mut Scene, ctx: &mut ChromePaintContext<'_>) {
    let transform = ctx.transform;

    // ── Bar background ────────────────────────────────────────────────────────
    let bar_rect = Rect::new(0.0, 0.0, ctx.window_width as f64, CHROME_HEIGHT as f64);
    scene.fill(
        Fill::NonZero,
        transform,
        ctx.palette.bar_bg,
        None,
        &bar_rect,
    );

    // ── Tab strip ─────────────────────────────────────────────────────────────
    let strip_rect = Rect::new(0.0, 0.0, ctx.window_width as f64, TAB_STRIP_HEIGHT as f64);
    scene.fill(
        Fill::NonZero,
        transform,
        ctx.palette.strip_bg,
        None,
        &strip_rect,
    );
    let layout = ChromeLayout {
        window_width: ctx.window_width,
        tab_count: ctx.tabs.len(),
        dropdown_count: if ctx.state.focused {
            ctx.state.suggestions.len()
        } else {
            0
        },
    };
    for (i, rect) in tab_rects(&layout) {
        let Some(entry) = ctx.tabs.get(i) else {
            continue;
        };
        let bg = if entry.active {
            ctx.palette.tab_active_bg
        } else {
            ctx.palette.tab_inactive_bg
        };
        scene.fill(Fill::NonZero, transform, bg, None, &rect);

        let text_color = if entry.active {
            ctx.palette.tab_text
        } else {
            ctx.palette.tab_text_inactive
        };
        let avail = (rect.width() as f32) - TAB_TEXT_PAD * 2.0 - TAB_CLOSE_W;
        let title = elide_to_width(&entry.title, avail, ctx.font_cx, ctx.layout_cx);
        let title_layout = build_chrome_text_layout(&title, ctx.font_cx, ctx.layout_cx);
        draw_text_layout(
            scene,
            &title_layout,
            rect.x0 as f32 + TAB_TEXT_PAD,
            (TAB_STRIP_HEIGHT - title_layout.height()) / 2.0,
            text_color,
            transform,
        );

        let close_layout = build_chrome_text_layout("×", ctx.font_cx, ctx.layout_cx);
        draw_text_layout(
            scene,
            &close_layout,
            rect.x1 as f32 - TAB_CLOSE_W + (TAB_CLOSE_W - close_layout.width()) / 2.0,
            (TAB_STRIP_HEIGHT - close_layout.height()) / 2.0,
            ctx.palette.tab_close_glyph,
            transform,
        );
    }
    let plus_rect = new_tab_rect(&layout);
    let plus_layout = build_chrome_text_layout("+", ctx.font_cx, ctx.layout_cx);
    draw_text_layout(
        scene,
        &plus_layout,
        plus_rect.x0 as f32 + (NEW_TAB_W - plus_layout.width()) / 2.0,
        (TAB_STRIP_HEIGHT - plus_layout.height()) / 2.0,
        ctx.palette.tab_text,
        transform,
    );

    // ── History sidebar toggle ────────────────────────────────────────────────
    paint_toolbar_button(
        scene,
        HISTORY_X,
        ButtonContent::Icon(get_icon_history()),
        if ctx.history_sidebar_open {
            ctx.palette.tab_active_bg
        } else {
            ctx.palette.btn_bg
        },
        ctx.palette.btn_text,
        ctx.history_sidebar_open,
        &mut NavButtonContext {
            palette: ctx.palette,
            font_cx: ctx.font_cx,
            layout_cx: ctx.layout_cx,
            transform,
        },
    );

    // ── Back button (dimmed + inert when the back stack is empty) ────────────
    paint_nav_button(
        scene,
        BACK_X,
        ButtonContent::Icon(get_icon_back()),
        ctx.can_go_back,
        &mut NavButtonContext {
            palette: ctx.palette,
            font_cx: ctx.font_cx,
            layout_cx: ctx.layout_cx,
            transform,
        },
    );

    // ── Reload button ─────────────────────────────────────────────────────────
    paint_toolbar_button(
        scene,
        RELOAD_X,
        ButtonContent::Icon(get_icon_reload()),
        ctx.palette.btn_bg,
        ctx.palette.btn_text,
        false,
        &mut NavButtonContext {
            palette: ctx.palette,
            font_cx: ctx.font_cx,
            layout_cx: ctx.layout_cx,
            transform,
        },
    );

    // ── Forward button (dimmed + inert when the forward stack is empty) ─────
    paint_nav_button(
        scene,
        FORWARD_X,
        ButtonContent::Icon(get_icon_forward()),
        ctx.can_go_forward,
        &mut NavButtonContext {
            palette: ctx.palette,
            font_cx: ctx.font_cx,
            layout_cx: ctx.layout_cx,
            transform,
        },
    );

    // ── URL bar ───────────────────────────────────────────────────────────────
    let url_bar_right = (ctx.window_width - STATUS_W).max(URL_BAR_X + 10.0);
    let url_bar_rect = RoundedRect::new(
        URL_BAR_X as f64,
        URL_BAR_Y as f64,
        url_bar_right as f64,
        (URL_BAR_Y + URL_BAR_H) as f64,
        4.0,
    );
    scene.fill(
        Fill::NonZero,
        transform,
        ctx.palette.url_bg,
        None,
        &url_bar_rect,
    );

    // Border (thicker / brighter when focused)
    let border_color = if ctx.state.focused {
        ctx.palette.url_border_focused
    } else {
        ctx.palette.url_border_idle
    };
    let border_stroke = Stroke::new(1.0);
    scene.stroke(&border_stroke, transform, border_color, None, &url_bar_rect);

    // Clip content to URL bar interior
    let clip_rect = Rect::new(
        (URL_BAR_X + URL_TEXT_PAD) as f64,
        URL_BAR_Y as f64,
        (url_bar_right - URL_TEXT_PAD) as f64,
        (URL_BAR_Y + URL_BAR_H) as f64,
    );
    scene.push_layer(
        BlendMode::new(Mix::Normal, Compose::SrcOver),
        1.0,
        transform,
        &clip_rect,
    );

    let text_left = URL_BAR_X + URL_TEXT_PAD;
    let text_top = URL_BAR_Y + (URL_BAR_H - CHROME_FONT_SIZE) / 2.0 - 1.0;

    // Selection highlight
    if ctx.state.focused
        && let Some((lo, hi)) = ctx.state.selection_range()
        && lo < hi
    {
        let x0 = url_cursor_x(&ctx.state.url, lo, ctx.font_cx, ctx.layout_cx);
        let x1 = url_cursor_x(&ctx.state.url, hi, ctx.font_cx, ctx.layout_cx);
        let sel_rect = Rect::new(
            (text_left + x0) as f64,
            URL_BAR_Y as f64,
            (text_left + x1) as f64,
            (URL_BAR_Y + URL_BAR_H) as f64,
        );
        scene.fill(
            Fill::NonZero,
            transform,
            ctx.palette.select,
            None,
            &sel_rect,
        );
    }

    // URL text
    let mut url_layout_width = 0.0;
    if !ctx.state.url.is_empty() {
        let url_layout = build_chrome_text_layout(&ctx.state.url, ctx.font_cx, ctx.layout_cx);
        url_layout_width = url_layout.width();
        draw_text_layout(
            scene,
            &url_layout,
            text_left,
            text_top,
            ctx.palette.url_text,
            transform,
        );
    }

    if let Some(inline) = &ctx.state.inline_completion {
        let inline_layout = build_chrome_text_layout(inline, ctx.font_cx, ctx.layout_cx);
        draw_text_layout(
            scene,
            &inline_layout,
            text_left + url_layout_width,
            text_top,
            ctx.palette.url_text.with_alpha_factor(0.4),
            transform,
        );
    }

    // Cursor (blinking via elapsed_ms)
    if ctx.state.focused && ctx.elapsed_ms % 1000 < 500 {
        let cx = url_cursor_x(&ctx.state.url, ctx.state.cursor, ctx.font_cx, ctx.layout_cx);
        let cursor_x = text_left + cx;
        let cursor_rect = Rect::new(
            cursor_x as f64,
            (URL_BAR_Y + 3.0) as f64,
            (cursor_x + 1.5) as f64,
            (URL_BAR_Y + URL_BAR_H - 3.0) as f64,
        );
        scene.fill(
            Fill::NonZero,
            transform,
            ctx.palette.cursor,
            None,
            &cursor_rect,
        );
    }

    scene.pop_layer(); // end URL bar clip

    // ── Autocomplete Dropdown ─────────────────────────────────────────────────
    if ctx.state.focused && !ctx.state.suggestions.is_empty() {
        let item_h = 24.0;
        let padding = 4.0;
        let dropdown_h = (ctx.state.suggestions.len() as f64) * item_h + padding * 2.0;
        let dropdown_rect = RoundedRect::new(
            URL_BAR_X as f64,
            (URL_BAR_Y + URL_BAR_H) as f64,
            url_bar_right as f64,
            (URL_BAR_Y + URL_BAR_H) as f64 + dropdown_h,
            4.0,
        );

        scene.fill(
            Fill::NonZero,
            transform,
            ctx.palette.bar_bg,
            None,
            &dropdown_rect,
        );
        scene.stroke(
            &Stroke::new(1.0),
            transform,
            ctx.palette.url_border_idle,
            None,
            &dropdown_rect,
        );

        for (i, suggestion) in ctx.state.suggestions.iter().enumerate() {
            let item_y = (URL_BAR_Y + URL_BAR_H) as f64 + padding + (i as f64) * item_h;
            let item_rect = Rect::new(
                URL_BAR_X as f64,
                item_y,
                url_bar_right as f64,
                item_y + item_h,
            );

            if Some(i) == ctx.state.selected_suggestion {
                scene.fill(
                    Fill::NonZero,
                    transform,
                    ctx.palette.select,
                    None,
                    &item_rect,
                );
            } else if Some(i) == ctx.state.hovered_suggestion {
                scene.fill(
                    Fill::NonZero,
                    transform,
                    ctx.palette.btn_bg,
                    None,
                    &item_rect,
                );
            }

            let title_str = if suggestion.title.is_empty() {
                suggestion.url.clone()
            } else {
                format!("{} - {}", suggestion.title, suggestion.url)
            };

            let elided = elide_to_width(
                &title_str,
                (url_bar_right - URL_BAR_X - 16.0) as f32,
                ctx.font_cx,
                ctx.layout_cx,
            );
            let elided_layout = build_chrome_text_layout(&elided, ctx.font_cx, ctx.layout_cx);
            draw_text_layout(
                scene,
                &elided_layout,
                URL_BAR_X + 8.0,
                (item_y as f32) + (item_h as f32 - elided_layout.height()) / 2.0,
                ctx.palette.url_text,
                transform,
            );
        }
    }

    // ── Status indicator ──────────────────────────────────────────────────────
    let indicator_cx = ctx.window_width - STATUS_W / 2.0;
    let indicator_cy = CHROME_HEIGHT / 2.0;

    if ctx.state.loading {
        // Three pulsing dots
        let active_dot = ((ctx.elapsed_ms / 300) % 3) as usize;
        for i in 0..3 {
            let dot_x = indicator_cx - 12.0 + (i as f32) * 12.0;
            let alpha = if i == active_dot { 255u8 } else { 80u8 };
            let dot_color = Color::rgba8(120, 170, 255, alpha);
            let dot = Circle::new((dot_x as f64, indicator_cy as f64), 3.5);
            scene.fill(Fill::NonZero, transform, dot_color, None, &dot);
        }
        // Request continuous redraw (caller checks chrome_state.loading)
    } else {
        let ok_dot = Circle::new((indicator_cx as f64, indicator_cy as f64), 5.0);
        scene.fill(Fill::NonZero, transform, ctx.palette.ok_dot, None, &ok_dot);
    }
}
