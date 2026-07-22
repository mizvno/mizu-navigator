//! Native Vello-based browser chrome rendering.
//!
//! This module replaces the former egui-based `chrome.rs`. It renders the
//! URL bar, navigation buttons, and loading indicator directly into the Vello
//! [`Scene`] without any egui intermediate pass.
//!
//! All layout coordinates are **logical pixels**. The caller is responsible for
//! applying `Affine::scale(dpi_scale)` so that everything scales correctly on
//! high-DPI displays.

#![forbid(unsafe_code)]

use parley::style::{FontFamily, FontFamilyName, GenericFamily, LineHeight, StyleProperty};
use vello::{
    Scene,
    kurbo::{Affine, Circle, Rect, RoundedRect, Stroke},
    peniko::{BlendMode, Color, Compose, Fill, Mix},
};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::render::preferences::ChromePalette;

// ── Geometry ─────────────────────────────────────────────────────────────────

/// Height of the chrome bar in logical pixels.
pub const CHROME_HEIGHT: f32 = 28.0;

const BTN_Y: f32 = 4.0;
const BTN_H: f32 = 20.0;
const BTN_W: f32 = 24.0;
const BACK_X: f32 = 4.0;
const RELOAD_X: f32 = 32.0;
const FORWARD_X: f32 = 60.0;
const URL_BAR_X: f32 = 88.0;
const URL_BAR_Y: f32 = 3.0;
const URL_BAR_H: f32 = 22.0;
/// Width reserved for the status indicator on the right.
const STATUS_W: f32 = 40.0;
/// Text padding inside the URL bar.
const URL_TEXT_PAD: f32 = 4.0;
/// Font size used throughout the chrome bar.
const CHROME_FONT_SIZE: f32 = 12.0;

// ── Palette ───────────────────────────────────────────────────────────────────
//
// The chrome's colors are no longer fixed constants (ux-5): they come from a
// `ChromePalette` chosen by `render::preferences::ChromePalette::for_preferences`
// from the caller's detected `UserPreferences` (light/dark, forced on
// high-contrast), passed into `paint_chrome` on every frame.

// ── Public types ─────────────────────────────────────────────────────────────

/// The state for the browser chrome UI element.
#[derive(Debug, Default)]
pub struct ChromeState {
    /// Current text in the URL bar.
    pub url: String,
    /// Cursor position as a **byte offset** into `url`.
    pub cursor: usize,
    /// Active selection as `(start, end)` byte offsets. `start` may be ≥ `end`
    /// (selection created by moving left). Use [`ChromeState::selection_range`]
    /// to get the normalised `(lo, hi)` range.
    pub selection: Option<(usize, usize)>,
    /// Whether the URL bar currently has keyboard focus.
    pub focused: bool,
    /// Whether the browser is loading a page.
    pub loading: bool,
}

/// A zone within the chrome bar hit by a mouse click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeHitZone {
    /// The "go back" button.
    BackButton,
    /// The "reload" button.
    ReloadButton,
    /// The "go forward" button.
    ForwardButton,
    /// The URL text input area.
    UrlBar,
    /// Any other part of the chrome bar (background).
    Background,
}

/// An action requested by a chrome keyboard event.
#[derive(Debug)]
pub enum ChromeKeyAction {
    /// Navigate to the given URL.
    Navigate(String),
    /// Trigger a page reload.
    Reload,
    /// Go back one entry in session history.
    Back,
    /// Copy the selected text to the clipboard (caller handles clipboard).
    Copy,
    /// Cut the selected text to the clipboard (caller handles clipboard).
    Cut,
    /// Paste from clipboard at the cursor (caller provides text via `paste_text`).
    Paste,
    /// Key was handled; redraw needed but no further action required.
    Handled,
    /// Key was not consumed by the chrome.
    Ignored,
}

// ── ChromeState implementation ────────────────────────────────────────────────

impl ChromeState {
    // ── Cursor helpers ────────────────────────────────────────────────────────

