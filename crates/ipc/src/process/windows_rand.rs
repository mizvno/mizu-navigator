//! Cryptographic entropy for the rendezvous name and handshake token.
//!
//! Uses `BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG`, the
//! supported CNG entry point for random bytes on Windows (`RtlGenRandom` /
//! `CryptGenRandom` are the deprecated predecessors). A userspace PRNG would
//! not do here: the token is the only thing distinguishing our child from a
//! same-user process that guessed the pipe name.
//!
//! ## Safety
//!
//! One FFI call, writing into a caller-owned slice whose length is passed
//! alongside it. The `NTSTATUS` return is checked before the buffer is
//! treated as initialized.

#![allow(unsafe_code)]

use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

use crate::error::IpcError;

/// Fills `buf` with cryptographically secure random bytes.
pub fn fill(buf: &mut [u8]) -> Result<(), IpcError> {
    // SAFETY: `buf.as_mut_ptr()` is valid for `buf.len()` bytes by
    // construction. A null algorithm handle is required (not merely allowed)
    // when `BCRYPT_USE_SYSTEM_PREFERRED_RNG` is set, per the CNG contract.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(IpcError::Sandbox(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{status:08x}"
        )));
    }
    Ok(())
}
