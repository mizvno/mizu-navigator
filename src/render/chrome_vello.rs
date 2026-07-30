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
    kurbo::{Affine, BezPath, Circle, Rect, RoundedRect, Stroke},
    peniko::{BlendMode, Color, Compose, Fill, Mix},
};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::render::preferences::ChromePalette;

// ── Geometry ─────────────────────────────────────────────────────────────────

/// Height of the tab strip band, above the navigation bar.
pub const TAB_STRIP_HEIGHT: f32 = 26.0;

/// Height of the navigation bar band (back/reload/forward + URL bar).
const BAR_HEIGHT: f32 = 28.0;

/// Total height of the chrome in logical pixels — both bands.
///
/// Every content-space calculation in the renderer treats this as "the
/// document's vertical offset", so keeping the tab strip *inside* it (rather
/// than adding a second offset) is what lets the strip be introduced without
/// touching hit tests, clip rects, or viewport maths.
pub const CHROME_HEIGHT: f32 = TAB_STRIP_HEIGHT + BAR_HEIGHT;

/// Minimum/maximum width of one tab in the strip.
const TAB_MIN_W: f32 = 80.0;
const TAB_MAX_W: f32 = 200.0;
/// Width of a tab's close affordance, at the tab's right edge.
const TAB_CLOSE_W: f32 = 16.0;
/// Width of the trailing "new tab" button.
const NEW_TAB_W: f32 = 24.0;
/// Horizontal padding inside a tab, before its title.
const TAB_TEXT_PAD: f32 = 6.0;

const BTN_Y: f32 = TAB_STRIP_HEIGHT + 4.0;
const BTN_H: f32 = 20.0;
const BTN_W: f32 = 24.0;
/// The history-sidebar toggle leads the bar, on the same edge as the panel it
/// opens — a left-docked panel with a right-edge switch reads as unrelated.
const HISTORY_X: f32 = 4.0;
const BACK_X: f32 = 32.0;
const FORWARD_X: f32 = 60.0;
const RELOAD_X: f32 = 88.0;
/// The X coordinate where the URL bar starts.
pub const URL_BAR_X: f32 = 116.0;
const URL_BAR_Y: f32 = TAB_STRIP_HEIGHT + 3.0;
const URL_BAR_H: f32 = 22.0;
/// Width reserved for the status indicator on the right.
pub const STATUS_W: f32 = 40.0;
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
    /// The actual URL of the currently loaded page (the origin of record).
    pub committed_url: String,
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
    /// Autocomplete suggestions retrieved from history.
    pub suggestions: Vec<crate::render::window::history::VisitRecord>,
    /// The currently selected autocomplete suggestion index.
    pub selected_suggestion: Option<usize>,
    /// The currently hovered autocomplete suggestion index.
    pub hovered_suggestion: Option<usize>,
    /// Inline autocompletion string (suffix).
    pub inline_completion: Option<String>,
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
    /// The history-sidebar toggle button, leading the navigation bar.
    HistoryButton,
    /// Any other part of the chrome bar (background).
    Background,
    /// The body of the tab at this index in the visible strip.
    TabItem(usize),
    /// The close affordance of the tab at this index.
    TabCloseButton(usize),
    /// The trailing "new tab" button.
    NewTabButton,
    /// An autocomplete suggestion at this index.
    AutocompleteSuggestion(usize),
}

/// Everything the strip's geometry depends on.
///
/// Passed to both [`chrome_hit_zone`] and [`paint_chrome`] so hit testing and
/// painting derive their rectangles from one function ([`tab_rects`]) and
/// cannot drift apart.
#[derive(Debug, Clone, Copy)]
pub struct ChromeLayout {
    /// Logical window width.
    pub window_width: f32,
    /// Number of open tabs.
    pub tab_count: usize,
    /// Number of visible autocomplete suggestions in the dropdown.
    pub dropdown_count: usize,
}

