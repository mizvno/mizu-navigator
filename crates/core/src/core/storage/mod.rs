//! # `storage` — Encrypted Local Storage for Mizu Apps
//!
//! Provides AES-256-GCM encrypted persistence under `%APPDATA%\mizu\storage\`.
//!
//! ## Design
//!
//! * Each app domain gets its own file: `{APPDATA}\mizu\storage\{sha256_hex}.enc`
//!   The filename is the lowercase SHA-256 hex digest of the normalised domain.
//! * The 256-bit encryption master key is generated once per domain and stored in the OS
//!   keyring (service `mizu_storage`, user = SHA-256 hex of normalised domain).
//! * Uses `redb` as an embedded key-value store for O(1) mutations.
//! * Every record (variable) is encrypted with a unique key derived via HKDF-SHA256
//!   from the domain master key and the variable name.
//! * Record format: `nonce (12 bytes) || AES-GCM ciphertext`.
//! * The plaintext is the `serde_json` serialization of a `crate::core::types::Value`.
//! * (RM-10) The domain master key and every derived key are held in
//!   `Zeroizing<[u8; 32]>`, so they are scrubbed from memory as soon as
//!   they're dropped instead of lingering (swap, core dumps, debugger
//!   access) — this matters most for `StorageEngine::master_key`, which is
//!   cached and kept alive for the life of the process by `StoragePool`.
//! * `MIZU_MASTER_KEY` (read by [`derive_or_create_key`]) is a headless/
//!   recovery **break-glass** mechanism, not a supported production
//!   key-management path: it bypasses the OS keyring entirely, so the key
//!   is exposed to anything that can read this process's environment
//!   (`/proc/<pid>/environ`, child-process inheritance, crash dumps). Every
//!   use of it is logged with `tracing::warn!` for exactly that reason.
//!
//! Split by concern: [`domain`] (`ValidatedDomain`, storage path), [`keys`]
//! (master/record key derivation), [`crypto`] (`encrypt_record`/
//! `decrypt_record`), [`engine`] (`StorageEngine`, `open_db`,
//! `read_storage`), and [`pool`] (`StoragePool`, the process-lifetime engine
//! cache).

#![forbid(unsafe_code)]

mod crypto;
mod domain;
mod engine;
mod keys;
mod pool;
#[cfg(test)]
mod tests;

pub use domain::{ValidatedDomain, mizu_storage_path};
pub use engine::{STORAGE_TABLE, StorageEngine, open_db, read_storage};
pub use keys::{derive_or_create_key, derive_record_key};
pub use pool::StoragePool;
