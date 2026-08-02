//! # Mizu Inspector — in-window developer panel
//!
//! A docked, read-only panel (toggled with **F12**) that makes the manifesto's
//! promise visible: everything a document *can* do is declared, so the
//! inspector shows both the declared surface (elements, styles, functions,
//! timers, endpoints) and the observed runtime activity (state mutations,
//! events, network requests — including the ones blocked by policy).
//!
//! ## Architecture
//!
//! The inspector runs entirely on the UI thread and only *reads*
//! `MizuWindowManager` state, so it needs no locks and no cross-thread
//! messages; the data it paints is exactly the state of the current frame.
//!
//! * [`InspectorState`] — visibility, active tab, selection, scroll, hover.
//! * [`log`] — always-on bounded ring buffers of runtime/network activity.
//! * [`model`] — builds the list of structured rows for the active tab.
//! * [`paint`] — Vello/Parley painting of the panel and page highlight.
//!
//! ## One geometry, shared by painting and hit testing
//!
//! Rows are not all the same height (section headers get leading, detail
//! lines are tighter), so "which row is at y" is not a division any more.
//! [`row_tops`] is the single source of truth for the panel's vertical
//! geometry and [`panel_hit`] for its horizontal zones; the paint pass and
//! the click router both consume them, which is what keeps what is on screen
//! and what is clickable from drifting apart.

#![forbid(unsafe_code)]

pub mod log;
pub mod model;
pub mod paint;

use ego_tree::{NodeId as EgoNodeId, Tree};
use std::collections::{HashMap, HashSet};

use crate::parser::MizuNode;

// ── Geometry ─────────────────────────────────────────────────────────────────

/// Fixed logical width of the docked panel.
///
/// Wide enough to spell out all five tab labels and to keep a typical
/// `mizu://host/path` on one line; the panel shrinks the page viewport, so
/// every extra pixel here is one the document loses.
pub const PANEL_WIDTH: f32 = 420.0;

/// Height of the tab bar at the top of the panel.
pub const TAB_BAR_HEIGHT: f32 = 30.0;

/// Height of a primary content row.
pub const ROW_HEIGHT: f32 = 21.0;

/// Height of a continuation row hanging off the item above it.
pub const DETAIL_ROW_HEIGHT: f32 = 19.0;

/// Height of a section header row, including its leading.
pub const HEADER_ROW_HEIGHT: f32 = 30.0;

/// Width of the element-picker button at the right end of the tab bar.
pub const PICKER_BTN_WIDTH: f32 = 38.0;

/// Horizontal padding between the panel's edges and its content.
pub const HPAD: f32 = 11.0;

/// Horizontal offset added per level of tree depth.
pub const INDENT_STEP: f32 = 13.0;

/// Width of the disclosure-triangle hit target on an expandable row.
pub const TWISTY_WIDTH: f32 = 13.0;

/// Width reserved along the panel's right edge for the scrollbar, so text
/// never runs underneath the thumb.
pub const SCROLLBAR_GUTTER: f32 = 10.0;

/// Vertical padding above the first row and below the last.
pub const CONTENT_VPAD: f32 = 4.0;

// ── Value-inspection drawer ─────────────────────────────────────────────────
//
// A row's segments are elided to fit; the drawer is where the elided text is
// read in full. It is a fixed region docked to the panel's bottom edge —
// mirroring how Chrome's Network panel opens request details below the list,
// or how VS Code's hover peek opens a scrollable pane rather than growing the
// line in place — rather than reflowing the clicked row itself, which would
// need font metrics during model building just to size `row_tops` correctly.

/// Fixed height of the value-inspection drawer.
pub const DRAWER_HEIGHT: f32 = 176.0;

/// Height of the drawer's header band (title + Copy + Close).
pub const DRAWER_HEADER_HEIGHT: f32 = 28.0;

/// Horizontal/vertical padding inside the drawer's body.
pub const DRAWER_PAD: f32 = 10.0;

/// Width of the drawer's square Close button.
pub const DRAWER_CLOSE_WIDTH: f32 = 28.0;

/// Width of the drawer's Copy button, immediately left of Close.
pub const DRAWER_COPY_WIDTH: f32 = 56.0;