/// Yields `(index, rect)` for every tab that fits in the strip.
///
/// Tabs past the right edge are simply not produced: the strip clips rather
/// than scrolls, and a tab with no rectangle is neither painted nor hittable,
/// which keeps the two consistent by construction.
pub fn tab_rects(layout: &ChromeLayout) -> impl Iterator<Item = (usize, Rect)> + '_ {
    let avail = (layout.window_width - NEW_TAB_W).max(0.0);
    let width = if layout.tab_count == 0 {
        TAB_MIN_W
    } else {
        (avail / layout.tab_count as f32).clamp(TAB_MIN_W, TAB_MAX_W)
    };
    (0..layout.tab_count).filter_map(move |i| {
        let x = i as f32 * width;
        if x + width > avail {
            return None;
        }
        Some((
            i,
            Rect::new(x as f64, 0.0, (x + width) as f64, TAB_STRIP_HEIGHT as f64),
        ))
    })
}

/// Rect of the trailing "new tab" button.
fn new_tab_rect(layout: &ChromeLayout) -> Rect {
    let x = tab_rects(layout)
        .last()
        .map(|(_, r)| r.x1 as f32)
        .unwrap_or(0.0);
    Rect::new(
        x as f64,
        0.0,
        (x + NEW_TAB_W) as f64,
        TAB_STRIP_HEIGHT as f64,
    )
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

    /// Selects the word or URL segment at the current cursor.
    pub fn select_word_at_cursor(&mut self) {
        if self.url.is_empty() {
            return;
        }

        let chars: Vec<(usize, char)> = self.url.char_indices().collect();
        let cursor_char_idx = chars
            .iter()
            .position(|&(idx, _)| idx >= self.cursor)
            .unwrap_or(chars.len());

        let is_separator = |c: char| " /.:?&=-".contains(c);

        let mut i = cursor_char_idx;
        while i > 0 {
            let (_, c) = chars[i - 1];
            if is_separator(c) {
                break;
            }
            i -= 1;
        }
        let mut start = if i < chars.len() {
            chars[i].0
        } else {
            self.url.len()
        };

        let mut j = cursor_char_idx;
        while j < chars.len() {
            let (_, c) = chars[j];
            if is_separator(c) {
                break;
            }
            j += 1;
        }
        let mut end = if j < chars.len() {
            chars[j].0
        } else {
            self.url.len()
        };

        if start == end && cursor_char_idx < chars.len() {
            start = chars[cursor_char_idx].0;
            end = start + chars[cursor_char_idx].1.len_utf8();
        }

        self.selection = Some((start, end));
        self.cursor = end;
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
                let target_url = if let Some(idx) = self.selected_suggestion {
                    if let Some(record) = self.suggestions.get(idx) {
                        record.url.clone()
                    } else {
                        self.url.trim().to_string()
                    }
                } else if let Some(inline) = &self.inline_completion {
                    format!("{}{}", self.url, inline)
                } else {
                    self.url.trim().to_string()
                };

                let mut url = target_url;
                if !url.is_empty() && !url.contains("://") && !url.starts_with("about:") {
                    url = format!("mizu://{url}");
                }
                self.url = url.clone();
                self.cursor = self.url.len();
                self.selection = None;
                self.focused = false;
                self.suggestions.clear();
                self.selected_suggestion = None;
                self.inline_completion = None;
                ChromeKeyAction::Navigate(url)
            }
            Key::Named(NamedKey::Escape) => {
                self.selection = None;
                self.focused = false;
                self.suggestions.clear();
                self.selected_suggestion = None;
                self.inline_completion = None;
                ChromeKeyAction::Handled
            }
            Key::Named(NamedKey::ArrowUp) => {
                if !self.suggestions.is_empty() {
                    if let Some(idx) = self.selected_suggestion {
                        if idx > 0 {
                            self.selected_suggestion = Some(idx - 1);
                        } else {
                            self.selected_suggestion = None;
                        }
                    } else {
                        self.selected_suggestion = Some(self.suggestions.len() - 1);
                    }
                    ChromeKeyAction::Handled
                } else {
                    ChromeKeyAction::Ignored
                }
            }
            Key::Named(NamedKey::ArrowDown) => {
                if !self.suggestions.is_empty() {
                    if let Some(idx) = self.selected_suggestion {
                        if idx + 1 < self.suggestions.len() {
                            self.selected_suggestion = Some(idx + 1);
                        } else {
                            self.selected_suggestion = None;
                        }
                    } else {
                        self.selected_suggestion = Some(0);
                    }
                    ChromeKeyAction::Handled
                } else {
                    ChromeKeyAction::Ignored
                }
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
pub fn chrome_hit_zone(x: f32, y: f32, layout: &ChromeLayout) -> ChromeHitZone {
    if layout.dropdown_count > 0 {
        let item_h = 24.0;
        let padding = 4.0;
        let dropdown_h = (layout.dropdown_count as f32) * item_h + padding * 2.0;

        let url_bar_right = (layout.window_width - STATUS_W).max(URL_BAR_X + 10.0);

        if x >= URL_BAR_X
            && x < url_bar_right
            && y >= URL_BAR_Y + URL_BAR_H
            && y < URL_BAR_Y + URL_BAR_H + dropdown_h
        {
            let relative_y = y - (URL_BAR_Y + URL_BAR_H + padding);
            if relative_y >= 0.0 {
                let index = (relative_y / item_h) as usize;
                if index < layout.dropdown_count {
                    return ChromeHitZone::AutocompleteSuggestion(index);
                }
            }
            return ChromeHitZone::Background;
        }
    }

    if !(0.0..CHROME_HEIGHT).contains(&y) {
        return ChromeHitZone::Background;
    }
    let window_width = layout.window_width;
    if y < TAB_STRIP_HEIGHT {
        for (i, rect) in tab_rects(layout) {
            if (rect.x0..rect.x1).contains(&(x as f64)) {
                // Close first: its glyph lives inside the tab's own rect, so
                // testing the tab body first would swallow every close click.
                return if (x as f64) >= rect.x1 - TAB_CLOSE_W as f64 {
                    ChromeHitZone::TabCloseButton(i)
                } else {
                    ChromeHitZone::TabItem(i)
                };
            }
        }
        let plus = new_tab_rect(layout);
        if (plus.x0..plus.x1).contains(&(x as f64)) {
            return ChromeHitZone::NewTabButton;
        }
        return ChromeHitZone::Background;
    }
    if (HISTORY_X..HISTORY_X + BTN_W).contains(&x) && (BTN_Y..BTN_Y + BTN_H).contains(&y) {
        return ChromeHitZone::HistoryButton;
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

use std::sync::OnceLock;

fn parse_svg_path(svg: &str) -> BezPath {
    let start_idx = svg.find("d=\"").expect("Valid SVG path") + 3;
    let end_idx = svg[start_idx..].find("\"").expect("Valid SVG path") + start_idx;
    let path_data = &svg[start_idx..end_idx];
    BezPath::from_svg(path_data).expect("Valid SVG path data")
}

fn get_icon_back() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| parse_svg_path(include_str!("../../assets/icons/arrow-left-bold.svg")))
}

fn get_icon_forward() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| parse_svg_path(include_str!("../../assets/icons/arrow-right-bold.svg")))
}

