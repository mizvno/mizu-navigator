//! Framer unit and property-based tests.
//!
//! These are integration-level tests that exercise `write_frame` / `read_frame`
//! through the public API, plus a proptest that feeds random byte sequences to
//! the receiver to verify it never panics (only returns errors).

use mizu_ipc::transport::frame::{read_frame, write_frame, MAX_FRAME_BYTES};
use mizu_ipc::error::IpcError;
use std::io::Cursor;

// ── Adversarial fuzz ──────────────────────────────────────────────────────────

use proptest::prelude::*;

proptest! {
    /// Feed arbitrary byte sequences to `read_frame`.
    /// The invariant: it must never panic — only return `Ok` or a recognised
    /// `IpcError` variant.
    #[test]
    fn read_frame_never_panics(bytes: Vec<u8>) {
        let result = read_frame(&mut Cursor::new(&bytes));
        match result {
            Ok(_)
            | Err(IpcError::WorkerDied(_))
            | Err(IpcError::FrameTooLarge { .. })
            | Err(IpcError::Transport(_)) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Any payload that fits in MAX_FRAME_BYTES survives a write→read cycle.
    #[test]
    fn write_then_read_round_trips(payload in proptest::collection::vec(any::<u8>(), 0..=65536usize)) {
        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).expect("write");
        let recovered = read_frame(&mut Cursor::new(&buf)).expect("read");
        prop_assert_eq!(recovered, payload);
    }
}

// ── Exact-boundary tests ──────────────────────────────────────────────────────

#[test]
fn exactly_max_frame_bytes_is_accepted() {
    let payload = vec![0u8; MAX_FRAME_BYTES as usize];
    let mut buf = Vec::new();
    write_frame(&mut buf, &payload).expect("write at exactly MAX_FRAME_BYTES");
    let rt = read_frame(&mut Cursor::new(&buf)).expect("read at exactly MAX_FRAME_BYTES");
    assert_eq!(rt.len(), MAX_FRAME_BYTES as usize);
}

#[test]
fn one_over_max_frame_bytes_is_rejected_on_write() {
    // We cannot actually allocate 64 MiB + 1 in a unit test on CI easily,
    // so we craft a fake length header directly and verify the read side.
    let crafted_len = MAX_FRAME_BYTES + 1;
    let mut buf = crafted_len.to_le_bytes().to_vec();
    buf.extend_from_slice(&[0u8; 8]); // some body bytes (read never happens)
    let err = read_frame(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(err, IpcError::FrameTooLarge { len, limit }
            if len == crafted_len && limit == MAX_FRAME_BYTES),
        "expected FrameTooLarge, got {err:?}",
    );
}