/// The inspector's content tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorTab {
    /// Document tree with selection and page highlight.
    Elements,
    /// Computed style and box metrics of the selected element.
    Style,
    /// Live variables, computed bindings, and functions.
    Logic,
    /// Declared timers/actions and the runtime event log.
    Events,
    /// Declared endpoints, storage quota, and the network log.
    Network,
}

impl InspectorTab {
    /// All tabs in display order.
    pub const ALL: [InspectorTab; 5] = [
        InspectorTab::Elements,
        InspectorTab::Style,
        InspectorTab::Logic,
        InspectorTab::Events,
        InspectorTab::Network,
    ];

    /// Display label.
    ///
    /// Spelled out rather than clipped to four characters: "Elem" and "Net"
    /// save pixels the panel does not need to save, at the cost of every
    /// first-time reader having to guess.
    pub fn label(self) -> &'static str {
        match self {
            InspectorTab::Elements => "Elements",
            InspectorTab::Style => "Style",
            InspectorTab::Logic => "Logic",
            InspectorTab::Events => "Events",
            InspectorTab::Network => "Network",
        }
    }

    /// Stable index into per-tab state arrays.
    pub fn index(self) -> usize {
        match self {
            InspectorTab::Elements => 0,
            InspectorTab::Style => 1,
            InspectorTab::Logic => 2,
            InspectorTab::Events => 3,
            InspectorTab::Network => 4,
        }
    }
}

/// Live UI state of the inspector panel.
#[derive(Debug)]
pub struct InspectorState {
    /// Whether the panel is visible.
    pub open: bool,
    /// Currently active tab.
    pub tab: InspectorTab,
    /// Currently selected DOM node (drives Style tab + page highlight).
    pub selected: Option<EgoNodeId>,
    /// Nodes whose children are hidden in the Elements tree.  Everything is
    /// expanded by default; toggling collapses.
    pub collapsed: HashSet<EgoNodeId>,
    /// When `true`, the next click in the page selects the hit node instead
    /// of interacting with it.
    pub picker: bool,
    /// Node currently under the cursor while picker mode is active
    /// (live page highlight before the click commits the selection).
    pub picker_hover: Option<EgoNodeId>,
    /// Cursor position in panel-local logical coordinates (x from the panel's
    /// left edge, y from the top of the tab bar), or `None` when the cursor is
    /// elsewhere.
    ///
    /// Stored as a raw point rather than a resolved row index so the hover
    /// highlight costs nothing on mouse move: the paint pass already computes
    /// the row geometry, and resolves the point against it for free.
    pub hover: Option<(f32, f32)>,
    /// Per-tab vertical scroll offset in logical pixels (index = tab index).
    pub scroll: [f32; 5],
    /// Maximum scroll extent of the active tab, updated by the paint pass.
    pub max_scroll: f32,
    /// Last instant the Events tab countdown was refreshed (2 Hz throttle).
    pub last_events_refresh: std::time::Instant,
    /// Flow metrics: (sources, sinks, violations).
    pub flow_metrics: Option<(usize, usize, usize)>,
    /// Measured text widths, reused across frames by the paint pass.
    pub text_metrics: paint::TextMetrics,
    /// The value-inspection drawer, open when a row's full text is being
    /// read. `None` means closed.
    pub value_view: Option<ValueView>,
}

/// State of the value-inspection drawer: the full text of one row, shown
/// word-wrapped with its own scroll — the panel's answer to "this value was
/// elided, now let me actually read it".
#[derive(Debug)]
pub struct ValueView {
    /// Short label from the row's [`model::InspectValue`].
    pub title: String,
    /// The complete text being shown.
    pub text: String,
    /// Vertical scroll offset within the wrapped text, in logical pixels.
    pub scroll: f32,
    /// Content height measured by the last paint, for scroll clamping.
    pub max_scroll: f32,
    /// When the Copy button was last pressed, so the paint pass can flash
    /// "Copied" for a moment — mirrors [`model`]'s mutation-flash convention.
    pub copied_at: Option<std::time::Instant>,
}

impl ValueView {
    fn new(title: String, text: String) -> Self {
        ValueView {
            title,
            text,
            scroll: 0.0,
            max_scroll: 0.0,
            copied_at: None,
        }
    }
}

