//! # `transport::frame` — Length-prefix framing over a byte stream
//!
//! ## Wire format
//!
//! ```text
//! ┌──────────────────┬────────────────────────────────────┐
//! │  length: u32 LE  │  body: [u8; length]  (rkyv bytes)  │
//! │    (4 bytes)     │                                     │
//! └──────────────────┴────────────────────────────────────┘
//! ```
//!
//! The receiver reads the 4-byte header, validates it against
//! `MAX_FRAME_BYTES`, allocates a buffer of exactly `length` bytes, then
//! reads the body.  The body is a raw rkyv archive; validation is left to
//! the caller (see [`crate::transport::channel`]).
//!
//! ## Limits
//!
//! `MAX_FRAME_BYTES` (64 MiB) is a hard cap enforced on both sides:
//! * The sender returns `IpcError::FrameTooLarge` if the serialized archive
//!   exceeds this limit.
//! * The receiver returns the same error if the incoming length header
//!   exceeds this limit — this prevents a malicious sender from causing an
//!   out-of-memory allocation.
//!
//! The 64 MiB cap is intentionally generous because `WireReloadPayload`
//! (which is large) never travels over the socket — it goes through shared
//! memory.  The socket carries only `WireUiEvent` and `WireWorkerEnvelope`,
//! both of which are small (typically ≪ 1 MiB).  The cap exists as a safety
//! net, not a practical bound.

#![forbid(unsafe_code)]

use std::io::{Read, Write};

use crate::error::IpcError;

/// Hard upper bound on a single frame's body length.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024; // 64 MiB

/// Serialize `body` and write it as a length-prefixed frame to `writer`.
///
/// `body` must already be the rkyv-serialized bytes of the message.
///
/// # Errors
///
/// * [`IpcError::FrameTooLarge`] if `body.len() > MAX_FRAME_BYTES`.
/// * [`IpcError::Transport`] if the underlying write fails.
pub fn write_frame<W: Write>(writer: &mut W, body: &[u8]) -> Result<(), IpcError> {
    let len = u32::try_from(body.len()).ok().filter(|&n| n <= MAX_FRAME_BYTES)
        .ok_or(IpcError::FrameTooLarge {
            len:   body.len() as u32,
            limit: MAX_FRAME_BYTES,
        })?;

    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

/// Read one length-prefixed frame from `reader` and return its body bytes.
///
/// Blocks until a complete frame is available or the stream ends.
///
/// # Errors
///
/// * [`IpcError::WorkerDied`]`(None)` on clean EOF (peer closed the stream).
/// * [`IpcError::FrameTooLarge`] if the advertised length exceeds `MAX_FRAME_BYTES`.
/// * [`IpcError::Transport`] on any other I/O error.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, IpcError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(IpcError::WorkerDied(None));
        }
        Err(e) => return Err(IpcError::Transport(e)),
    }

    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge { len, limit: MAX_FRAME_BYTES });
    }

    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            IpcError::WorkerDied(None)
        } else {
            IpcError::Transport(e)
        }
    })?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_empty_body() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &[]).unwrap();
        let body = read_frame(&mut Cursor::new(&buf)).unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn round_trip_small_body() {
        let payload = b"hello, mizu-ipc";
        let mut buf = Vec::new();
        write_frame(&mut buf, payload).unwrap();
        let body = read_frame(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(body, payload);
    }

    #[test]
    fn frame_too_large_write() {
        // Construct an oversized slice reference without allocating 64 MiB.
        // We fake `body.len()` by passing a small slice and overriding the
        // length check — instead, just test with a fresh Vec.
        let big: Vec<u8> = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        let mut buf = Vec::new();
        let err = write_frame(&mut buf, &big).unwrap_err();
        assert!(matches!(err, IpcError::FrameTooLarge { .. }));
    }

    #[test]
    fn frame_too_large_read() {
        // Craft a header advertising MAX+1 bytes.
        let len_bytes = (MAX_FRAME_BYTES + 1).to_le_bytes();
        let err = read_frame(&mut Cursor::new(len_bytes)).unwrap_err();
        assert!(matches!(err, IpcError::FrameTooLarge { .. }));
    }

    #[test]
    fn eof_on_empty_stream() {
        let err = read_frame(&mut Cursor::new(&[])).unwrap_err();
        assert!(matches!(err, IpcError::WorkerDied(None)));
    }

    #[test]
    fn multiple_frames_sequential() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"frame-1").unwrap();
        write_frame(&mut buf, b"frame-2").unwrap();
        let mut cur = Cursor::new(&buf);
        assert_eq!(read_frame(&mut cur).unwrap(), b"frame-1");
        assert_eq!(read_frame(&mut cur).unwrap(), b"frame-2");
        assert!(matches!(read_frame(&mut cur).unwrap_err(), IpcError::WorkerDied(None)));
    }
}
