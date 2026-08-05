//! # `transport` — IPC channel framer and typed sender/receiver
//!
//! The transport layer converts rkyv archives into length-prefixed byte frames
//! and sends them over the platform-native IPC stream (UNIX domain socket on
//! Linux/macOS, Named Pipe on Windows).
//!
//! ## Sub-modules
//!
//! * [`frame`] — raw byte framing: `write_frame` / `read_frame`.
//! * [`channel`] — typed, generic wrappers: `IpcSender<T>` / `IpcReceiver<T>`.
//! * [`platform`] — OS-specific transport streams.

pub mod channel;
pub mod frame;
pub mod platform;
