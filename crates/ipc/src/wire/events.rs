//! # `wire::events` — `WireUiEvent` and `WireTabId`
//!
//! These are the rkyv-archivable mirrors of
//! [`mizu_core::messages::UiEvent`] and [`mizu_core::messages::TabId`].
//! They flow **main process → worker** over the IPC socket as
//! length-prefixed frames.
//!
//! ## `Reload` variant
//!
//! `UiEvent::Reload` carries a `Box<ReloadPayload>` — the whole compiled
//! document. It travels inline in the event frame; see the variant's own
//! docs for why that is preferred over the shared-memory handle the
//! original design used.
//! ## `SubmitForm` encoding
//!
//! `UiEvent::SubmitForm` carries an `FxHashMap<String, Value>`.  `HashMap`
//! is not rkyv-archivable in a zero-copy fashion, so it is encoded as two
//! parallel `Vec`s (`field_keys` / `field_values`) of equal length.  The
//! worker reassembles the map in O(n) after archive validation.

#![forbid(unsafe_code)]

use rkyv::{Archive, Deserialize, Serialize};

use crate::wire::value::WireValue;
use crate::wire::reload::WireReloadPayload;

/// Wire-format mirror of [`mizu_core::messages::TabId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireTabId(pub u64);

impl From<mizu_core::messages::TabId> for WireTabId {
    fn from(t: mizu_core::messages::TabId) -> Self {
        WireTabId(t.0)
    }
}

impl From<WireTabId> for mizu_core::messages::TabId {
    fn from(w: WireTabId) -> Self {
        mizu_core::messages::TabId(w.0)
    }
}

/// Describes the shared-memory region that holds the serialized
/// `WireReloadPayload` archive.
///
/// Transmitted as part of `WireUiEvent::Reload`.  The actual OS handle
/// (file descriptor / Windows HANDLE) is delivered separately via the
/// socket's ancillary data channel before the event frame is sent, so the
/// worker can map the region before processing the frame.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireReloadHandle {
    /// Byte length of the rkyv archive stored in the SHM region.
    pub byte_len: u64,
    /// Monotonically increasing generation counter.  The worker unmaps the
    /// previous region when it receives a handle with a new generation.
    pub generation: u32,
}

/// Wire-format mirror of [`mizu_core::messages::UiEvent`].
///
/// Framed as `[u32 LE len][rkyv bytes]` and sent over the IPC socket.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireUiEvent {
    /// A click landed on a node carrying a `click -> …` action.
    Click {
        /// u32 id of the clicked node.
        node_id: u32,
    },

    /// A root-level `timer …` declared in the `logic` block fired.
    RootTimer {
        /// Index into `WireReloadPayload::root_timer_actions`.
        index: u32,
    },

    /// Aggregated form submission.
    ///
    /// Encoded as two parallel `Vec`s instead of a `HashMap` for
    /// rkyv compatibility (see module-level note).
    SubmitForm {
        /// u32 id of the submit-button node whose `submit -> …` action fires.
        submitter_node_id: u32,
        /// Form field names, one per entry.
        field_keys: Vec<String>,
        /// Corresponding field values, parallel to `field_keys`.
        field_values: Vec<WireValue>,
    },

    /// Updates a variable in the worker store by name.
    ///
    /// Uses a resolved string (not a `Symbol`) for the same reason as
    /// [`mizu_core::messages::UiEvent::UpdateVariable`]: the broker and the
    /// worker each own independent clones of the frozen interner, so a
    /// `Symbol` minted by one is meaningless on the other.
    UpdateVariable {
        /// Variable name string.
        name: String,
        /// New value.
        value: WireValue,
    },

    /// Document reload, carrying the compiled document inline.
    ///
    /// # Why inline rather than via shared memory
    ///
    /// The original design put the payload in a [`crate::shm`] region and
    /// sent only a [`WireReloadHandle`] here. That requires passing an OS
    /// handle to the child — `SCM_RIGHTS` on UNIX, `DuplicateHandle` on
    /// Windows — which is exactly the capability-leak surface the Phase 4
    /// spawner was designed to avoid by having the worker inherit *nothing*.
    ///
    /// Inline transmission costs one copy through the framer, bounded by
    /// [`crate::transport::frame::MAX_FRAME_BYTES`] (64 MiB) — orders of
    /// magnitude above any real document — and in exchange the worker needs
    /// no handle-passing machinery, and therefore no syscall for one in its
    /// seccomp allowlist. `shm` remains available for a future payload large
    /// enough to justify reintroducing that surface; nothing uses it today.
    Reload(Box<WireReloadPayload>),

    /// The tab was closed; the worker should drop its per-tab state.
    CloseTab,
}

// ── Conversions ──────────────────────────────────────────────────────────────

impl From<&mizu_core::messages::UiEvent> for WireUiEvent {
    fn from(e: &mizu_core::messages::UiEvent) -> Self {
        use mizu_core::messages::UiEvent;
        match e {
            UiEvent::Click { node_id } => WireUiEvent::Click { node_id: *node_id },
            UiEvent::RootTimer { index } => WireUiEvent::RootTimer { index: *index },
            UiEvent::SubmitForm {
                submitter_node_id,
                fields,
            } => {
                // Parallel vectors, built from one pass so the two can never
                // disagree in length (the rehydrator rejects it if they do).
                let (field_keys, field_values) = fields
                    .iter()
                    .map(|(k, v)| (k.clone(), WireValue::from(v)))
                    .unzip();
                WireUiEvent::SubmitForm {
                    submitter_node_id: *submitter_node_id,
                    field_keys,
                    field_values,
                }
            }
            UiEvent::UpdateVariable { name, value } => WireUiEvent::UpdateVariable {
                name: name.clone(),
                value: WireValue::from(value),
            },
            UiEvent::Reload(payload) => {
                WireUiEvent::Reload(Box::new(WireReloadPayload::from(payload.as_ref())))
            }
            UiEvent::CloseTab => WireUiEvent::CloseTab,
        }
    }
}
