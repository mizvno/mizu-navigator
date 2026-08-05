//! Wire-type module re-exports.
//!
//! Every `Wire*` type in this module is a rkyv-archivable mirror of the
//! corresponding type in `mizu-core`.  They are **not** the same types —
//! they are purpose-built for IPC serialization and may differ structurally
//! (e.g. `HashMap` → parallel `Vec`s, `Arc<str>` → `String`).

#![forbid(unsafe_code)]

pub mod actions;
pub mod events;
pub mod rehydrate;
pub mod reload;
pub mod response;
pub mod value;

pub use actions::{WireNetworkMethod, WirePayloadFormat, WireRuntimeAction};
pub use events::{WireReloadHandle, WireTabId, WireUiEvent};
pub use reload::{
    WireAction, WireComputedBinding, WireMizuFunction, WireReloadPayload, WireUrlEndpoint,
    WireUrlEndpointKind,
};
pub use response::{WireWorkerEnvelope, WireWorkerError, WireWorkerResponse};
pub use value::{WireRecordField, WireValue};
