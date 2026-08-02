//! In-memory session history: a bounded two-stack model for the chrome
//! Back/Forward buttons, plus a larger persistent log that backs the
//! history sidebar panel (ux-history).
//!
//! ## Two separate data structures, deliberately not one
//!
//! * [`HistoryStack`] — per-tab, in-memory only, bounded at
//!   [`MAX_HISTORY_ENTRIES`]. Its [`HistoryEntry`] carries a URL and the
//!   scroll offset to restore, and nothing else: a back/forward step has no
//!   use for a timestamp, and the stack is never serialized.
//!
//! * [`HistoryLog`] — window-level, persisted across launches, bounded at
//!   [`MAX_LOG_ENTRIES`]. Its [`VisitRecord`] carries a URL, the document
//!   title, and a wall-clock timestamp, and has no use for a scroll offset.
//!   One record is appended per *arrival* on a page (see
//!   `super::navigate::handle_navigate_success`), which is what makes the
//!   page currently on screen appear in the sidebar at all.
//!
//! ## Security / privacy note
//!
//! Browsing history is exactly the kind of data a local attacker mines, so
//! the log is encrypted at rest (AES-256-GCM, key in the OS keyring) rather
//! than left as readable JSON — see [`crypto`].
//!
//! ## Deliberately minimal Back/Forward scope (retained from ux-4 guard)
//!
//! A history step is still a full top-level navigation — it must go through
//! the same [`super::navigate::navigate_to_url`] choke point as any other
//! navigation (`SECURITY-INVARIANTS.md` N2); this module only tracks
//! *which URL to navigate to next*, never navigates itself and never stores
//! document state or tainted values.
//!
//! Split by concern: [`entry`] (`HistoryEntry` + the size constants),
//! [`visit`] (`VisitRecord`), [`stack`] (`HistoryStack`), [`log`]
//! (`HistoryLog`), [`crypto`] (the AEAD envelope and OS-keyring key
//! management), [`platform`] (data-directory resolution), and [`sidebar`]
//! (`HistorySidebarState`).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::SystemTime;

mod crypto;
mod entry;
mod log;
mod platform;
mod sidebar;
mod stack;
#[cfg(test)]
mod tests;
mod visit;

pub use entry::*;
pub use log::*;
pub use sidebar::*;
pub use stack::*;
pub use visit::*;