impl Default for InspectorState {
    fn default() -> Self {
        Self::new()
    }
}

impl InspectorState {
    /// Creates a closed inspector with default state.
    pub fn new() -> Self {
        Self {
            open: false,
            tab: InspectorTab::Elements,
            selected: None,
            collapsed: HashSet::new(),
            picker: false,
            picker_hover: None,
            hover: None,
            scroll: [0.0; 5],
            max_scroll: 0.0,
            last_events_refresh: std::time::Instant::now(),
            flow_metrics: None,
            text_metrics: paint::TextMetrics::default(),
            value_view: None,
        }
    }

    /// Toggles panel visibility; closing also cancels picker mode and the
    /// value drawer.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.set_picker(false);
            self.hover = None;
            self.value_view = None;
        }
    }

    /// Enables or disables picker mode, clearing the hover highlight when off.
    pub fn set_picker(&mut self, on: bool) {
        self.picker = on;
        if !on {
            self.picker_hover = None;
        }
    }

    /// Clears document-bound state (selection, collapse set, picker) after a
    /// navigation: the old node ids belong to a dropped tree.
    pub fn reset_document_state(&mut self) {
        self.selected = None;
        self.collapsed.clear();
        self.set_picker(false);
        self.hover = None;
        self.scroll = [0.0; 5];
        self.max_scroll = 0.0;
        self.value_view = None;
    }

    /// Scrolls the active tab so the row starting at `row_top` sits in the
    /// upper third of a viewport `viewport_h` pixels tall (clamped by the next
    /// paint pass).
    pub fn scroll_to(&mut self, row_top: f32, viewport_h: f32) {
        self.scroll[self.tab.index()] = (row_top - viewport_h * 0.33).max(0.0);
    }

    /// Scroll offset of the active tab.
    pub fn scroll_offset(&self) -> f32 {
        self.scroll[self.tab.index()]
    }

    /// Scrolls the active tab by `delta` logical pixels, clamped to content.
    pub fn scroll_by(&mut self, delta: f32) {
        let idx = self.tab.index();
        self.scroll[idx] = (self.scroll[idx] + delta).clamp(0.0, self.max_scroll.max(0.0));
    }

    /// Selects `node` and expands every ancestor so the selection is visible
    /// in the Elements tree.  Used by the element picker.
    pub fn select_with_ancestors(&mut self, dom: &Tree<MizuNode>, node: EgoNodeId) {
        self.selected = Some(node);
        self.tab = InspectorTab::Elements;
        let mut cur = dom.get(node);
        while let Some(n) = cur {
            self.collapsed.remove(&n.id());
            cur = n.parent();
        }
    }
}

/// Left edge (logical x) of the panel for a given window width.
pub fn panel_left(window_logical_width: f32) -> f32 {
    (window_logical_width - PANEL_WIDTH).max(0.0)
}

/// Top edge of the scrollable content area, measured from the top of the
/// panel (i.e. below the tab bar and its hairline).
pub fn content_top() -> f32 {
    TAB_BAR_HEIGHT + 1.0
}

/// Cumulative top edge of each row in content space, plus a final entry
/// holding the total content height.
///
/// Returned as a `Vec` with `rows.len() + 1` entries so callers can read a
/// row's extent as `tops[i] .. tops[i + 1]` without a bounds special case.
pub fn row_tops(rows: &[model::Row]) -> Vec<f32> {
    let mut tops = Vec::with_capacity(rows.len() + 1);
    let mut y = CONTENT_VPAD;
    for row in rows {
        tops.push(y);
        y += row.height();
    }
    tops.push(y + CONTENT_VPAD);
    tops
}

/// Index of the row covering content-space `y`, if any.
pub fn row_at(tops: &[f32], y: f32) -> Option<usize> {
    if tops.len() < 2 || y < tops[0] || y >= tops[tops.len() - 1] {
        return None;
    }
    // `tops` is sorted, so the row is the last one starting at or before `y`.
    let idx = tops.partition_point(|&t| t <= y).saturating_sub(1);
    (idx + 1 < tops.len()).then_some(idx)
}

/// Left edge of a row's content, in panel-local coordinates.
pub fn row_content_left(indent: u8) -> f32 {
    HPAD + indent as f32 * INDENT_STEP
}

