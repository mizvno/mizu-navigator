//! `HistoryEntry`: one per-tab back/forward stack entry (URL + scroll offset).

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum entries kept per stack (`back` and `forward` independently). Caps
/// memory for a long-lived session; the oldest entry is dropped when the cap
/// is exceeded (consistent with the project's other named, bounded budgets —
/// see `SECURITY-INVARIANTS.md` §2 L1).
pub(crate) const MAX_HISTORY_ENTRIES: usize = 100;

/// Maximum entries in the persistent [`HistoryLog`]. Larger than the
/// back/forward stacks because the log covers the full session history shown
/// in the sidebar, not just undo-able steps.
pub(crate) const MAX_LOG_ENTRIES: usize = 5_000;

// ── HistoryEntry (back/forward) ────────────────────────────────────────────────

/// A single back/forward entry: the resolved URL and the vertical scroll
/// offset at the moment the page was left.
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
