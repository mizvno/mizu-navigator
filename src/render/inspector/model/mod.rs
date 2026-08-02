//! Builds the row lists displayed by each inspector tab.
//!
//! The model is a pure function of the manager's current state: every call to
//! [`build_rows`] produces the rows for the active tab from scratch.  Redraws
//! are event-driven, documents are small by design, and all inputs live on the
//! UI thread, so rebuilding is both cheap and always consistent.
//!
//! ## Rows are structured, not pre-formatted
//!
//! A [`Row`] is a list of typed [`Seg`]ments rather than one finished string.
//! That split is what lets the paint pass do its job properly: it can colour a
//! key differently from its value, set code in monospace and labels in the UI
//! face, right-align durations against the panel's edge, and — critically —
//! decide *at paint time*, against the real measured width, which segment to
//! elide and how (see [`Flex`]).  A pre-joined string can do none of that; it
//! can only be clipped mid-glyph.
//!
//! The corollary is that **the model never truncates for display**.  It hands
//! over the full text and lets the painter fit it.  The only truncation that
//! remains is [`crate::render::inspector::log`]'s memory bound on retained log
//! strings, which is deliberately far wider than any panel.
//!
//! ## Reading a value that does not fit
//!
//! Eliding a long URL or expression to fit a 420px row is right for the row —
//! but it must not be the only way to read that value, the way the old panel
//! left it. Any row whose text is long enough that elision is a real risk
//! carries an [`InspectValue`] payload: the row's full, untruncated text plus
//! a short label. Clicking the row opens the panel's value-inspection drawer
//! (see [`crate::render::inspector::ValueView`]) with that text word-wrapped
//! and independently scrollable — the same shape as a browser's Network
//! request-details pane or an Elements attribute editor, rather than a tooltip
//! that vanishes when the mouse moves.
//!
//! Split by concern: [`types`] (the row vocabulary — `Tone`/`Face`/`Flex`/
//! `Seg`/`RowKind`/`InspectValue`/`Row`/`InspectorSources`, plus `build_rows`),
//! [`rows`] (the per-tab row builders), and [`format`] (expression/action
//! pretty-printing).

#![forbid(unsafe_code)]

mod format;
mod rows;
#[cfg(test)]
mod tests;
mod types;

pub use format::{format_action, format_expr};
pub use rows::{node_label, node_label_segs};
pub use types::*;
