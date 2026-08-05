//! # `transport::channel` — Typed `IpcSender<T>` and `IpcReceiver<T>`
//!
//! These generic wrappers sit on top of the raw byte framer and provide a
//! type-safe, rkyv-based IPC channel.
//!
//! ## Usage pattern (broker side — sending events, receiving responses)
//!
//! ```rust,ignore
//! let stream = platform::connect_or_listen()?;
//! let (mut tx, mut rx) = ipc_channel_pair::<WireUiEvent, WireWorkerEnvelope>(stream)?;
//!
//! tx.send(&WireUiEvent::Click { node_id: 42 })?;
//! let envelope = rx.recv()?;
//! ```
//!
//! ## Archive validation
//!
//! `IpcReceiver::recv` calls [`rkyv::access`] with `bytecheck` validation
//! before deserializing.  This means a malformed byte stream from a
//! compromised peer produces `IpcError::Validation`, never undefined behavior.
//!
//! ## Buffering
//!
//! Both sender and receiver wrap the underlying stream in
//! `BufReader`/`BufWriter` to batch small writes and avoid per-message
//! system-call overhead.

#![forbid(unsafe_code)]

use std::io::{BufReader, BufWriter, Read, Write};

use rkyv::api::high::{HighDeserializer, HighSerializer};
use rkyv::rancor::Error as RkyvError;
use rkyv::{Archive, Deserialize, Serialize};
use rkyv::ser::allocator::ArenaHandle;
use rkyv::util::AlignedVec;

use crate::error::IpcError;
use crate::transport::frame::{read_frame, write_frame};

// ── Sender ───────────────────────────────────────────────────────────────────

/// Typed sending half of an IPC channel.
pub struct IpcSender<T> {
    inner: BufWriter<Box<dyn Write + Send>>,
    _marker: std::marker::PhantomData<fn(T)>,
}

impl<T> IpcSender<T>
where
    T: for<'a> Serialize<HighSerializer<AlignedVec, ArenaHandle<'a>, RkyvError>>,
{
    /// Wrap a writable stream in a typed sender.
    pub fn new(stream: impl Write + Send + 'static) -> Self {
        IpcSender {
            inner: BufWriter::new(Box::new(stream)),
            _marker: std::marker::PhantomData,
        }
    }

    /// Serialize `msg` with rkyv and write it as a length-prefixed frame.
    pub fn send(&mut self, msg: &T) -> Result<(), IpcError> {
        let bytes = rkyv::to_bytes::<RkyvError>(msg)
            .map_err(|e| IpcError::Validation(e.to_string()))?;
        write_frame(&mut self.inner, &bytes)
    }
}

impl<T> IpcSender<T> {
    /// Closes the underlying stream, so the peer's next read sees EOF.
    ///
    /// This is the outbound half of the shutdown protocol: the worker treats
    /// EOF as "the broker is gone" and exits. Replacing the stream with a
    /// sink (rather than exposing `Option`) keeps every other method
    /// infallible in the same way it was before the close; subsequent sends
    /// are silently discarded, which is correct — there is no longer anyone
    /// to receive them.
    pub fn close(&mut self) {
        self.inner = BufWriter::new(Box::new(std::io::sink()));
    }
}

// ── Receiver ─────────────────────────────────────────────────────────────────

/// Typed receiving half of an IPC channel.
pub struct IpcReceiver<T> {
    inner: BufReader<Box<dyn Read + Send>>,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> IpcReceiver<T>
where
    T: Archive,
    T::Archived: for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>
        + Deserialize<T, HighDeserializer<RkyvError>>,
{
    /// Wrap a readable stream in a typed receiver.
    pub fn new(stream: impl Read + Send + 'static) -> Self {
        IpcReceiver {
            inner: BufReader::new(Box::new(stream)),
            _marker: std::marker::PhantomData,
        }
    }

    /// Block until one frame arrives, validate its archive, and return an
    /// owned `T`.
    pub fn recv(&mut self) -> Result<T, IpcError> {
        let bytes = read_frame(&mut self.inner)?;
        let archived = rkyv::access::<T::Archived, RkyvError>(&bytes)
            .map_err(|e| IpcError::Validation(e.to_string()))?;
        rkyv::deserialize::<T, RkyvError>(archived)
            .map_err(|e| IpcError::Validation(e.to_string()))
    }
}

impl<T> IpcReceiver<T> {
    /// Closes the underlying stream. Counterpart to [`IpcSender::close`];
    /// subsequent `recv` calls report EOF as [`IpcError::WorkerDied`].
    pub fn close(&mut self) {
        self.inner = BufReader::new(Box::new(std::io::empty()));
    }
}

// ── Convenience constructor ───────────────────────────────────────────────────

/// Split a bidirectional stream into a `(IpcSender<S>, IpcReceiver<R>)` pair.
pub fn ipc_channel_pair<S, R, Stream>(
    stream: Stream,
) -> Result<(IpcSender<S>, IpcReceiver<R>), IpcError>
where
    Stream: crate::transport::platform::DuplexStream,
    S: for<'a> Serialize<HighSerializer<AlignedVec, ArenaHandle<'a>, RkyvError>>,
    R: Archive,
    R::Archived: for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>
        + Deserialize<R, HighDeserializer<RkyvError>>,
{
    let (write_half, read_half) = stream.split()?;
    Ok((IpcSender::new(write_half), IpcReceiver::new(read_half)))
}
