//! `VisitRecord`: one persistent-log entry (URL + title + timestamp) and its
//! day-label/grouping helpers for the sidebar.

use super::*;

// ── VisitRecord (sidebar log) ─────────────────────────────────────────────────

/// One visited page, as displayed by the history sidebar.
///
/// Carries only what the sidebar renders and groups by. This is the only
/// history type that is ever written to disk, so every field added here is a
/// field persisted about the user's browsing — keep it minimal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisitRecord {
    /// The resolved `mizu://` or `file://` URL of the page.
    pub url: String,
    /// Document title from the `doc "…"` attribute; empty when the document
    /// has none, in which case the sidebar falls back to the URL.
    #[serde(default)]
    pub title: String,
    /// Wall-clock instant of the visit, in seconds since the Unix epoch.
    /// `0` means "unknown" (a record written before this field existed).
    #[serde(default)]
    pub timestamp_secs: u64,
}

/// Seconds since the Unix epoch, or `0` if the system clock is unreadable.
pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl VisitRecord {
    /// Records a visit to `url` with `title`, stamped with the current time.
    pub fn new(url: String, title: String) -> Self {
        Self {
            url,
            title,
            timestamp_secs: now_secs(),
        }
    }

    /// The label the sidebar shows for this visit: the title when non-empty,
    /// otherwise the URL.
    pub fn display_label(&self) -> &str {
        if self.title.is_empty() {
            &self.url
        } else {
            &self.title
        }
    }

    /// Whole days elapsed since this visit (0 = today, 1 = yesterday, …),
    /// on UTC midnight boundaries.
    ///
    /// UTC rather than local midnight is a deliberate simplification: the
    /// grouping is a convenience, and pulling in a timezone database to move
    /// a label by a few hours is not worth the dependency. A visit with an
    /// unknown timestamp, or one dated in the future by clock skew, counts
    /// as today.
    pub fn days_ago(&self) -> u64 {
        if self.timestamp_secs == 0 {
            return 0;
        }
        const DAY: u64 = 86_400;
        let now = now_secs();
        if now < self.timestamp_secs {
            return 0;
        }
        now / DAY - self.timestamp_secs / DAY
    }

    /// Human-readable day label for the sidebar's group headers.
    pub fn day_label(&self) -> String {
        /// `1 → "1 <unit> ago"`, otherwise `"n <unit>s ago"`.
        fn ago(n: u64, unit: &str) -> String {
            if n == 1 {
                format!("1 {unit} ago")
            } else {
                format!("{n} {unit}s ago")
            }
        }
        match self.days_ago() {
            0 => "Today".to_string(),
            1 => "Yesterday".to_string(),
            d if d < 7 => ago(d, "day"),
            d if d < 30 => ago(d / 7, "week"),
            d if d < 365 => ago(d / 30, "month"),
            d => ago(d / 365, "year"),
        }
    }
}
