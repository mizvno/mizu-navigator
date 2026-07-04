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
//! * [`InspectorState`] — visibility, active tab, selection, scroll (this
//!   module), plus click routing for the panel area.
//! * [`log`] — always-on bounded ring buffers of runtime/network activity.
//! * [`model`] — builds the flat list of text rows for the active tab.
//! * [`paint`] — Vello/Parley painting of the panel and page highlight.

#![forbid(unsafe_code)]

pub mod log;
pub mod model;
pub mod paint;

use ego_tree::{NodeId as EgoNodeId, Tree};
use std::collections::{HashMap, HashSet};

use crate::parser::MizuNode;

/// Fixed logical width of the docked panel.
pub const PANEL_WIDTH: f32 = 380.0;
/// Height of the tab bar at the top of the panel.
pub const TAB_BAR_HEIGHT: f32 = 26.0;
/// Height of a single content row.
pub const ROW_HEIGHT: f32 = 18.0;
/// Width of the element-picker button at the right end of the tab bar.
pub const PICKER_BTN_WIDTH: f32 = 34.0;

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
    pub fn label(self) -> &'static str {
        match self {
            InspectorTab::Elements => "Elem",
            InspectorTab::Style => "Style",
            InspectorTab::Logic => "Logic",
            InspectorTab::Events => "Events",
            InspectorTab::Network => "Net",
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
    /// Per-tab vertical scroll offset in logical pixels (index = tab index).
    pub scroll: [f32; 5],
    /// Maximum scroll extent of the active tab, updated by the paint pass.
    pub max_scroll: f32,
    /// Last instant the Events tab countdown was refreshed (2 Hz throttle).
    pub last_events_refresh: std::time::Instant,
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
            scroll: [0.0; 5],
            max_scroll: 0.0,
            last_events_refresh: std::time::Instant::now(),
        }
    }

    /// Toggles panel visibility; closing also cancels picker mode.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.set_picker(false);
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
        self.scroll = [0.0; 5];
        self.max_scroll = 0.0;
    }

    /// Scrolls the active tab so row `idx` sits roughly in the upper third of
    /// a viewport `viewport_h` pixels tall (clamped by the next paint pass).
    pub fn scroll_to_row(&mut self, idx: usize, viewport_h: f32) {
        let target = (idx as f32 * ROW_HEIGHT - viewport_h * 0.33).max(0.0);
        self.scroll[self.tab.index()] = target;
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

/// Routes a click inside the panel area.
///
/// `x` is relative to the panel's left edge; `y` is relative to the top of
/// the panel (i.e. just below the chrome bar).  `rows` must be the row list
/// currently displayed (same build the paint pass used).
///
/// Returns `true` when the click changed inspector state (needs a redraw).
pub fn handle_panel_click(
    state: &mut InspectorState,
    rows: &[model::Row],
    x: f32,
    y: f32,
) -> bool {
    // ── Tab bar ──────────────────────────────────────────────────────────
    if y < TAB_BAR_HEIGHT {
        if x >= PANEL_WIDTH - PICKER_BTN_WIDTH {
            state.set_picker(!state.picker);
            return true;
        }
        let tab_strip_width = PANEL_WIDTH - PICKER_BTN_WIDTH;
        let tab_width = tab_strip_width / InspectorTab::ALL.len() as f32;
        let idx = ((x / tab_width) as usize).min(InspectorTab::ALL.len() - 1);
        if let Some(&tab) = InspectorTab::ALL.get(idx)
            && state.tab != tab
        {
            state.tab = tab;
            return true;
        }
        return false;
    }

    // ── Content rows ─────────────────────────────────────────────────────
    let content_y = y - TAB_BAR_HEIGHT + state.scroll_offset();
    if content_y < 0.0 {
        return false;
    }
    let row_idx = (content_y / ROW_HEIGHT) as usize;
    let Some(row) = rows.get(row_idx) else {
        return false;
    };

    let mut changed = false;
    if let Some(node) = row.node {
        if state.selected != Some(node) {
            state.selected = Some(node);
            changed = true;
        }
        // Clicking an expandable Elements row also toggles its children.
        if state.tab == InspectorTab::Elements && row.expandable {
            if !state.collapsed.remove(&node) {
                state.collapsed.insert(node);
            }
            changed = true;
        }
    }
    changed
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
    use super::*;

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
        let rows: Vec<model::Row> = Vec::new();
        // Click in the last tab slot (Network).
        let tab_strip = PANEL_WIDTH - PICKER_BTN_WIDTH;
        let changed = handle_panel_click(&mut s, &rows, tab_strip - 1.0, 10.0);
        assert!(changed);
        assert_eq!(s.tab, InspectorTab::Network);
    }

    #[test]
    fn picker_button_toggles_picker() {
        let mut s = InspectorState::new();
        let rows: Vec<model::Row> = Vec::new();
        let changed = handle_panel_click(&mut s, &rows, PANEL_WIDTH - 5.0, 10.0);
        assert!(changed);
        assert!(s.picker);
    }
}
