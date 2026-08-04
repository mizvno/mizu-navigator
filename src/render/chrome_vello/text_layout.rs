//! Text layout helper shared by every text-painting call site in the chrome
//! bar (tab titles, URL bar, autocomplete dropdown).

use super::*;

// ── Text layout helper ────────────────────────────────────────────────────────

pub(super) fn build_chrome_text_layout(
    text: &str,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
) -> parley::Layout<vello::peniko::Color> {
    // Chrome text is embedded-only (see `render::embedded_fonts`): the
    // generic `Monospace` family always resolves to IBM Plex Mono, so a
    // named-font fallback chain probing for OS-installed monospace faces
    // (Consolas/Cascadia Code/Courier New) would never match anything —
    // there is no system font backend to match against.
    let font_family =
        FontFamily::Single(FontFamilyName::Generic(GenericFamily::Monospace));
    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::FontFamily(font_family));
    builder.push_default(StyleProperty::FontSize(CHROME_FONT_SIZE));
    // Placeholder brush: this layout is used both for measurement
    // (`url_cursor_x`/`url_cursor_from_x`, never painted) and for painting
    // via `draw_text_layout`, which always applies its own explicit `color`
    // argument at draw time — so the actual on-screen color is never this
    // one, and it doesn't need to be theme-aware.
    builder.push_default(StyleProperty::Brush(Color::rgba8(204, 204, 204, 255)));
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(1.0)));
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    layout
}

/// Renders glyph runs from a Parley layout at the given logical (x, y) position.
pub(super) fn draw_text_layout(
    scene: &mut Scene,
    layout: &parley::Layout<vello::peniko::Color>,
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
                let peniko_blob = vello::peniko::Blob::from_raw_parts(arc_data, id);
                let vello_font = vello::peniko::Font::new(peniko_blob, font_data.index);
                let glyphs = run.positioned_glyphs().map(|g| vello::glyph::Glyph {
                    id: g.id,
                    x: g.x,
                    y: g.y,
                });
                scene
                    .draw_glyphs(&vello_font)
                    .font_size(CHROME_FONT_SIZE)
                    .brush(color)
                    .transform(transform * Affine::translate((x as f64, (y + y_offset) as f64)))
                    .draw(Fill::NonZero, glyphs);
            }
        }
    }
}

use std::sync::OnceLock;

pub(super) fn parse_svg_path(svg: &str) -> BezPath {
    let start_idx = svg.find("d=\"").expect("Valid SVG path") + 3;
    let end_idx = svg[start_idx..].find("\"").expect("Valid SVG path") + start_idx;
    let path_data = &svg[start_idx..end_idx];
    BezPath::from_svg(path_data).expect("Valid SVG path data")
}

pub(super) fn get_icon_back() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| parse_svg_path(include_str!("../../../assets/icons/arrow-left-bold.svg")))
}

pub(super) fn get_icon_forward() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| parse_svg_path(include_str!("../../../assets/icons/arrow-right-bold.svg")))
}

pub(super) fn get_icon_reload() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| {
        parse_svg_path(include_str!(
            "../../../assets/icons/arrow-clockwise-bold.svg"
        ))
    })
}

pub(super) fn get_icon_history() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| {
        parse_svg_path(include_str!(
            "../../../assets/icons/clock-counter-clockwise-bold.svg"
        ))
    })
}

pub(super) enum ButtonContent<'a> {
    #[allow(dead_code)]
    Text(&'a str),
    Icon(&'a BezPath),
}

/// Context for [`paint_nav_button`]: everything shared across both the
/// Back and Forward button paint calls in a single `paint_chrome` pass.
pub(super) struct NavButtonContext<'a> {
    pub(super) palette: &'a ChromePalette,
    pub(super) font_cx: &'a mut parley::FontContext,
    pub(super) layout_cx: &'a mut parley::LayoutContext<vello::peniko::Color>,
    pub(super) transform: Affine,
}

/// Paints a single square nav button (Back/Forward) at logical X `x`
/// containing the centered glyph `label`. When `enabled` is `false`, both the
/// button background and glyph render at reduced alpha — the dimmed
/// affordance signals the button is inert (empty back/forward stack); the
/// caller is responsible for actually ignoring clicks on it (`window::history`
/// already makes a Back/Forward step a no-op when its stack is empty, so
/// this dimming is purely visual confirmation, not the enforcement point).
pub(super) fn paint_nav_button(
    scene: &mut Scene,
    x: f32,
    content: ButtonContent<'_>,
    enabled: bool,
    ctx: &mut NavButtonContext<'_>,
) {
    let (bg, text_color) = if enabled {
        (ctx.palette.btn_bg, ctx.palette.btn_text)
    } else {
        (ctx.palette.btn_bg_disabled, ctx.palette.btn_text_disabled)
    };
    paint_toolbar_button(scene, x, content, bg, text_color, false, ctx);
}

/// Paints one `BTN_W × BTN_H` toolbar button at `x` with an explicit
/// background and glyph color.
pub(super) fn paint_toolbar_button(
    scene: &mut Scene,
    x: f32,
    content: ButtonContent<'_>,
    bg: Color,
    text_color: Color,
    active: bool,
    ctx: &mut NavButtonContext<'_>,
) {
    let rect = RoundedRect::new(
        x as f64,
        BTN_Y as f64,
        (x + BTN_W) as f64,
        (BTN_Y + BTN_H) as f64,
        3.0,
    );
    scene.fill(Fill::NonZero, ctx.transform, bg, None, &rect);
    if active {
        let underline = Rect::new(
            (x + 4.0) as f64,
            (BTN_Y + BTN_H - 2.0) as f64,
            (x + BTN_W - 4.0) as f64,
            (BTN_Y + BTN_H) as f64,
        );
        scene.fill(
            Fill::NonZero,
            ctx.transform,
            ctx.palette.url_border_focused,
            None,
            &underline,
        );
    }

    match content {
        ButtonContent::Text(label) => {
            let layout = build_chrome_text_layout(label, ctx.font_cx, ctx.layout_cx);
            let text_x = x + (BTN_W - layout.width()) / 2.0;
            let text_y = BTN_Y + (BTN_H - layout.height()) / 2.0;
            draw_text_layout(scene, &layout, text_x, text_y, text_color, ctx.transform);
        }
        ButtonContent::Icon(path) => {
            let icon_size = 14.0;
            let scale = icon_size / 256.0;
            let icon_x = x + (BTN_W - icon_size) / 2.0;
            let icon_y = BTN_Y + (BTN_H - icon_size) / 2.0;
            let icon_transform = ctx.transform
                * Affine::translate((icon_x as f64, icon_y as f64))
                * Affine::scale(scale as f64);
            scene.fill(Fill::NonZero, icon_transform, text_color, None, path);
        }
    }
}