/// What a point inside the panel refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelHit {
    /// One of the content tabs.
    Tab(InspectorTab),
    /// The element-picker toggle.
    Picker,
    /// The disclosure triangle of row `index` — toggles, never selects.
    Twisty(usize),
    /// The body of row `index`.
    Row(usize),
    /// The drawer's Close button.
    DrawerClose,
    /// The drawer's Copy-to-clipboard button.
    DrawerCopy,
    /// The drawer's scrollable body — consumes the click, nothing to toggle.
    DrawerBody,
    /// Panel chrome with no action of its own.
    Background,
}

/// Resolves a panel-local point to what it refers to.
///
/// `x` is relative to the panel's left edge; `y` is relative to the top of
/// the panel (i.e. just below the chrome bar).  `rows` must be the row list
/// currently displayed, and `scroll` its offset — the same pair the paint
/// pass used. `panel_height` is the panel's total logical height (window
/// height minus the chrome bar) and `drawer_open` whether the value drawer
/// currently occupies the bottom [`DRAWER_HEIGHT`] of it.
pub fn panel_hit(
    rows: &[model::Row],
    scroll: f32,
    panel_height: f32,
    drawer_open: bool,
    x: f32,
    y: f32,
) -> PanelHit {
    if y < TAB_BAR_HEIGHT {
        if x >= PANEL_WIDTH - PICKER_BTN_WIDTH {
            return PanelHit::Picker;
        }
        let tab_strip_width = PANEL_WIDTH - PICKER_BTN_WIDTH;
        let tab_width = tab_strip_width / InspectorTab::ALL.len() as f32;
        let idx = ((x / tab_width).max(0.0) as usize).min(InspectorTab::ALL.len() - 1);
        return PanelHit::Tab(InspectorTab::ALL[idx]);
    }

    if drawer_open {
        let drawer_top = panel_height - DRAWER_HEIGHT;
        if y >= drawer_top {
            return drawer_hit(x, y - drawer_top);
        }
    }

    let tops = row_tops(rows);
    let Some(idx) = row_at(&tops, y - content_top() + scroll) else {
        return PanelHit::Background;
    };
    let row = &rows[idx];
    // The disclosure triangle owns its own strip so a parent can be selected
    // without collapsing it — one click doing both is why the old panel could
    // not inspect a container without hiding its contents.
    if row.expandable {
        let twisty_x0 = row_content_left(row.indent);
        if (twisty_x0..twisty_x0 + TWISTY_WIDTH).contains(&x) {
            return PanelHit::Twisty(idx);
        }
    }
    PanelHit::Row(idx)
}

/// Resolves a point within the drawer's own coordinate space (`y` relative to
/// the drawer's top edge) to a drawer zone.
fn drawer_hit(x: f32, y: f32) -> PanelHit {
    if y < DRAWER_HEADER_HEIGHT {
        let close_x0 = PANEL_WIDTH - DRAWER_CLOSE_WIDTH;
        if x >= close_x0 {
            return PanelHit::DrawerClose;
        }
        let copy_x0 = close_x0 - DRAWER_COPY_WIDTH;
        if x >= copy_x0 {
            return PanelHit::DrawerCopy;
        }
    }
    PanelHit::DrawerBody
}

/// Outcome of routing a click through the panel.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClickOutcome {
    /// Whether the click changed state that needs a redraw.
    pub changed: bool,
    /// Text to place on the system clipboard, set only by the drawer's Copy
    /// button. The inspector module never touches the clipboard itself —
    /// that stays with this codebase's other `arboard` call sites, all in
    /// the window event loop.
    pub copy: Option<String>,
}

impl ClickOutcome {
    fn changed() -> Self {
        ClickOutcome {
            changed: true,
            copy: None,
        }
    }

    fn none() -> Self {
        ClickOutcome::default()
    }
}

