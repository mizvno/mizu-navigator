//! # `navigation` — Unified Navigation Policy
//!
//! This module is the **single policy choke point** for all document-level
//! navigation in Mizu.  Every proposed navigation — address bar, link click,
//! `navigate` action from logic, redirect of a prior navigation — must pass
//! through [`check_navigation`] before any state change or network command is
//! emitted.
//!
//! ## Invariants
//!
//! These invariants are the future verification target (Kani / Creusot); the
//! function is kept pure (no I/O, no side effects) for that reason.
//!
//! - **N1 — No escalation.** A network operation whose purpose is data or media
//!   (`Fetch`, `FetchImage`, `NetworkRequest`) must never cause document
//!   navigation, under any server response.  Enforced by the callers: those
//!   paths never call [`check_navigation`].
//!
//! - **N2 — Single choke point.** Every top-level navigation passes through
//!   [`check_navigation`] before any state change or `NetworkCmd::Navigate` is
//!   emitted.
//!
//! - **N3 — Agency.** Same-origin top-level navigation is always allowed.
//!   Cross-origin top-level navigation is allowed only when the initiating
//!   cause carries a user gesture.  Logic-initiated navigation without a
//!   gesture (timer tick, network-response batch) may not leave the origin.
//!
//! - **N4 — Scheme.** Only `mizu://` is navigable over the network; `file://`
//!   only under the existing sandbox rules; `http(s)://` and everything else
//!   are refused *at this choke point*, not per call site.  `about:blank` is a
//!   no-op handled upstream.
//!
//! - **N5 — Uniform lifecycle.** Origin-scoped state (`capability_policy`
//!   reset, redirect-chain budget, `url_registry` replacement on load) is
//!   handled identically on every navigation path.  Callers must reset
//!   `capability_policy` on every `Allow` verdict.  No path may set
//!   `chrome_state.url` or emit `NetworkCmd::Navigate` around the choke point.

#![forbid(unsafe_code)]

pub use mizu_core::security::navigation::{
    NavigationInitiator, NavigationVerdict, check_navigation,
};
