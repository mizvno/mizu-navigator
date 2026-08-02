//! `MizuWindowManager` and its lifecycle/state methods.
//!
//! Split by concern: [`types`] (`TabState`, `MizuWindowManager`,
//! `ReloadedDocument`, `TabDocument`, `WindowCtx`), [`tabs`] (`split_active`
//! and tab lifecycle: active/open/close/switch), [`construct`]
//! (`TabState::new`, `MizuWindowManager::new*`, and the smaller per-tab
//! bookkeeping methods), [`reload`] (`reload_tab_document`), [`viewport`]
//! (`resize_tab_viewport`/`refresh_tab_virtualized_windows`), and
//! [`capability`] (`execute_tab_capability_action`).

mod capability;
mod construct;
mod reload;
mod tabs;
mod types;
mod viewport;

pub(crate) use capability::execute_tab_capability_action;
pub(crate) use reload::reload_tab_document;
pub use types::*;
pub(crate) use viewport::{refresh_tab_virtualized_windows, resize_tab_viewport};
