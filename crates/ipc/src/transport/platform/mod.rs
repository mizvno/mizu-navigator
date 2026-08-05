//! # `transport::platform` — OS-specific IPC stream implementations
//!
//! This module provides the `DuplexStream` trait and its platform-specific
//! implementations:
//!
//! * `unix` — UNIX domain sockets (Linux, macOS).
//! * `windows` — Named Pipes.
//!
//! Both implementations expose `std::io::Read + std::io::Write` so that the
//! generic framer and channel types can work with either transport without
//! conditional compilation at the call site.


use crate::error::IpcError;
use std::io::{Read, Write};

/// A bidirectional byte stream that can be split into independent read and
/// write halves for use by [`crate::transport::channel::ipc_channel_pair`].
pub trait DuplexStream: Sized {
    /// The read half type.
    type ReadHalf: Read + Send + 'static;
    /// The write half type.
    type WriteHalf: Write + Send + 'static;

    /// Split the stream into independent read and write halves.
    ///
    /// On UNIX this `dup(2)`s the underlying file descriptor.
    /// On Windows it duplicates the handle.
    fn split(self) -> Result<(Self::WriteHalf, Self::ReadHalf), IpcError>;
}

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

// Re-export the platform-native stream type under a single name so callers
// don't need cfg guards.
#[cfg(unix)]
pub use unix::UnixIpcStream;
#[cfg(windows)]
pub use windows::NamedPipeStream;