    /// Returns the normalised `(lo, hi)` selection range, or `None`.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
    }

    /// Returns the selected text, or `None` if there is no selection.
    pub fn selected_text(&self) -> Option<&str> {
        let (lo, hi) = self.selection_range()?;
        if lo == hi {
            return None;
        }
        self.url.get(lo..hi)
    }

    /// Moves the cursor to the previous char boundary.
    fn prev_char_boundary(&self, from: usize) -> usize {
        if from == 0 {
            return 0;
        }
        let mut o = from - 1;
        while o > 0 && !self.url.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    /// Moves the cursor to the next char boundary.
    fn next_char_boundary(&self, from: usize) -> usize {
        let len = self.url.len();
        if from >= len {
            return len;
        }
        let mut o = from + 1;
        while o < len && !self.url.is_char_boundary(o) {
            o += 1;
        }
        o
    }

    // ── Text mutation ─────────────────────────────────────────────────────────

    /// Inserts `text` at the cursor, replacing any active selection.
    ///
    /// The single choke point for anything entering the URL bar's text
    /// (typed characters and paste both call this) — bidi
    /// override/embedding/isolate control characters are stripped here
    /// (ux-7 anti-spoofing policy, `docs/design/bidi.md` §4) so neither
    /// path can plant one, and so cursor byte-offset math never has to
    /// reconcile a stripped display string against an unstripped buffer.
    pub fn insert_text(&mut self, text: &str) {
        let text = crate::render::bidi::strip_bidi_overrides(text);
        // Delete selection first if any
        if let Some((lo, hi)) = self.selection_range()
            && lo < hi
        {
            self.url.replace_range(lo..hi, "");
            self.cursor = lo;
            self.selection = None;
        }
        let cursor = self.cursor.min(self.url.len());
        self.url.insert_str(cursor, &text);
        self.cursor = cursor + text.len();
        self.selection = None;
    }

    /// Deletes the selection, or the character before the cursor (Backspace).
    pub fn delete_backward(&mut self) {
        if let Some((lo, hi)) = self.selection_range()
            && lo < hi
        {
            self.url.replace_range(lo..hi, "");
            self.cursor = lo;
            self.selection = None;
            return;
        }
        self.selection = None;
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_char_boundary(self.cursor);
        self.url.remove(prev);
        self.cursor = prev;
    }

    /// Deletes the selection, or the character after the cursor (Delete key).
    pub fn delete_forward(&mut self) {
        if let Some((lo, hi)) = self.selection_range()
            && lo < hi
        {
            self.url.replace_range(lo..hi, "");
            self.cursor = lo;
            self.selection = None;
            return;
        }
        self.selection = None;
        let len = self.url.len();
        if self.cursor >= len {
            return;
        }
        let next = self.next_char_boundary(self.cursor);
        self.url.replace_range(self.cursor..next, "");
    }

    /// Moves the cursor one character to the left.
    /// If `extend` is true, extends the selection instead of collapsing it.
    pub fn move_left(&mut self, extend: bool) {
        if extend {
            let anchor = match self.selection {
                Some((a, _)) => a,
                None => self.cursor,
            };
            let new_pos = self.prev_char_boundary(self.cursor);
            self.cursor = new_pos;
            self.selection = Some((anchor, new_pos));
        } else if let Some((lo, hi)) = self.selection_range() {
            // Collapse to left of selection
            self.cursor = lo;
            self.selection = None;
            let _ = hi; // suppress unused warning
        } else {
            self.cursor = self.prev_char_boundary(self.cursor);
            self.selection = None;
        }
    }

    /// Moves the cursor one character to the right.
    /// If `extend` is true, extends the selection instead of collapsing it.
    pub fn move_right(&mut self, extend: bool) {
        if extend {
            let anchor = match self.selection {
                Some((a, _)) => a,
                None => self.cursor,
            };
            let new_pos = self.next_char_boundary(self.cursor);
            self.cursor = new_pos;
            self.selection = Some((anchor, new_pos));
        } else if let Some((lo, hi)) = self.selection_range() {
            // Collapse to right of selection
            self.cursor = hi;
            self.selection = None;
            let _ = lo;
        } else {
            self.cursor = self.next_char_boundary(self.cursor);
            self.selection = None;
        }
    }

    /// Moves cursor to the start. If `extend`, extends selection.
    pub fn move_to_start(&mut self, extend: bool) {
        if extend {
            let anchor = self.selection.map(|(a, _)| a).unwrap_or(self.cursor);
            self.cursor = 0;
            self.selection = Some((anchor, 0));
        } else {
            self.cursor = 0;
            self.selection = None;
        }
    }

    /// Moves cursor to the end. If `extend`, extends selection.
    pub fn move_to_end(&mut self, extend: bool) {
        let end = self.url.len();
        if extend {
            let anchor = self.selection.map(|(a, _)| a).unwrap_or(self.cursor);
            self.cursor = end;
            self.selection = Some((anchor, end));
        } else {
            self.cursor = end;
            self.selection = None;
        }
    }

    /// Selects all text in the URL bar.
    pub fn select_all(&mut self) {
        let len = self.url.len();
        self.selection = Some((0, len));
        self.cursor = len;
    }

    // ── Clipboard helpers ─────────────────────────────────────────────────────

    /// Returns the selected text as a `String` (for copy to clipboard).
    pub fn copy_text(&self) -> Option<String> {
        self.selected_text().map(str::to_string)
    }

    /// Deletes the selection, returning the deleted text (for cut to clipboard).
    pub fn cut_text(&mut self) -> Option<String> {
        let text = self.copy_text()?;
        self.delete_backward(); // delete_backward removes selection if any
        Some(text)
    }

    /// Pastes `text` at the cursor (replaces any active selection).
    pub fn paste_text(&mut self, text: &str) {
        self.insert_text(text);
    }

    // ── Mouse handling ────────────────────────────────────────────────────────

    /// Positions the cursor at the URL-bar logical X coordinate `click_x`.
    /// `bar_left_x` is the logical X of the left edge of the text area inside the bar.
    pub fn set_cursor_from_click(
        &mut self,
        click_x: f32,
        bar_left_x: f32,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
    ) {
        let offset = url_cursor_from_x(&self.url, click_x - bar_left_x, font_cx, layout_cx);
        self.cursor = offset;
        self.selection = None;
    }

    /// Extends the selection to the URL-bar logical X coordinate `x`.
    pub fn extend_selection_to_x(
        &mut self,
        x: f32,
        bar_left_x: f32,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
    ) {
        let new_end = url_cursor_from_x(&self.url, x - bar_left_x, font_cx, layout_cx);
        let anchor = self.selection.map(|(a, _)| a).unwrap_or(self.cursor);
        self.cursor = new_end;
        self.selection = Some((anchor, new_end));
    }

    // ── Keyboard handling ─────────────────────────────────────────────────────

    /// Process a keyboard event when the URL bar is focused.
    ///
    /// Returns a [`ChromeKeyAction`] describing what the caller should do next.
    /// `text` is `key_event.text.as_deref()` from winit.
    pub fn handle_key(
        &mut self,
        key: &Key,
        text: Option<&str>,
        mods: ModifiersState,
    ) -> ChromeKeyAction {
        let ctrl = mods.control_key();
        let shift = mods.shift_key();

        match key {
            Key::Named(NamedKey::Enter) => {
                // Normalise URL: add schema if missing
                let mut url = self.url.trim().to_string();
                if !url.is_empty() && !url.contains("://") {
                    url = format!("mizu://{url}");
                    self.url = url.clone();
                    self.cursor = self.url.len();
                }
                self.selection = None;
                self.focused = false;
                ChromeKeyAction::Navigate(url)
            }
            Key::Named(NamedKey::Escape) => {
                self.selection = None;
                self.focused = false;
                ChromeKeyAction::Handled
            }
            Key::Named(NamedKey::Backspace) => {
                self.delete_backward();
                ChromeKeyAction::Handled
            }
            Key::Named(NamedKey::Delete) => {
                self.delete_forward();
                ChromeKeyAction::Handled
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.move_left(shift);
                ChromeKeyAction::Handled
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.move_right(shift);
                ChromeKeyAction::Handled
            }
            Key::Named(NamedKey::Home) => {
                self.move_to_start(shift);
                ChromeKeyAction::Handled
            }
            Key::Named(NamedKey::End) => {
                self.move_to_end(shift);
                ChromeKeyAction::Handled
            }
            Key::Character(ch) if ctrl => match ch.as_str() {
                "a" | "A" => {
                    self.select_all();
                    ChromeKeyAction::Handled
                }
                "c" | "C" => ChromeKeyAction::Copy,
                "x" | "X" => ChromeKeyAction::Cut,
                "v" | "V" => ChromeKeyAction::Paste,
                _ => ChromeKeyAction::Ignored,
            },
            _ => {
                // Printable character
                if let Some(t) = text {
                    let chars: String = t.chars().filter(|c| !c.is_control()).collect();
                    if !chars.is_empty() {
                        self.insert_text(&chars);
                        return ChromeKeyAction::Handled;
                    }
                }
                ChromeKeyAction::Ignored
            }
        }
    }
}