/// Routes a click inside the panel area.
///
/// `panel_height` is the panel's total logical height — see [`panel_hit`].
pub fn handle_panel_click(
    state: &mut InspectorState,
    rows: &[model::Row],
    panel_height: f32,
    x: f32,
    y: f32,
) -> ClickOutcome {
    let drawer_open = state.value_view.is_some();
    match panel_hit(rows, state.scroll_offset(), panel_height, drawer_open, x, y) {
        PanelHit::Picker => {
            state.set_picker(!state.picker);
            ClickOutcome::changed()
        }
        PanelHit::Tab(tab) => {
            if state.tab == tab {
                return ClickOutcome::none();
            }
            state.tab = tab;
            ClickOutcome::changed()
        }
        PanelHit::Twisty(idx) => {
            let Some(node) = rows[idx].node else {
                return ClickOutcome::none();
            };
            if !state.collapsed.remove(&node) {
                state.collapsed.insert(node);
            }
            ClickOutcome::changed()
        }
        PanelHit::Row(idx) => {
            let row = &rows[idx];
            // Selecting a tree node and opening the value drawer are
            // independent effects of the same click: a row can do either,
            // both, or neither.
            let mut changed = false;
            if let Some(node) = row.node
                && state.selected != Some(node)
            {
                state.selected = Some(node);
                changed = true;
            }
            if let Some(inspect) = &row.inspect {
                let already_this_one = state
                    .value_view
                    .as_ref()
                    .is_some_and(|v| v.title == inspect.title && v.text == inspect.text);
                // Clicking the same value again closes the drawer, matching
                // how the disclosure triangle and the picker button both
                // toggle rather than only ever opening.
                state.value_view = if already_this_one {
                    None
                } else {
                    Some(ValueView::new(inspect.title.clone(), inspect.text.clone()))
                };
                changed = true;
            }
            if changed {
                ClickOutcome::changed()
            } else {
                ClickOutcome::none()
            }
        }
        PanelHit::DrawerClose => {
            state.value_view = None;
            ClickOutcome::changed()
        }
        PanelHit::DrawerCopy => {
            let copy = state.value_view.as_ref().map(|v| v.text.clone());
            if let Some(view) = &mut state.value_view {
                view.copied_at = Some(std::time::Instant::now());
            }
            ClickOutcome {
                changed: true,
                copy,
            }
        }
        PanelHit::DrawerBody | PanelHit::Background => ClickOutcome::none(),
    }
}

/// Computes the on-screen rectangle of `node` in logical coordinates
/// (already including the chrome offset and scroll), for the page highlight.
///
/// Mirrors the coordinate model of [`crate::render::hit_test`]: each
/// ancestor contributes its Taffy location, and scrolled ancestors shift
/// their children up by their scroll offset.
pub fn node_screen_rect(
    dom: &Tree<MizuNode>,
    taffy: &taffy::TaffyTree<EgoNodeId>,
    node_to_taffy_id: &HashMap<EgoNodeId, taffy::prelude::NodeId>,
    scroll_offsets: &HashMap<EgoNodeId, f32>,
    root_scroll_offset_y: f32,
    chrome_height: f32,
    node: EgoNodeId,
) -> Option<vello::kurbo::Rect> {
    let &t_id = node_to_taffy_id.get(&node)?;
    let layout = taffy.layout(t_id).ok()?;
    let mut x = layout.location.x;
    let mut y = layout.location.y;
    let w = layout.size.width;
    let h = layout.size.height;

    let mut cur = dom.get(node)?.parent();
    while let Some(ancestor) = cur {
        let id = ancestor.id();
        if let Some(&a_tid) = node_to_taffy_id.get(&id)
            && let Ok(a_layout) = taffy.layout(a_tid)
        {
            x += a_layout.location.x;
            y += a_layout.location.y;
        }
        // A scrolled container shifts its children up.
        y -= scroll_offsets.get(&id).copied().unwrap_or(0.0);
        cur = ancestor.parent();
    }

    let top = y + chrome_height - root_scroll_offset_y;
    Some(vello::kurbo::Rect::new(
        x as f64,
        top as f64,
        (x + w) as f64,
        (top + h) as f64,
    ))
}

#[cfg(test)]
mod tests {
    use super::model::{InspectValue, Row, Seg, Tone};
    use super::*;

    /// A generous stand-in for the window height in tests that do not care
    /// about the panel's overall extent, only about rows near its top.
    const PANEL_H: f32 = 600.0;

