//! # `error` — `IpcError` and its conversion into `MizuError`
//!
//! All IPC failures surface as [`IpcError`] variants inside this crate.
//! At the boundary with `mizu-core` the display string is forwarded into
//! [`mizu_core::core::errors::MizuError::IpcError`], avoiding a circular
//! crate dependency while still keeping the full diagnostic message.

#![forbid(unsafe_code)]

use thiserror::Error;

/// The canonical error type for every failure that can occur in the IPC layer:
/// transport I/O, frame framing limits, rkyv archive validation, shared
/// memory allocation/mapping, and unexpected worker termination.
///
/// # Integration with `MizuError`
///
/// `IpcError` is not `#[from]`-linked to `MizuError` (which lives in
/// `mizu-core`).  Convert at the boundary with:
///
/// ```rust,ignore
/// ipc_result.map_err(|e| MizuError::IpcError(e.to_string()))?;
/// ```
#[derive(Debug, Error)]
pub enum IpcError {
    /// A low-level I/O failure on the socket or named pipe.
    #[error("IPC transport error: {0}")]
    Transport(#[from] std::io::Error),

    /// The incoming frame's length field exceeds [`crate::transport::frame::MAX_FRAME_BYTES`].
    ///
    /// Either the sender is misbehaving or the connection is corrupted.
    /// The transport is considered unrecoverable after this error.
    #[error("IPC frame too large: {len} bytes (limit {limit})")]
    FrameTooLarge {
        /// Byte length advertised by the sender.
        len: u32,
        /// Hard cap enforced by the receiver.
        limit: u32,
    },

    /// The rkyv archive failed `bytecheck` validation.
    ///
    /// This is the first line of defense against a compromised peer sending
    /// a malformed byte stream.  The inner string carries the validator's
    /// diagnostic message.
    #[error("IPC archive validation failed: {0}")]
    Validation(String),

    /// A shared memory region could not be created, mapped, or sealed.
    #[error("IPC shared memory error: {0}")]
    Shm(String),

    /// The worker process terminated unexpectedly.
    ///
    /// The optional `i32` is the exit code if the OS provided one.
    #[error("worker process terminated unexpectedly (exit code: {0:?})")]
    WorkerDied(Option<i32>),

    /// The worker sent a message that is semantically invalid in the current
    /// protocol state (e.g., a `WireWorkerEnvelope` without a prior event).
    #[error("IPC protocol violation: {0}")]
    Protocol(String),

    /// The worker process's OS-level sandbox (seccomp-bpf, Job Object +
    /// integrity level, or App Sandbox) could not be installed.
    ///
    /// This is always treated as fatal: a worker that cannot confirm its own
    /// confinement must never proceed to process untrusted document logic.
    #[error("IPC sandbox initialization failed: {0}")]
    Sandbox(String),
}