// ── Hit testing ───────────────────────────────────────────────────────────────

/// Returns the [`ChromeHitZone`] for a logical (x, y) coordinate.
/// Returns [`ChromeHitZone::Background`] if the point is outside the chrome area
/// (y ≥ CHROME_HEIGHT) or in an unoccupied region.
pub fn chrome_hit_zone(x: f32, y: f32, window_width: f32) -> ChromeHitZone {
    if !(0.0..CHROME_HEIGHT).contains(&y) {
        return ChromeHitZone::Background;
    }
    if (BACK_X..BACK_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::BackButton;
    }
    if (RELOAD_X..RELOAD_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::ReloadButton;
    }
    if (FORWARD_X..FORWARD_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::ForwardButton;
    }
    let url_bar_right = (window_width - STATUS_W).max(URL_BAR_X + 10.0);
    if x >= URL_BAR_X && x < url_bar_right && (URL_BAR_Y..URL_BAR_Y + URL_BAR_H).contains(&y) {
        return ChromeHitZone::UrlBar;
    }
    ChromeHitZone::Background
}

/// Returns the logical X left edge of the URL text area (inside the bar padding).
pub fn url_text_left(window_width: f32) -> f32 {
    let _ = window_width;
    URL_BAR_X + URL_TEXT_PAD
}

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

