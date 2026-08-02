//! `HistoryStack`: the per-tab, in-memory back/forward stacks.

use super::*;

// ── HistoryStack (per-tab, in-memory back/forward) ────────────────────────────

/// Pushes `entry` onto `stack`, dropping the oldest entry if the push would
/// exceed [`MAX_HISTORY_ENTRIES`].
pub(super) fn push_capped(stack: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    stack.push(entry);
    if stack.len() > MAX_HISTORY_ENTRIES {
        stack.remove(0);
    }
}

/// The bounded two-stack session history model.
///
/// `back` holds pages navigated away from, oldest first; `forward` holds
/// pages "undone" by a Back step, oldest first — both capped at
/// [`MAX_HISTORY_ENTRIES`] with oldest-first eviction.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HistoryStack {
    pub(super) back: Vec<HistoryEntry>,
    pub(super) forward: Vec<HistoryEntry>,
}

impl HistoryStack {
    /// Whether a Back step is available.
    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether a Forward step is available.
    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Records a fresh top-level navigation away from `leaving` (i.e. one
    /// that is neither a history step nor a redirect continuation of one).
    ///
    /// Clears `forward` — standard browser semantics: a fresh navigation
    /// invalidates whatever was "undone". Pushes `leaving` onto `back`,
    /// capped at [`MAX_HISTORY_ENTRIES`].
    pub fn record_navigation(&mut self, leaving: HistoryEntry) {
        self.forward.clear();
        push_capped(&mut self.back, leaving);
    }

    /// Pops the most recent `back` entry to navigate to, pushing `leaving`
    /// (the page being left) onto `forward`.
    ///
    /// Returns `None` — and leaves both stacks untouched — when `back` is
    /// empty, so a click on a disabled Back button is a guaranteed no-op
    /// rather than a silent wrong navigation.
    pub fn go_back(&mut self, leaving: HistoryEntry) -> Option<HistoryEntry> {
        let target = self.back.pop()?;
        push_capped(&mut self.forward, leaving);
        Some(target)
    }

    /// Symmetric to [`Self::go_back`]: pops the most recent `forward` entry,
    /// pushing `leaving` onto `back`.
    pub fn go_forward(&mut self, leaving: HistoryEntry) -> Option<HistoryEntry> {
        let target = self.forward.pop()?;
        push_capped(&mut self.back, leaving);
        Some(target)
    }
}
