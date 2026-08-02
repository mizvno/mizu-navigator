//! At-rest encryption for the persisted history log: the AEAD envelope
//! format and OS-keyring-backed key management.

use super::platform::*;

// ── AEAD envelope ─────────────────────────────────────────────────────────────

/// Length of the AES-GCM nonce prefixed to every stored blob.
pub(super) const NONCE_LEN: usize = 12;
/// Length of the AES-GCM authentication tag appended by `encrypt`.
const TAG_LEN: usize = 16;

/// Seals `plaintext` under `key` as `nonce || ciphertext || tag`.
///
/// A fresh 96-bit nonce is drawn from the OS CSPRNG on every call: reusing a
/// nonce under one key is the single fatal mistake in GCM, and saving the
/// same log twice must never produce the same blob.
pub(super) fn encrypt_blob(key: &[u8; 32], plaintext: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::{
        Aes256Gcm, Key, Nonce,
        aead::{Aead, KeyInit},
    };
    use rand_chacha::rand_core::{RngCore, SeedableRng};

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand_chacha::ChaCha20Rng::from_entropy().fill_bytes(&mut nonce_bytes);

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .ok()?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Some(blob)
}

/// Opens a blob produced by [`encrypt_blob`], returning `None` when it is
/// too short, was sealed under a different key, or has been modified.
pub(super) fn decrypt_blob(key: &[u8; 32], blob: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::{
        Aes256Gcm, Key, Nonce,
        aead::{Aead, KeyInit},
    };
    if blob.len() < NONCE_LEN + TAG_LEN {
        return None;
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .ok()
}

// ── Encryption key management ─────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
const KEYRING_SERVICE: &str = "mizu-navigator";
#[cfg(not(target_os = "windows"))]
const KEYRING_HISTORY_KEY: &str = "history-encryption-key";

/// Returns the AES-256-GCM history encryption key, loading it from the OS
/// keyring if it already exists or generating and storing a new one on first
/// run.
///
/// Uses `ChaCha20Rng::from_entropy()` for key generation — this seeds from
/// the OS CSPRNG (via `getrandom`), the same source the OS keyring itself
/// uses for its own key material.
///
/// Returns `None` when the keyring is unavailable (e.g. headless CI or a
/// locked session); the caller must treat this as "skip persistence".
#[cfg(target_os = "windows")]
pub(super) fn get_or_create_history_key() -> Option<[u8; 32]> {
    let path = data_dir().join("history_key.bin");

    // Try to load an existing key.
    if let Ok(blob) = std::fs::read(&path) {
        match windows_dpapi::decrypt_data(&blob, windows_dpapi::Scope::User, None) {
            Ok(key) => {
                if key.len() == 32 {
                    let mut k = [0u8; 32];
                    k.copy_from_slice(&key);
                    return Some(k);
                }
                tracing::warn!("history: DPAPI key was malformed; regenerating");
            }
            Err(e) => {
                tracing::warn!(error = %e, "history: DPAPI decryption failed; regenerating");
            }
        }
    }

    // Generate a fresh 256-bit key from the OS CSPRNG.
    use rand_chacha::rand_core::{RngCore, SeedableRng};
    let mut rng = rand_chacha::ChaCha20Rng::from_entropy();
    let mut key = [0u8; 32];
    rng.fill_bytes(&mut key);

    match windows_dpapi::encrypt_data(&key, windows_dpapi::Scope::User, None) {
        Ok(blob) => {
            if let Err(e) = std::fs::write(&path, blob) {
                tracing::warn!(error = %e, "history: failed to write DPAPI key to disk; history will not persist this session");
                return None;
            }
            tracing::debug!("history: new DPAPI encryption key generated and stored");
            Some(key)
        }
        Err(e) => {
            tracing::warn!(error = %e, "history: DPAPI encryption failed; history will not persist this session");
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(super) fn get_or_create_history_key() -> Option<[u8; 32]> {
    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_HISTORY_KEY) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "history: keyring entry creation failed; history will not persist");
            return None;
        }
    };

    // Try to load an existing key.
    match entry.get_password() {
        Ok(hex) => {
            if let Some(key) = hex_to_key32(&hex) {
                return Some(key);
            }
            tracing::warn!("history: keyring key was malformed; regenerating");
        }
        Err(keyring::Error::NoEntry) => {
            // Expected on first run — fall through to generate.
        }
        Err(e) => {
            tracing::warn!(error = %e, "history: keyring read failed; history will not persist");
            return None;
        }
    }

    // Generate a fresh 256-bit key from the OS CSPRNG.
    use rand_chacha::rand_core::{RngCore, SeedableRng};
    let mut rng = rand_chacha::ChaCha20Rng::from_entropy();
    let mut key = [0u8; 32];
    rng.fill_bytes(&mut key);

    if let Err(e) = entry.set_password(&key_to_hex32(&key)) {
        tracing::warn!(error = %e, "history: failed to persist key in keyring; history will not persist this session");
        return None;
    }
    tracing::debug!("history: new encryption key generated and stored in keyring");
    Some(key)
}

/// Encodes a 32-byte key as a 64-character lowercase hex string.
/// Zero-dependency alternative to the `hex` crate.
#[cfg(not(target_os = "windows"))]
pub(super) fn key_to_hex32(key: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in key {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

/// Decodes a 64-character lowercase hex string into a 32-byte key.
/// Returns `None` on length or character errors.
#[cfg(not(target_os = "windows"))]
pub(super) fn hex_to_key32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

#[cfg(not(target_os = "windows"))]
pub(super) fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
