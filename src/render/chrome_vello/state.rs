//! `ChromeState`'s behavior: URL-bar editing (cursor/selection-consistent
//! text replacement) and cursor/selection movement.

use super::cursor::*;
use super::*;

// ── ChromeState implementation ────────────────────────────────────────────────

impl ChromeState {
    /// Replaces the URL-bar text and drops every offset that described the
    /// previous text.
    ///
    /// `cursor` and `selection` are byte offsets into [`Self::url`]; assigning
    /// the field directly would leave them pointing into a string that no
    /// longer exists, at an index that may be past the end or inside a
    /// multi-byte character — the shape that turns a navigation into a panic
    /// the moment the user next touches the bar. Every caller that swaps the
    /// displayed URL goes through here so that cannot be forgotten.
    ///
    /// This is display state only: it never establishes an origin. See
    /// [`Self::committed_url`].
    pub fn set_displayed_url(&mut self, url: String) {
        self.url = url;
        self.cursor = self.url.len();
        self.selection = None;
        self.inline_completion = None;
    }

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
