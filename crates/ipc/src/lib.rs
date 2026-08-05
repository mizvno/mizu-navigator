//! # `mizu-ipc` — Zero-Copy Inter-Process Communication Layer
//!
//! This crate defines the complete wire-type architecture and transport
//! primitives used to communicate between the sandboxed `mizu-worker` process
//! and the unsandboxed main broker process.
//!
//! ## Architecture overview
//!
//! ```text
//! ┌─────────────────────────────────────┐    ┌────────────────────────────┐
//! │  Main Process (Capability Broker)   │    │   Worker Process (Jailed)  │
//! │                                     │    │                            │
//! │  IpcSender<WireUiEvent>    ─────────┼────┼──▶ IpcReceiver<WireUiEvent>│
//! │  IpcReceiver<WireWorkerEnvelope> ◀──┼────┼─── IpcSender<WireWorkerEnvelope>
//! │                                     │    │                            │
//! │  ShmRegion (write) ─── seal ───────┼────┼──▶ MappedShmSlice (read)  │
//! │  (WireReloadPayload archive)        │    │   (rkyv zero-copy access)  │
//! └─────────────────────────────────────┘    └────────────────────────────┘
//! ```
//!
//! ## Safety invariant
//!
//! `unsafe_code` is forbidden in every module of this crate except the
//! platform-specific shm/transport/sandbox sub-modules (`shm::unix`,
//! `shm::windows`, `transport::platform::unix`, `transport::platform::windows`,
//! `sandbox::linux`, `sandbox::windows`, `sandbox::macos`), which are the only
//! locations that contain `unsafe` code. Every entry point into those modules
//! is a safe function. `#![forbid(unsafe_code)]` cannot be placed at the
//! crate root because `forbid` cannot be overridden by a descendant module's
//! `allow`; instead each safe module opts in individually.
//!
//! ## Wire-format summary
//!
//! All messages are serialized with **rkyv 0.8** and transmitted as
//! length-prefixed frames (4-byte little-endian `u32` length followed by
//! the rkyv archive bytes).  The `WireReloadPayload` is large enough that
//! it bypasses the frame channel: it is written into an anonymous shared
//! memory region, the OS handle is passed via socket ancillary data
//! (SCM_RIGHTS on Linux/macOS, handle duplication on Windows), and the
//! worker maps the region read-only.  This is the zero-copy path for AST
//! and interner data.

pub mod error;
pub mod process;
pub mod sandbox;
pub mod shm;
pub mod transport;
pub mod wire;

pub use error::IpcError;
pub use sandbox::{confine_current_process, WorkerIpcHandles};
pub use transport::channel::{IpcReceiver, IpcSender};
pub use wire::{
    WireTabId, WireUiEvent, WireWorkerEnvelope, WireWorkerError, WireWorkerResponse,
};
