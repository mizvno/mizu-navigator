//! `encrypt_record`/`decrypt_record`: AES-256-GCM record encryption using
//! a per-record key derived via `derive_record_key`.

use aes_gcm::aead::{Aead, AeadInPlace, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand_core::RngCore;

use crate::core::errors::MizuError;

use super::keys::derive_record_key;

/// Encrypts `plaintext` with AES-256-GCM using a record-specific key and returns `nonce || ciphertext`.
///
/// Nonces are drawn from `OsRng`. This is safe against AES-GCM's catastrophic
/// nonce-reuse failure mode because that failure mode is reuse *under the same key*,
/// and `derive_record_key` derives a distinct 32-byte key per record (via HKDF-SHA256,
/// keyed on `variable_name`) — nonce reuse across *different* keys carries no such risk.
pub fn encrypt_record(
    master_key: &[u8; 32],
    variable_name: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, MizuError> {
    let key = derive_record_key(master_key, variable_name)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_ref()));
    drop(key); // record key is single-use; scrub it now instead of at function end.
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Single allocation for nonce || plaintext, encrypted in place below —
    // encrypt_in_place_detached operates on a plain &mut [u8] (unlike
    // encrypt_in_place, which needs a growable aead::Buffer), so the
    // plaintext region can be encrypted in place inside `out` instead of
    // allocating a separate ciphertext Vec first and copying it in.
    let mut out = Vec::with_capacity(12 + plaintext.len() + 16);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(plaintext);

    let tag = cipher
        .encrypt_in_place_detached(nonce, b"", &mut out[12..])
        .map_err(|e| MizuError::ExecutionError(format!("AES-GCM encrypt: {e}")))?;
    out.extend_from_slice(&tag);

    Ok(out)
}

/// Decrypts a blob produced by `encrypt_record`.
pub fn decrypt_record(
    master_key: &[u8; 32],
    variable_name: &str,
    blob: &[u8],
) -> Result<Vec<u8>, MizuError> {
    if blob.len() < 12 {
        return Err(MizuError::ExecutionError(
            "storage blob too short (missing nonce)".to_owned(),
        ));
    }
    let key = derive_record_key(master_key, variable_name)?;
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_ref()));
    drop(key); // record key is single-use; scrub it now instead of at function end.
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| MizuError::ExecutionError(format!("AES-GCM decrypt: {e}")))
}
