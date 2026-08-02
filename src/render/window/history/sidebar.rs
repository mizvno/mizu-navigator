//! `HistorySidebarState`: transient UI state (open/scroll/hover) for the
//! history sidebar panel.

// ── HistorySidebarState ───────────────────────────────────────────────────────

/// UI state for the history sidebar panel.
///
/// Window-level, like the panel itself and like [`HistoryLog`]: the sidebar
/// shows every tab's history, so it cannot live on a `TabState` the way the
/// inspector's state does.
#[derive(Debug, Default, Clone)]
pub struct HistorySidebarState {
    /// Whether the panel is currently visible.
    pub open: bool,
    /// Vertical scroll offset of the panel's content (logical pixels).
    pub scroll_offset: f32,
    /// Index (newest-first) of the record under the cursor, if any.
    pub hovered: Option<usize>,
}

impl HistorySidebarState {
    /// Shows or hides the panel, returning the new visibility.
    ///
    /// Opening starts at the top of the list — the newest visits, which is
    /// what a user opening the history is looking for — and closing drops
    /// the hover highlight so it cannot flash stale on the next open.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.scroll_offset = 0.0;
        self.hovered = None;
        self.open
    }

    /// Hides the panel, clearing transient state. A no-op when already
    /// closed, so callers can use it as an unconditional "dismiss".
    pub fn close(&mut self) {
        self.open = false;
        self.scroll_offset = 0.0;
        self.hovered = None;
    }
}
