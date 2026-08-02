//! Public state and geometry types for the chrome bar: `ChromeState`,
//! hit-zone/layout types, and the tab-strip rectangle layout that both
//! painting and hit testing derive from.

use vello::kurbo::Rect;

use super::*;

// ── Public types ─────────────────────────────────────────────────────────────

/// The state for the browser chrome UI element.
#[derive(Debug, Default)]
pub struct ChromeState {
    /// The URL of the document **currently loaded in this tab** — the origin
    /// of record, and the only field any security decision may read.
    ///
    /// It is written at exactly one moment: when a document commits
    /// (`window::navigate::handle_navigate_success`). Dispatching a navigation
    /// does not touch it, so a navigation that is still in flight — or one
    /// that failed and never replaced anything — cannot relabel the origin of
    /// the document that is still running. Everything that decides a
    /// capability from an origin (the `file://`→remote call block, the storage
    /// domain and quota tier, the image sandbox base, relative-URL
    /// resolution, and `check_navigation`'s "current origin") reads this
    /// field; see `SECURITY-INVARIANTS.md` §N5.
    pub committed_url: String,
    /// Current text in the URL bar: a *display and editing* buffer, freely
    /// mutated by typing, pasting and autocomplete.
    ///
    /// It is therefore attacker- and user-influenced at any instant and is
    /// never an origin. Use [`Self::committed_url`] for anything but
    /// rendering the bar.
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
pub(super) fn new_tab_rect(layout: &ChromeLayout) -> Rect {
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
