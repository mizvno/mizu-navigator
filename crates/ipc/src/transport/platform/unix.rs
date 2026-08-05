//! # `transport::platform::unix` — UNIX domain socket IPC transport
//!
//! Provides [`UnixIpcStream`], a thin wrapper around a
//! [`std::os::unix::net::UnixStream`] that implements the
//! [`super::DuplexStream`] trait required by
//! [`crate::transport::channel::ipc_channel_pair`].
//!
//! ## Socket lifecycle
//!
//! * **Broker side**: calls [`UnixIpcStream::bind_and_accept`] to create a
//!   socket at a unique path (e.g., `/tmp/mizu-worker-<pid>.sock`), spawns
//!   the worker process passing that path as an argument, then blocks in
//!   `accept()` until the worker connects.
//! * **Worker side**: calls [`UnixIpcStream::connect`] with the path
//!   received from `argv[1]`.
//!
//! The socket file is unlinked by the broker after the worker connects,
//! so it is never visible to other processes for longer than the connection
//! setup window.
//!
//! ## `SCM_RIGHTS` (ancillary data for SHM handles)
//!
//! Shared memory file descriptors are passed separately via `sendmsg` /
//! `recvmsg` with `SCM_RIGHTS`.  That functionality lives in `shm::unix`
//! and is not exposed here.  `UnixIpcStream` is purely for the message
//! channel.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use crate::error::IpcError;
use super::DuplexStream;

/// A UNIX domain socket stream for use as the IPC transport.
pub struct UnixIpcStream {
    stream: UnixStream,
}

impl UnixIpcStream {
    /// **Broker side**: bind a listener at `socket_path`, accept one
    /// connection, unlink the socket file, and return the stream.
    ///
    /// Blocks until the worker connects.  This function is designed to be
    /// called *after* the worker process has been spawned so the race window
    /// is minimised.
    pub fn bind_and_accept(socket_path: &Path) -> Result<Self, IpcError> {
        let listener = UnixListener::bind(socket_path)?;
        let (stream, _addr) = listener.accept()?;
        // Unlink immediately: the connection is established, no other process
        // should be able to connect.
        let _ = std::fs::remove_file(socket_path);
        Ok(UnixIpcStream { stream })
    }

    /// The raw fd backing this stream.
    ///
    /// Needed by [`crate::sandbox::linux`], whose `read`/`write` rules are
    /// fd-equality conditions: the filter can only name the IPC endpoint if
    /// the worker can read its fd number back out of the stream first.
    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.stream.as_raw_fd()
    }

    /// **Broker side**: accept one connection on an already-bound listener.
    ///
    /// Separate from [`bind_and_accept`](Self::bind_and_accept) because the
    /// spawner must bind *before* launching the worker (so the child cannot
    /// lose a connect race) but can only block in `accept` *after*.
    pub fn from_listener(listener: UnixListener) -> Result<Self, IpcError> {
        let (stream, _addr) = listener.accept()?;
        Ok(UnixIpcStream { stream })
    }

    /// **Worker side**: connect to `socket_path` and return the stream.
    pub fn connect(socket_path: &Path) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(socket_path)?;
        Ok(UnixIpcStream { stream })
    }
}

impl Read for UnixIpcStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for UnixIpcStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// Read half of a `UnixIpcStream` (backed by a `dup`-ed file descriptor).
pub struct UnixReadHalf(UnixStream);
/// Write half of a `UnixIpcStream` (backed by a `dup`-ed file descriptor).
pub struct UnixWriteHalf(UnixStream);

impl Read for UnixReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for UnixWriteHalf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl DuplexStream for UnixIpcStream {
    type ReadHalf  = UnixReadHalf;
    type WriteHalf = UnixWriteHalf;

    fn split(self) -> Result<(Self::WriteHalf, Self::ReadHalf), IpcError> {
        // `try_clone` calls `dup(2)` — the cloned fd shares the same socket
        // but can be used independently from a different thread.
        let read_stream  = self.stream.try_clone()?;
        let write_stream = self.stream;
        Ok((UnixWriteHalf(write_stream), UnixReadHalf(read_stream)))
    }
}