    /// A one-node tree, plus its node id, for exercising row routing.
    fn tree_with_root() -> (Tree<MizuNode>, EgoNodeId) {
        let node = MizuNode {
            primitive: crate::parser::Primitive::Box,
            attributes: Default::default(),
            events: Default::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        };
        let tree = Tree::new(node);
        let id = tree.root().id();
        (tree, id)
    }

    fn expandable_row(node: EgoNodeId, indent: u8) -> Row {
        Row {
            kind: model::RowKind::Item,
            indent,
            segs: vec![Seg::mono("box", Tone::Accent)],
            node: Some(node),
            expandable: true,
            expanded: true,
            inspect: None,
        }
    }

    /// A non-tree row carrying a full-value payload, as Logic/Network/Events
    /// rows do.
    fn inspectable_row(title: &str, text: &str) -> Row {
        Row {
            kind: model::RowKind::Item,
            indent: 1,
            segs: vec![Seg::mono(text, Tone::Value)],
            node: None,
            expandable: false,
            expanded: false,
            inspect: Some(InspectValue {
                title: title.to_string(),
                text: text.to_string(),
            }),
        }
    }

    #[test]
    fn toggle_closes_picker() {
        let mut s = InspectorState::new();
        s.open = true;
        s.picker = true;
        s.toggle();
        assert!(!s.open);
        assert!(!s.picker, "closing the panel must cancel picker mode");
    }

    #[test]
    fn toggle_closes_the_value_drawer() {
        let mut s = InspectorState::new();
        s.open = true;
        s.value_view = Some(ValueView::new("Value".into(), "x".repeat(100)));
        s.toggle();
        assert!(
            s.value_view.is_none(),
            "closing the panel must not leave a stale drawer open"
        );
    }

    #[test]
    fn scroll_is_clamped_to_content() {
        let mut s = InspectorState::new();
        s.max_scroll = 100.0;
        s.scroll_by(250.0);
        assert_eq!(s.scroll_offset(), 100.0);
        s.scroll_by(-500.0);
        assert_eq!(s.scroll_offset(), 0.0);
    }

    #[test]
    fn tab_bar_click_switches_tab() {
        let mut s = InspectorState::new();
        let rows: Vec<Row> = Vec::new();
        // Click in the last tab slot (Network).
        let tab_strip = PANEL_WIDTH - PICKER_BTN_WIDTH;
        let outcome = handle_panel_click(&mut s, &rows, PANEL_H, tab_strip - 1.0, 10.0);
        assert!(outcome.changed);
        assert_eq!(s.tab, InspectorTab::Network);
    }

    #[test]
    fn every_tab_is_reachable_from_its_own_slot() {
        let strip = PANEL_WIDTH - PICKER_BTN_WIDTH;
        let width = strip / InspectorTab::ALL.len() as f32;
        for (i, tab) in InspectorTab::ALL.iter().enumerate() {
            let centre = i as f32 * width + width / 2.0;
            assert_eq!(
                panel_hit(&[], 0.0, PANEL_H, false, centre, 5.0),
                PanelHit::Tab(*tab),
                "the centre of slot {i} must select {tab:?}"
            );
        }
    }

    #[test]
    fn picker_button_toggles_picker() {
        let mut s = InspectorState::new();
        let rows: Vec<Row> = Vec::new();
        let outcome = handle_panel_click(&mut s, &rows, PANEL_H, PANEL_WIDTH - 5.0, 10.0);
        assert!(outcome.changed);
        assert!(s.picker);
    }

    #[test]
    fn row_tops_accumulate_per_row_heights() {
        let (_tree, id) = tree_with_root();
        let rows = vec![
            Row {
                kind: model::RowKind::Header,
                indent: 0,
                segs: vec![],
                node: None,
                expandable: false,
                expanded: false,
                inspect: None,
            },
            expandable_row(id, 0),
        ];
        let tops = row_tops(&rows);
        assert_eq!(tops.len(), 3);
        assert_eq!(tops[0], CONTENT_VPAD);
        assert_eq!(tops[1], CONTENT_VPAD + HEADER_ROW_HEIGHT);
        assert_eq!(
            tops[2],
            CONTENT_VPAD + HEADER_ROW_HEIGHT + ROW_HEIGHT + CONTENT_VPAD
        );
    }