fn get_icon_reload() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| parse_svg_path(include_str!("../../assets/icons/arrow-clockwise-bold.svg")))
}

fn get_icon_history() -> &'static BezPath {
    static ICON: OnceLock<BezPath> = OnceLock::new();
    ICON.get_or_init(|| {
        parse_svg_path(include_str!(
            "../../assets/icons/clock-counter-clockwise-bold.svg"
        ))
    })
}

enum ButtonContent<'a> {
    #[allow(dead_code)]
    Text(&'a str),
    Icon(&'a BezPath),
}

/// Context for [`paint_nav_button`]: everything shared across both the
/// Back and Forward button paint calls in a single `paint_chrome` pass.
struct NavButtonContext<'a> {
    palette: &'a ChromePalette,
    font_cx: &'a mut parley::FontContext,
    layout_cx: &'a mut parley::LayoutContext<vello::peniko::Color>,
    transform: Affine,
}

/// Paints a single square nav button (Back/Forward) at logical X `x`
/// containing the centered glyph `label`. When `enabled` is `false`, both the
/// button background and glyph render at reduced alpha — the dimmed
/// affordance signals the button is inert (empty back/forward stack); the
/// caller is responsible for actually ignoring clicks on it (`window::history`
/// already makes a Back/Forward step a no-op when its stack is empty, so
/// this dimming is purely visual confirmation, not the enforcement point).
fn paint_nav_button(
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
fn paint_toolbar_button(
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

// ── Main paint function ───────────────────────────────────────────────────────

/// Truncates `text` with an ellipsis so it fits `max_width` logical pixels.
///
/// Measures with the same layout builder the strip paints with, so the elision
/// point matches what is actually drawn.
fn elide_to_width(
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

    /// A single-tab strip over an 800px window.
    fn test_layout(tab_count: usize) -> ChromeLayout {
        ChromeLayout {
            window_width: 800.0,
            tab_count,
            dropdown_count: 0,
        }
    }

    /// Hit-tests a point given in *navigation-bar* coordinates: `y` is
    /// measured from the top of the bar, below the tab strip.
    fn bar_zone(x: f32, y: f32) -> ChromeHitZone {
        chrome_hit_zone(x, y + TAB_STRIP_HEIGHT, &test_layout(1))
    }

    #[test]
    fn tab_strip_hit_zones_prefer_close_over_body() {
        let layout = test_layout(3);
        let (_, first) = tab_rects(&layout).next().expect("at least one tab fits");
        assert_eq!(
            chrome_hit_zone(first.x0 as f32 + 4.0, 8.0, &layout),
            ChromeHitZone::TabItem(0)
        );
        assert_eq!(
            chrome_hit_zone(first.x1 as f32 - 4.0, 8.0, &layout),
            ChromeHitZone::TabCloseButton(0),
            "the close glyph sits inside the tab rect, so it must be tested first"
        );
    }

    #[test]
    fn new_tab_button_follows_the_last_tab() {
        let layout = test_layout(2);
        let last = tab_rects(&layout).last().expect("tabs fit").1;
        assert_eq!(
            chrome_hit_zone(last.x1 as f32 + 4.0, 8.0, &layout),
            ChromeHitZone::NewTabButton
        );
    }

    #[test]
    fn tab_strip_clips_rather_than_overflowing() {
        // 32 tabs at the 80px minimum need 2560px; only what fits in 800px
        // (minus the new-tab button) is produced.
        let layout = test_layout(32);
        let visible = tab_rects(&layout).count();
        assert!(visible < 32 && visible > 0, "got {visible} visible tabs");
        assert!(
            tab_rects(&layout).all(|(_, r)| r.x1 <= (layout.window_width - 24.0) as f64),
            "no tab may extend past the new-tab button"
        );
    }

    #[test]
    fn chrome_hit_zone_history_button() {
        assert_eq!(
            bar_zone(HISTORY_X + 5.0, 10.0),
            ChromeHitZone::HistoryButton,
            "the sidebar toggle leads the bar, on the side the panel opens"
        );
    }

    #[test]
    fn chrome_hit_zone_back_button() {
        assert_eq!(bar_zone(BACK_X + 5.0, 10.0), ChromeHitZone::BackButton);
    }

    #[test]
    fn chrome_hit_zone_reload_button() {
        assert_eq!(bar_zone(RELOAD_X + 8.0, 10.0), ChromeHitZone::ReloadButton);
    }

    #[test]
    fn chrome_hit_zone_forward_button() {
        assert_eq!(
            bar_zone(FORWARD_X + 5.0, 10.0),
            ChromeHitZone::ForwardButton
        );
    }

    #[test]
    fn chrome_hit_zone_url_bar() {
        assert_eq!(bar_zone(200.0, 10.0), ChromeHitZone::UrlBar);
    }

    #[test]
    fn toolbar_buttons_do_not_overlap() {
        // Each button owns BTN_W pixels, and every pair must leave a gap:
        // that gap is what the "background between buttons" cases below hit.
        // Left to right as they are laid out, which is not the order the
        // constants happen to be declared in.
        let xs = [HISTORY_X, BACK_X, FORWARD_X, RELOAD_X, URL_BAR_X];
        for pair in xs.windows(2) {
            assert!(
                pair[0] + BTN_W <= pair[1],
                "the button at {} overruns the one at {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn chrome_hit_zone_background_below_chrome() {
        assert_eq!(
            chrome_hit_zone(200.0, CHROME_HEIGHT + 1.0, &test_layout(1)),
            ChromeHitZone::Background
        );
    }

    #[test]
    fn chrome_hit_zone_background_between_reload_and_forward() {
        assert_eq!(
            bar_zone(RELOAD_X + BTN_W + 1.0, 10.0),
            ChromeHitZone::Background
        );
    }

    #[test]
    fn chrome_hit_zone_background_between_forward_and_url_bar() {
        assert_eq!(
            bar_zone(FORWARD_X + BTN_W + 1.0, 10.0),
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
