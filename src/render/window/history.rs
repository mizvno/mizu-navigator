//! In-memory session history: a bounded two-stack model for the chrome
//! Back/Forward buttons.
//!
//! Deliberately minimal (ux-4 scope guard): no tabs, no history-list UI, no
//! persistence across launches. A history step is still a full top-level
//! navigation — it must go through the same [`super::navigate::navigate_to_url`]
//! choke point as any other navigation (`SECURITY-INVARIANTS.md` N2); this
//! module only tracks *which URL to navigate to next*, never navigates
//! itself and never stores document state or tainted values.

/// Maximum entries kept per stack (`back` and `forward` independently). Caps
/// memory for a long-lived session; the oldest entry is dropped when the cap
/// is exceeded (consistent with the project's other named, bounded budgets —
/// see `SECURITY-INVARIANTS.md` §2 L1).
pub(crate) const MAX_HISTORY_ENTRIES: usize = 100;

/// A single history entry: the resolved URL and the vertical scroll offset
/// at the moment the page was left.
///
/// Deliberately just these two fields — never document state, form values,
/// or anything tainted. Restoring a history entry re-navigates to `url`
/// through the normal navigation choke point exactly like a fresh
/// navigation; `scroll_y` is cosmetic restoration applied after the page
/// reloads.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// The resolved `mizu://` or `file://` URL of the page.
    pub url: String,
    /// Vertical scroll offset (logical pixels) at the moment this page was
    /// left, restored after navigating back/forward to it.
    pub scroll_y: f32,
}

/// The bounded two-stack session history model.
///
/// `back` holds pages navigated away from, oldest first; `forward` holds
/// pages "undone" by a Back step, oldest first — both capped at
/// [`MAX_HISTORY_ENTRIES`] with oldest-first eviction.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HistoryStack {
    back: Vec<HistoryEntry>,
    forward: Vec<HistoryEntry>,
}

/// Pushes `entry` onto `stack`, dropping the oldest entry if the push would
/// exceed [`MAX_HISTORY_ENTRIES`].
fn push_capped(stack: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    stack.push(entry);
    if stack.len() > MAX_HISTORY_ENTRIES {
        stack.remove(0);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str) -> HistoryEntry {
        HistoryEntry {
            url: url.to_string(),
            scroll_y: 0.0,
        }
    }

    fn urls(stack: &[HistoryEntry]) -> Vec<&str> {
        stack.iter().map(|e| e.url.as_str()).collect()
    }

    #[test]
    fn navigate_a_b_c_then_back_and_forward() {
        // Pure stack logic, no window needed: navigate A -> B -> C, then
        // walk back to A and forward to B, then a fresh navigation to D.
        let mut h = HistoryStack::default();

        // Start at A, navigate to B: push A.
        h.record_navigation(entry("A"));
        assert_eq!(urls(&h.back), vec!["A"]);
        assert!(!h.can_go_forward());

        // At B, navigate to C: push B.
        h.record_navigation(entry("B"));
        assert_eq!(urls(&h.back), vec!["A", "B"]);
        assert!(!h.can_go_forward());

        // At C, go back: target must be B; back=[A], forward=[C].
        let target = h.go_back(entry("C")).expect("back must be available");
        assert_eq!(target.url, "B");
        assert_eq!(urls(&h.back), vec!["A"]);
        assert_eq!(urls(&h.forward), vec!["C"]);

        // At B, go back again: target must be A; back=[], forward=[C,B].
        let target = h.go_back(entry("B")).expect("back must be available");
        assert_eq!(target.url, "A");
        assert!(urls(&h.back).is_empty());
        assert_eq!(urls(&h.forward), vec!["C", "B"]);

        // At A, go forward: target must be B; back=[A], forward=[C].
        let target = h.go_forward(entry("A")).expect("forward must be available");
        assert_eq!(target.url, "B");
        assert_eq!(urls(&h.back), vec!["A"]);
        assert_eq!(urls(&h.forward), vec!["C"]);

        // At B, a FRESH navigation to D: forward is cleared, back=[A,B].
        h.record_navigation(entry("B"));
        assert_eq!(urls(&h.back), vec!["A", "B"]);
        assert!(
            !h.can_go_forward(),
            "a fresh navigation must clear the forward stack"
        );
    }

    #[test]
    fn go_back_on_empty_stack_is_a_no_op() {
        let mut h = HistoryStack::default();
        assert!(!h.can_go_back());
        let result = h.go_back(entry("current"));
        assert!(result.is_none());
        assert!(
            h.forward.is_empty(),
            "a no-op back must not push onto forward either"
        );
    }

    #[test]
    fn go_forward_on_empty_stack_is_a_no_op() {
        let mut h = HistoryStack::default();
        assert!(!h.can_go_forward());
        let result = h.go_forward(entry("current"));
        assert!(result.is_none());
        assert!(h.back.is_empty());
    }

    #[test]
    fn back_stack_is_capped_oldest_dropped() {
        let mut h = HistoryStack::default();
        for i in 0..(MAX_HISTORY_ENTRIES + 1) {
            h.record_navigation(entry(&format!("page-{i}")));
        }
        assert_eq!(h.back.len(), MAX_HISTORY_ENTRIES, "back stack must be capped");
        assert_eq!(
            h.back.first().unwrap().url,
            "page-1",
            "oldest entry (page-0) must have been dropped"
        );
        assert_eq!(h.back.last().unwrap().url, format!("page-{MAX_HISTORY_ENTRIES}"));
    }

    #[test]
    fn forward_stack_is_capped_oldest_dropped() {
        let mut h = HistoryStack::default();
        // Build a deep back stack, then walk it all the way back to fill forward.
        for i in 0..(MAX_HISTORY_ENTRIES + 1) {
            h.record_navigation(entry(&format!("page-{i}")));
        }
        let mut current = format!("page-{MAX_HISTORY_ENTRIES}");
        while h.can_go_back() {
            let target = h.go_back(entry(&current)).unwrap();
            current = target.url;
        }
        assert_eq!(
            h.forward.len(),
            MAX_HISTORY_ENTRIES,
            "forward stack must be capped"
        );
    }

    #[test]
    fn scroll_y_round_trips_through_a_history_step() {
        let mut h = HistoryStack::default();
        h.record_navigation(HistoryEntry {
            url: "A".to_string(),
            scroll_y: 420.0,
        });
        let target = h
            .go_back(HistoryEntry {
                url: "B".to_string(),
                scroll_y: 0.0,
            })
            .unwrap();
        assert_eq!(target.scroll_y, 420.0);
    }
}