    #[test]
    fn row_at_resolves_variable_height_rows() {
        let (_tree, id) = tree_with_root();
        let rows = vec![
            Row {
                kind: model::RowKind::Header,
                indent: 0,
                segs: vec![],
                node: None,
                expandable: false,
                expanded: false,
                inspect: None,
            },
            expandable_row(id, 0),
        ];
        let tops = row_tops(&rows);
        assert_eq!(row_at(&tops, CONTENT_VPAD + 1.0), Some(0));
        assert_eq!(
            row_at(&tops, CONTENT_VPAD + HEADER_ROW_HEIGHT + 1.0),
            Some(1)
        );
        assert_eq!(row_at(&tops, 0.0), None, "the top padding is not a row");
        assert_eq!(row_at(&tops, 10_000.0), None, "past the end is not a row");
    }

    #[test]
    fn selecting_a_parent_does_not_collapse_it() {
        let (_tree, id) = tree_with_root();
        let rows = vec![expandable_row(id, 0)];
        let mut s = InspectorState::new();
        // Click well to the right of the disclosure triangle.
        let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
        let x = row_content_left(0) + TWISTY_WIDTH + 20.0;
        assert!(handle_panel_click(&mut s, &rows, PANEL_H, x, y).changed);
        assert_eq!(s.selected, Some(id));
        assert!(
            s.collapsed.is_empty(),
            "selecting a container must not hide its children"
        );
    }

    #[test]
    fn the_twisty_toggles_without_selecting() {
        let (_tree, id) = tree_with_root();
        let rows = vec![expandable_row(id, 0)];
        let mut s = InspectorState::new();
        let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
        let x = row_content_left(0) + TWISTY_WIDTH / 2.0;
        assert!(handle_panel_click(&mut s, &rows, PANEL_H, x, y).changed);
        assert!(s.collapsed.contains(&id));
        assert_eq!(
            s.selected, None,
            "the triangle is a disclosure, not a target"
        );
        // And it toggles back.
        assert!(handle_panel_click(&mut s, &rows, PANEL_H, x, y).changed);
        assert!(s.collapsed.is_empty());
    }

    #[test]
    fn the_twisty_strip_follows_indentation() {
        let (_tree, id) = tree_with_root();
        let rows = vec![expandable_row(id, 3)];
        let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
        assert_eq!(
            panel_hit(&rows, 0.0, PANEL_H, false, row_content_left(0) + 2.0, y),
            PanelHit::Row(0),
            "a deep row's triangle is not at the panel's left margin"
        );
        assert_eq!(
            panel_hit(&rows, 0.0, PANEL_H, false, row_content_left(3) + 2.0, y),
            PanelHit::Twisty(0)
        );
    }

    #[test]
    fn clicks_past_the_last_row_hit_nothing() {
        let (_tree, id) = tree_with_root();
        let rows = vec![expandable_row(id, 0)];
        let mut s = InspectorState::new();
        assert!(!handle_panel_click(&mut s, &rows, PANEL_H, 100.0, content_top() + 900.0).changed);
        assert_eq!(s.selected, None);
    }

    #[test]
    fn scrolled_rows_hit_test_at_their_scrolled_position() {
        let (_tree, id) = tree_with_root();
        let rows = vec![expandable_row(id, 0), expandable_row(id, 0)];
        // Scroll the first row off the top; the second now sits where the
        // first was.
        assert_eq!(
            panel_hit(
                &rows,
                ROW_HEIGHT,
                PANEL_H,
                false,
                200.0,
                content_top() + CONTENT_VPAD + 2.0
            ),
            PanelHit::Row(1)
        );
    }

    // ── Value drawer ─────────────────────────────────────────────────────