// ── Text layout helper ────────────────────────────────────────────────────────

fn build_chrome_text_layout(
    text: &str,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
) -> parley::Layout<vello::peniko::Color> {
    let fallbacks = vec![
        FontFamilyName::named("Consolas"),
        FontFamilyName::named("Cascadia Code"),
        FontFamilyName::named("Courier New"),
        FontFamilyName::Generic(GenericFamily::Monospace),
        FontFamilyName::Generic(GenericFamily::SansSerif),
    ];
    let font_family = FontFamily::List(std::borrow::Cow::Owned(fallbacks));
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
fn draw_text_layout(
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

/// Paints a single square nav button (Back/Forward) at logical X `x`
/// containing the centered glyph `label`. When `enabled` is `false`, both the
/// button background and glyph render at reduced alpha — the dimmed
/// affordance signals the button is inert (empty back/forward stack); the
/// caller is responsible for actually ignoring clicks on it (`window::history`
/// already makes a Back/Forward step a no-op when its stack is empty, so
/// this dimming is purely visual confirmation, not the enforcement point).
#[allow(clippy::too_many_arguments)]
fn paint_nav_button(
    scene: &mut Scene,
    x: f32,
    label: &str,
    enabled: bool,
    palette: &ChromePalette,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
    transform: Affine,
) {
    let (bg, text_color) = if enabled {
        (palette.btn_bg, palette.btn_text)
    } else {
        (palette.btn_bg_disabled, palette.btn_text_disabled)
    };
    let rect = RoundedRect::new(
        x as f64,
        BTN_Y as f64,
        (x + BTN_W) as f64,
        (BTN_Y + BTN_H) as f64,
        3.0,
    );
    scene.fill(Fill::NonZero, transform, bg, None, &rect);
    let layout = build_chrome_text_layout(label, font_cx, layout_cx);
    let text_x = x + (BTN_W - layout.width()) / 2.0;
    let text_y = BTN_Y + (BTN_H - layout.height()) / 2.0;
    draw_text_layout(scene, &layout, text_x, text_y, text_color, transform);
}

// ── Main paint function ───────────────────────────────────────────────────────

/// Renders the browser chrome bar into `scene`.
///
/// All coordinates are **logical pixels**. `transform` should be
/// `Affine::scale(dpi_scale)` so the chrome scales on high-DPI displays.
#[allow(clippy::too_many_arguments)]
pub fn paint_chrome(
    scene: &mut Scene,
    state: &ChromeState,
    window_width: f32,
    transform: Affine,
    elapsed_ms: u64,
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<vello::peniko::Color>,
    can_go_back: bool,
    can_go_forward: bool,
    palette: &ChromePalette,
) {
    // ── Bar background ────────────────────────────────────────────────────────
    let bar_rect = Rect::new(0.0, 0.0, window_width as f64, CHROME_HEIGHT as f64);
    scene.fill(Fill::NonZero, transform, palette.bar_bg, None, &bar_rect);

    // ── Back button (dimmed + inert when the back stack is empty) ────────────
    paint_nav_button(
        scene, BACK_X, "←", can_go_back, palette, font_cx, layout_cx, transform,
    );

    // ── Reload button ─────────────────────────────────────────────────────────
    let reload_rect = RoundedRect::new(
        RELOAD_X as f64,
        BTN_Y as f64,
        (RELOAD_X + BTN_W) as f64,
        (BTN_Y + BTN_H) as f64,
        3.0,
    );
    scene.fill(Fill::NonZero, transform, palette.btn_bg, None, &reload_rect);
    let reload_layout = build_chrome_text_layout("↻", font_cx, layout_cx);
    let btn2_text_x = RELOAD_X + (BTN_W - reload_layout.width()) / 2.0;
    let btn2_text_y = BTN_Y + (BTN_H - reload_layout.height()) / 2.0;
    draw_text_layout(
        scene,
        &reload_layout,
        btn2_text_x,
        btn2_text_y,
        palette.btn_text,
        transform,
    );

    // ── Forward button (dimmed + inert when the forward stack is empty) ─────
    paint_nav_button(
        scene,
        FORWARD_X,
        "→",
        can_go_forward,
        palette,
        font_cx,
        layout_cx,
        transform,
    );

    // ── URL bar ───────────────────────────────────────────────────────────────
    let url_bar_right = (window_width - STATUS_W).max(URL_BAR_X + 10.0);
    let url_bar_rect = RoundedRect::new(
        URL_BAR_X as f64,
        URL_BAR_Y as f64,
        url_bar_right as f64,
        (URL_BAR_Y + URL_BAR_H) as f64,
        4.0,
    );
    scene.fill(Fill::NonZero, transform, palette.url_bg, None, &url_bar_rect);

    // Border (thicker / brighter when focused)
    let border_color = if state.focused {
        palette.url_border_focused
    } else {
        palette.url_border_idle
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
    if state.focused
        && let Some((lo, hi)) = state.selection_range()
        && lo < hi
    {
        let x0 = url_cursor_x(&state.url, lo, font_cx, layout_cx);
        let x1 = url_cursor_x(&state.url, hi, font_cx, layout_cx);
        let sel_rect = Rect::new(
            (text_left + x0) as f64,
            URL_BAR_Y as f64,
            (text_left + x1) as f64,
            (URL_BAR_Y + URL_BAR_H) as f64,
        );
        scene.fill(Fill::NonZero, transform, palette.select, None, &sel_rect);
    }

    // URL text
    if !state.url.is_empty() {
        let url_layout = build_chrome_text_layout(&state.url, font_cx, layout_cx);
        draw_text_layout(
            scene,
            &url_layout,
            text_left,
            text_top,
            palette.url_text,
            transform,
        );
    }

    // Cursor (blinking via elapsed_ms)
    if state.focused && elapsed_ms % 1000 < 500 {
        let cx = url_cursor_x(&state.url, state.cursor, font_cx, layout_cx);
        let cursor_x = text_left + cx;
        let cursor_rect = Rect::new(
            cursor_x as f64,
            (URL_BAR_Y + 3.0) as f64,
            (cursor_x + 1.5) as f64,
            (URL_BAR_Y + URL_BAR_H - 3.0) as f64,
        );
        scene.fill(Fill::NonZero, transform, palette.cursor, None, &cursor_rect);
    }

    scene.pop_layer(); // end URL bar clip

    // ── Status indicator ──────────────────────────────────────────────────────
    let indicator_cx = window_width - STATUS_W / 2.0;
    let indicator_cy = CHROME_HEIGHT / 2.0;

    if state.loading {
        // Three pulsing dots
        let active_dot = ((elapsed_ms / 300) % 3) as usize;
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
        scene.fill(Fill::NonZero, transform, palette.ok_dot, None, &ok_dot);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(url: &str) -> ChromeState {
        ChromeState {
            url: url.to_string(),
            cursor: url.len(),
            ..Default::default()
        }
    }

    #[test]
    fn handle_key_enter_adds_schema() {
        let mut s = make_state("example.com");
        let action = s.handle_key(&Key::Named(NamedKey::Enter), None, ModifiersState::empty());
        match action {
            ChromeKeyAction::Navigate(url) => assert_eq!(url, "mizu://example.com"),
            other => panic!("expected Navigate, got {:?}", other),
        }
    }

    #[test]
    fn handle_key_enter_preserves_existing_schema() {
        let mut s = make_state("https://example.com");
        let action = s.handle_key(&Key::Named(NamedKey::Enter), None, ModifiersState::empty());
        match action {
            ChromeKeyAction::Navigate(url) => assert_eq!(url, "https://example.com"),
            other => panic!("expected Navigate, got {:?}", other),
        }
    }

    #[test]
    fn insert_text_advances_cursor() {
        let mut s = make_state("ab");
        s.cursor = 1; // between 'a' and 'b'
        s.insert_text("X");
        assert_eq!(s.url, "aXb");
        assert_eq!(s.cursor, 2);
    }

    // ────────────────────────────────────────────────────────────────────────
    // Bidi anti-spoofing (ux-7): insert_text is the single choke point both
    // typed characters and paste go through — see docs/design/bidi.md §4.
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn insert_text_strips_rlo_override_character() {
        // Security regression: U+202E (Right-to-Left Override) must never
        // enter the URL bar's buffer — typing or pasting it must not be able
        // to visually disguise a domain.
        let mut s = make_state("");
        s.insert_text("evil\u{202E}gnp.exe");
        assert!(
            !s.url.contains('\u{202E}'),
            "RLO must be stripped, got: {:?}",
            s.url
        );
        assert_eq!(s.url, "evilgnp.exe");
    }

    #[test]
    fn insert_text_strips_bidi_isolates_too() {
        let mut s = make_state("");
        s.insert_text("a\u{2066}b\u{2069}c");
        assert_eq!(s.url, "abc");
    }

    #[test]
    fn insert_text_leaves_clean_urls_untouched() {
        let mut s = make_state("");
        s.insert_text("mizu://example.com/page");
        assert_eq!(s.url, "mizu://example.com/page");
    }

    #[test]
    fn paste_text_also_strips_bidi_overrides() {
        // paste_text -> insert_text, so it inherits the same choke point;
        // pinned separately since paste is a distinct entry point a user
        // (or a malicious clipboard source) could exploit independently of
        // typing.
        let mut s = make_state("");
        s.paste_text("safe\u{202E}evil.com");
        assert!(!s.url.contains('\u{202E}'));
    }

    #[test]
    fn delete_backward_removes_selection() {
        let mut s = make_state("hello");
        s.selection = Some((1, 4)); // select "ell"
        s.cursor = 4;
        s.delete_backward();
        assert_eq!(s.url, "ho");
        assert_eq!(s.cursor, 1);
        assert!(s.selection.is_none());
    }

    #[test]
    fn delete_backward_no_selection_removes_char() {
        let mut s = make_state("abc");
        s.cursor = 2;
        s.delete_backward();
        assert_eq!(s.url, "ac");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn select_all_covers_entire_string() {
        let mut s = make_state("hello world");
        s.select_all();
        assert_eq!(s.selection, Some((0, 11)));
        assert_eq!(s.cursor, 11);
    }

    #[test]
    fn selection_range_normalises_inverted_selection() {
        let mut s = make_state("hello");
        s.selection = Some((4, 1)); // inverted (user dragged left)
        assert_eq!(s.selection_range(), Some((1, 4)));
    }

    #[test]
    fn cursor_clamp_on_move_left_at_start() {
        let mut s = make_state("abc");
        s.cursor = 0;
        s.move_left(false);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn cursor_clamp_on_move_right_at_end() {
        let mut s = make_state("abc");
        s.cursor = 3;
        s.move_right(false);
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn chrome_hit_zone_back_button() {
        assert_eq!(chrome_hit_zone(5.0, 10.0, 800.0), ChromeHitZone::BackButton);
    }

    #[test]
    fn chrome_hit_zone_reload_button() {
        assert_eq!(
            chrome_hit_zone(40.0, 10.0, 800.0),
            ChromeHitZone::ReloadButton
        );
    }

    #[test]
    fn chrome_hit_zone_forward_button() {
        assert_eq!(
            chrome_hit_zone(65.0, 10.0, 800.0),
            ChromeHitZone::ForwardButton
        );
    }

    #[test]
    fn chrome_hit_zone_url_bar() {
        assert_eq!(chrome_hit_zone(200.0, 10.0, 800.0), ChromeHitZone::UrlBar);
    }

    #[test]
    fn chrome_hit_zone_background_below_chrome() {
        assert_eq!(
            chrome_hit_zone(200.0, 50.0, 800.0),
            ChromeHitZone::Background
        );
    }

    #[test]
    fn chrome_hit_zone_background_between_reload_and_forward() {
        // x = 57 is between the Reload button end (56) and Forward button
        // start (60).
        assert_eq!(
            chrome_hit_zone(57.0, 10.0, 800.0),
            ChromeHitZone::Background
        );
    }

    #[test]
    fn chrome_hit_zone_background_between_forward_and_url_bar() {
        // x = 85 is between the Forward button end (84) and URL bar start (88).
        assert_eq!(
            chrome_hit_zone(85.0, 10.0, 800.0),
            ChromeHitZone::Background
        );
    }

    #[test]
    fn paste_text_replaces_selection() {
        let mut s = make_state("hello");
        s.selection = Some((0, 5));
        s.cursor = 5;
        s.paste_text("world");
        assert_eq!(s.url, "world");
        assert_eq!(s.cursor, 5);
        assert!(s.selection.is_none());
    }

    #[test]
    fn cut_text_returns_selection() {
        let mut s = make_state("hello");
        s.selection = Some((1, 4));
        s.cursor = 4;
        let cut = s.cut_text();
        assert_eq!(cut, Some("ell".to_string()));
        assert_eq!(s.url, "ho");
    }

    #[test]
    fn ctrl_a_action() {
        let mut s = make_state("hello");
        let mods = ModifiersState::CONTROL;
        let action = s.handle_key(&Key::Character("a".into()), None, mods);
        matches!(action, ChromeKeyAction::Handled);
        assert_eq!(s.selection, Some((0, 5)));
    }
}