    #[test]
    fn clicking_a_long_value_row_opens_the_drawer() {
        let long = "x".repeat(200);
        let rows = vec![inspectable_row("Value", &long)];
        let mut s = InspectorState::new();
        let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
        let outcome = handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y);
        assert!(outcome.changed);
        let view = s.value_view.expect("the drawer must open");
        assert_eq!(view.title, "Value");
        assert_eq!(view.text, long, "the drawer must show the untruncated text");
    }

    #[test]
    fn clicking_the_same_value_again_closes_the_drawer() {
        let long = "y".repeat(200);
        let rows = vec![inspectable_row("Value", &long)];
        let mut s = InspectorState::new();
        let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
        handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y);
        assert!(s.value_view.is_some());
        handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y);
        assert!(
            s.value_view.is_none(),
            "clicking the same value must toggle the drawer closed, like the twisty"
        );
    }

    #[test]
    fn clicking_a_different_value_replaces_the_drawer() {
        let rows = vec![inspectable_row("Value", &"a".repeat(200)), {
            let mut row = inspectable_row("Target", &"b".repeat(200));
            row.kind = model::RowKind::Detail;
            row
        }];
        let mut s = InspectorState::new();
        let y1 = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
        handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y1);
        let y2 = content_top() + CONTENT_VPAD + ROW_HEIGHT + DETAIL_ROW_HEIGHT / 2.0;
        handle_panel_click(&mut s, &rows, PANEL_H, 50.0, y2);
        let view = s
            .value_view
            .expect("a different value must open, not close");
        assert_eq!(view.title, "Target");
    }

    #[test]
    fn the_drawer_occupies_the_panel_bottom_and_its_buttons_are_reachable() {
        let drawer_top = PANEL_H - DRAWER_HEIGHT;
        assert_eq!(
            panel_hit(&[], 0.0, PANEL_H, true, PANEL_WIDTH - 5.0, drawer_top + 5.0),
            PanelHit::DrawerClose
        );
        assert_eq!(
            panel_hit(
                &[],
                0.0,
                PANEL_H,
                true,
                PANEL_WIDTH - DRAWER_CLOSE_WIDTH - 5.0,
                drawer_top + 5.0
            ),
            PanelHit::DrawerCopy
        );
        assert_eq!(
            panel_hit(
                &[],
                0.0,
                PANEL_H,
                true,
                20.0,
                drawer_top + DRAWER_HEADER_HEIGHT + 10.0
            ),
            PanelHit::DrawerBody
        );
    }

    #[test]
    fn closing_the_drawer_frees_the_rows_it_was_covering() {
        let (_tree, id) = tree_with_root();
        let rows = vec![expandable_row(id, 0)];
        // With the drawer open, a click at the row's old position (now under
        // the drawer) must not reach the row.
        let y = content_top() + CONTENT_VPAD + ROW_HEIGHT / 2.0;
        // Only true if the row sits below the drawer's top; pick a tall panel
        // so the row is unambiguously in the drawer's shadow instead.
        let short_panel = content_top() + DRAWER_HEIGHT - 1.0;
        assert_eq!(
            panel_hit(
                &rows,
                0.0,
                short_panel,
                true,
                100.0,
                y.min(short_panel - 1.0)
            ),
            PanelHit::DrawerBody,
            "a row underneath an open drawer must not be clickable"
        );
    }

    #[test]
    fn copy_reports_the_full_text_without_touching_the_clipboard() {
        let long = "z".repeat(200);
        let mut s = InspectorState::new();
        s.value_view = Some(ValueView::new("Value".into(), long.clone()));
        let drawer_top = PANEL_H - DRAWER_HEIGHT;
        let outcome = handle_panel_click(
            &mut s,
            &[],
            PANEL_H,
            PANEL_WIDTH - DRAWER_CLOSE_WIDTH - 5.0,
            drawer_top + 5.0,
        );
        assert_eq!(
            outcome.copy,
            Some(long),
            "the click must hand back the exact text to copy"
        );
        assert!(
            s.value_view.is_some(),
            "copying must not itself close the drawer"
        );
        assert!(
            s.value_view.unwrap().copied_at.is_some(),
            "the flash timestamp must be recorded for the paint pass"
        );
    }

    #[test]
    fn drawer_close_button_closes_it() {
        let mut s = InspectorState::new();
        s.value_view = Some(ValueView::new("Value".into(), "hello".into()));
        let drawer_top = PANEL_H - DRAWER_HEIGHT;
        let outcome = handle_panel_click(&mut s, &[], PANEL_H, PANEL_WIDTH - 5.0, drawer_top + 5.0);
        assert!(outcome.changed);
        assert!(s.value_view.is_none());
    }
}
