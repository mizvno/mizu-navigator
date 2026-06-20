//! # `storage` — Encrypted Local Storage for Mizu Apps
//!
//! Provides AES-256-GCM encrypted persistence under `%APPDATA%\mizu\storage\`.
//!
//! ## Design
//!
//! * Each app domain gets its own file: `{APPDATA}\mizu\storage\{sha256_hex}.enc`
//!   The filename is the lowercase SHA-256 hex digest of the normalised domain
//!   (trim + lowercase), giving a fixed-length, filesystem-safe, collision-free
//!   identifier with no lossy character substitution and no path-traversal risk.
//! * The 256-bit encryption key is generated once per domain and stored in the OS
//!   keyring (service `mizu_storage`, user = SHA-256 hex of normalised domain).
//! * File layout: `nonce (12 bytes) || AES-GCM ciphertext`
//! * The plaintext is a JSON map `{ "key": "value", ... }`.
//!
//! ## Threat Model
//!
//! This protects against offline disk reads but NOT against a compromised OS
//! account (the key lives in the same keyring).  It satisfies the requirement to
//! avoid plain-text secrets sitting in `CWD`.
//!
//! ## Multi-Tenant Isolation
//!
//! [`ValidatedDomain`] is a newtype that acts as a proof-of-validation token.
//! Functions that operate on per-domain resources (`mizu_storage_path`,
//! `derive_or_create_key`, `read_storage`, `write_storage`) accept
//! `&ValidatedDomain` rather than a bare `&str`, making it impossible at the
//! type-system level to accidentally pass a raw, un-hashed domain string.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

use crate::core::errors::MizuError;
use crate::core::types::{Value, from_json, to_json};


/// A validated, opaque domain identifier whose inner value is the lowercase
/// SHA-256 hex digest of the normalised raw domain string.
///
/// ## Construction
///
/// Use [`ValidatedDomain::from_raw`].  There is intentionally no `From<String>`
/// or `From<&str>` impl — every construction site must go through the canonical
/// normalisation + hashing pipeline.
///
/// ## Guarantees
///
/// * **Deterministic** — equal raw domains always produce equal `ValidatedDomain`s.
/// * **Filesystem-safe** — the inner string is 64 lowercase hex characters
///   (`[0-9a-f]{64}`), safe for use as a filename on every major OS without
///   any further escaping.
/// * **Collision-resistant** — backed by SHA-256 (256-bit pre-image resistance).
/// * **Isolation** — two distinct normalised domains can never share the same
///   digest, preventing cross-tenant data access.
pub struct ValidatedDomain(String);

impl ValidatedDomain {
    /// Normalises `domain` (trim whitespace + lowercase) and returns a
    /// [`ValidatedDomain`] whose inner value is the lowercase SHA-256 hex
    /// digest of the normalised string.
    ///
    /// # Example
    ///
    /// ```
    /// use mizu::core::storage::ValidatedDomain;
    ///
    /// let a = ValidatedDomain::from_raw("  Example.COM  ");
    /// let b = ValidatedDomain::from_raw("example.com");
    /// assert_eq!(a.as_str(), b.as_str()); // same normalisation → same digest
    /// ```
    pub fn from_raw(domain: &str) -> Self {
        let normalised = domain.trim().to_lowercase();
        let mut hasher = Sha256::new();
        hasher.update(normalised.as_bytes());
        let digest = hasher.finalize();
        ValidatedDomain(hex::encode(digest))
    }

    /// Returns the inner SHA-256 hex digest string (64 lowercase hex chars).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}


/// Returns the path where the encrypted storage file for `domain` will live.
///
/// | Platform | Base path |
/// |----------|-----------|
/// | **Windows** | `%APPDATA%\mizu\storage\` |
/// | **Unix / Linux / macOS** | `$XDG_DATA_HOME/mizu/storage/` → falls back to `$HOME/.local/share/mizu/storage/` (XDG Base Directory Specification) |
/// | **Other** | `./mizu_storage/mizu/storage/` (relative fallback) |
///
/// The final filename is always `{sha256_hex}.enc`.
pub fn mizu_storage_path(domain: &ValidatedDomain) -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./mizu_storage"));

    #[cfg(unix)]
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|home| PathBuf::from(home).join(".local").join("share"))
                .unwrap_or_else(|_| PathBuf::from("./mizu_storage"))
        });

    #[cfg(not(any(windows, unix)))]
    let base = PathBuf::from("./mizu_storage");

    let dir = base.join("mizu").join("storage");
    let filename = format!("{}.enc", domain.as_str());
    dir.join(filename)
}


const KEYRING_SERVICE: &str = "mizu_storage";

/// Fail-secure environment integrity check for key generation.
///
/// If a `.enc` storage file already exists on disk but the OS keyring entry is
/// absent (possible causes: OS update, credential store reset, profile migration,
/// temporary keyring unavailability), generating a *new* random key would cause
/// the next `write_storage` call to encrypt future data with the new key and
/// atomically overwrite the existing ciphertext — an **unrecoverable data-loss
/// event** with no recovery path.
///
/// This function aborts before any key generation and returns a descriptive
/// [`MizuError::ExecutionError`] that instructs the operator to either restore
/// the OS keyring entry or provide `MIZU_MASTER_KEY` for headless recovery.
/// It is deliberately a pure path-existence check with no keyring interaction so
/// it can be unit-tested without mocking the OS credential store.
pub(crate) fn fail_if_desync(storage_path: &std::path::Path) -> Result<(), MizuError> {
    if storage_path.exists() {
        return Err(MizuError::ExecutionError(
            "keyring integrity violation: a storage file exists for this domain but the \
             corresponding keyring entry is missing — environment integrity has been \
             compromised (possible OS update, credential reset, or profile migration). \
             Refusing to generate a new key: doing so would irrecoverably overwrite the \
             existing encrypted data. Restore the OS keyring entry or set \
             MIZU_MASTER_KEY to recover access."
                .to_owned(),
        ));
    }
    Ok(())
}

/// Decodes a 64-character lowercase hex string into a 32-byte AES-256 key.
/// Called by [`derive_or_create_key`] when the `MIZU_MASTER_KEY` env var is set.
/// Extracted as a pure function so it can be unit-tested without mutating the
/// process environment (which is `unsafe` in Rust ≥ 1.81).
fn parse_master_key_hex(hex: &str) -> Result<[u8; 32], MizuError> {
    let bytes = hex::decode(hex)
        .map_err(|e| MizuError::ExecutionError(format!("MIZU_MASTER_KEY decode: {e}")))?;
    bytes.try_into().map_err(|_| {
        MizuError::ExecutionError(
            "MIZU_MASTER_KEY must be exactly 32 bytes (64 hex chars)".to_owned(),
        )
    })
}

/// Derives a domain-specific AES-256 key from a shared master key.
///
/// `derived = HMAC-SHA256(master_key, domain_digest_bytes)` where
/// `domain_digest_bytes` is the UTF-8 encoding of the 64-character lowercase
/// SHA-256 hex of the normalised domain (i.e. `domain.as_str().as_bytes()`).
///
/// This ensures that each tenant receives a cryptographically distinct key even
/// when all tenants share the same `MIZU_MASTER_KEY`, preventing one compromised
/// app from decrypting another app's storage.
fn derive_domain_key(
    master_key: &[u8; 32],
    domain: &ValidatedDomain,
) -> Result<[u8; 32], MizuError> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(master_key)
        .map_err(|e| MizuError::ExecutionError(format!("HMAC init: {e}")))?;
    mac.update(domain.as_str().as_bytes());
    Ok(mac.finalize().into_bytes().into())
}

/// Loads the AES-256 key for `domain` from the OS keyring, creating and saving
/// a fresh 32-byte random key if none exists yet.
///
/// ## Headless / CI fallback
///
/// If the environment variable `MIZU_MASTER_KEY` is set, its value is decoded
/// as 64 lowercase hex characters (= 32 bytes) and used as the *master* key.
/// A per-domain derived key is then computed as `HMAC-SHA256(master, domain_digest)`
/// so each tenant retains its own distinct encryption key even in headless mode.
/// This replaces the previous (broken) behaviour of returning the master key
/// unchanged for every domain, which allowed any tenant to decrypt another's data.
pub fn derive_or_create_key(domain: &ValidatedDomain) -> Result<[u8; 32], MizuError> {
    // Env-var override — checked first so headless environments never
    // attempt a keyring connection that would return a hard error.
    if let Ok(hex) = std::env::var("MIZU_MASTER_KEY") {
        let master = parse_master_key_hex(&hex)?;
        return derive_domain_key(&master, domain);
    }

    let entry = match keyring::Entry::new(KEYRING_SERVICE, domain.as_str()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                "keyring unavailable ({}); set MIZU_MASTER_KEY for headless operation",
                e
            );
            return Err(MizuError::ExecutionError(format!("keyring open: {e}")));
        }
    };

    match entry.get_password() {
        Ok(hex) => {
            let bytes = hex::decode(&hex)
                .map_err(|e| MizuError::ExecutionError(format!("keyring key decode: {e}")))?;
            bytes
                .try_into()
                .map_err(|_| MizuError::ExecutionError("keyring key wrong length".to_owned()))
        }
        Err(keyring::Error::NoEntry) => {
            fail_if_desync(&mizu_storage_path(domain))?;
            let raw_key = Aes256Gcm::generate_key(OsRng);
            let hex_key = hex::encode(raw_key.as_slice());
            entry
                .set_password(&hex_key)
                .map_err(|e| MizuError::ExecutionError(format!("keyring save: {e}")))?;
            Ok(raw_key.into())
        }
        Err(e) => {
            tracing::warn!(
                "keyring read failed ({}); set MIZU_MASTER_KEY for headless operation",
                e
            );
            Err(MizuError::ExecutionError(format!("keyring read: {e}")))
        }
    }
}


/// Encrypts `plaintext` with AES-256-GCM and returns `nonce || ciphertext`.
pub fn encrypt_storage(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, MizuError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| MizuError::ExecutionError(format!("AES-GCM encrypt: {e}")))?;

    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts a blob produced by [`encrypt_storage`] (`nonce || ciphertext`).
pub fn decrypt_storage(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, MizuError> {
    if blob.len() < 12 {
        return Err(MizuError::ExecutionError(
            "storage blob too short (missing nonce)".to_owned(),
        ));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| MizuError::ExecutionError(format!("AES-GCM decrypt: {e}")))
}


/// Reads the encrypted JSON map for `domain`, returning an empty map if the
/// storage file does not exist yet.
///
/// The on-disk format is a JSON object whose values are full JSON
/// representations of [`Value`]s (not flat strings), so complex types
/// such as `Value::List` and `Value::Record` survive the round-trip
/// without information loss.
pub fn read_storage(domain: &ValidatedDomain) -> Result<HashMap<String, Value>, MizuError> {
    let path = mizu_storage_path(domain);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let blob = std::fs::read(&path)
        .map_err(|e| MizuError::ExecutionError(format!("storage read: {e}")))?;
    let key = derive_or_create_key(domain)?;
    let plaintext = decrypt_storage(&key, &blob)?;
    let json: serde_json::Value = serde_json::from_slice(&plaintext)
        .map_err(|e| MizuError::ExecutionError(format!("storage json decode: {e}")))?;
    match json {
        serde_json::Value::Object(map) => {
            Ok(map.into_iter().map(|(k, v)| (k, from_json(&v))).collect())
        }
        _ => Err(MizuError::ExecutionError(
            "storage json: expected top-level object".to_owned(),
        )),
    }
}

/// Writes `data` as an encrypted JSON map for `domain`.
///
/// Each [`Value`] is serialised via [`to_json`] before encryption, preserving
/// the full structural type information (lists, nested records, etc.).
///
/// Uses a write-then-rename pattern: the ciphertext is written to a `.tmp`
/// file in the same directory, then atomically renamed over the target.
/// A crash between the two steps leaves the original file intact.
pub fn write_storage(
    domain: &ValidatedDomain,
    data: &HashMap<String, Value>,
) -> Result<(), MizuError> {
    let path = mizu_storage_path(domain);
    let json_map: serde_json::Map<String, serde_json::Value> =
        data.iter().map(|(k, v)| (k.clone(), to_json(v))).collect();
    let plaintext = serde_json::to_vec(&serde_json::Value::Object(json_map))
        .map_err(|e| MizuError::ExecutionError(format!("storage json encode: {e}")))?;
    let key = derive_or_create_key(domain)?;
    let blob = encrypt_storage(&key, &plaintext)?;
    write_bytes_atomic(&path, &blob)
}

/// Atomically replaces `path` with `data` using a tmp-then-rename pattern.
///
/// The tmp file lives in the same directory as `path` so the rename stays on
/// the same filesystem (cross-device rename is not atomic).  If the rename
/// fails, the tmp file is removed on a best-effort basis before returning the
/// error.
///
/// ## Crash durability
///
/// After writing all bytes the file is explicitly `sync_all`'d before the
/// rename.  This ensures the kernel flushes both data and metadata to durable
/// storage so a power loss between write and rename never leaves a partial
/// file behind.
pub(crate) fn write_bytes_atomic(path: &std::path::Path, data: &[u8]) -> Result<(), MizuError> {
    let parent = path.parent().ok_or_else(|| {
        MizuError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "storage path has no parent directory",
        ))
    })?;
    std::fs::create_dir_all(parent)?;

    let tmp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("mizu_storage")
    ));

    let mut tmp_file = std::fs::File::create(&tmp_path)?;
    tmp_file.write_all(data).map_err(MizuError::IoError)?;
    tmp_file.sync_all().map_err(MizuError::IoError)?;
    // Drop before rename so Windows releases the exclusive lock.
    drop(tmp_file);

    if let Err(rename_err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);

        return Err(MizuError::IoError(rename_err));
    }

    // Flush the parent directory's metadata to durable storage.  On
    // journaling filesystems (ext4, XFS, APFS) a power loss immediately
    // after the rename can leave an unlinked directory entry; syncing the
    // directory handle ensures the rename is durably committed.
    // On Windows, NTFS provides this guarantee on the rename path without
    // an explicit directory sync, so the call is skipped there.
    #[cfg(unix)]
    {
        let dir_file = std::fs::File::open(parent).map_err(MizuError::IoError)?;
        dir_file.sync_all().map_err(MizuError::IoError)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_domain_normalises_case_and_whitespace() {
        let a = ValidatedDomain::from_raw("  Example.COM  ");
        let b = ValidatedDomain::from_raw("example.com");
        assert_eq!(
            a.as_str(),
            b.as_str(),
            "same normalised form must yield the same digest"
        );
    }

    #[test]
    fn validated_domain_digest_is_hex_64_chars() {
        let d = ValidatedDomain::from_raw("example.com");
        assert_eq!(d.as_str().len(), 64, "SHA-256 hex digest must be 64 chars");
        assert!(
            d.as_str().chars().all(|c| c.is_ascii_hexdigit()),
            "digest must be all hex digits"
        );
    }

    #[test]
    fn validated_domain_distinct_inputs_yield_distinct_digests() {
        let a = ValidatedDomain::from_raw("app-a.mizu");
        let b = ValidatedDomain::from_raw("app-b.mizu");
        assert_ne!(
            a.as_str(),
            b.as_str(),
            "distinct domains must produce distinct digests"
        );
    }

    #[test]
    fn validated_domain_path_traversal_is_neutralised() {
        // Even a path-traversal string produces a safe 64-char hex filename.
        let d = ValidatedDomain::from_raw("../../etc/passwd");
        let s = d.as_str();
        assert!(!s.contains('/'), "digest must not contain /");
        assert!(!s.contains('.'), "digest must not contain .");
        assert_eq!(s.len(), 64);
    }


    #[test]
    fn storage_path_ends_with_enc() {
        let d = ValidatedDomain::from_raw("example.com");
        let p = mizu_storage_path(&d);
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("enc"));
    }

    #[test]
    fn storage_path_filename_is_hex_digest() {
        let d = ValidatedDomain::from_raw("foo:bar");
        let p = mizu_storage_path(&d);
        let stem = p.file_stem().and_then(|n| n.to_str()).unwrap_or("");
        // Stem must be the 64-char SHA-256 hex — no colons, slashes, or dots.
        assert_eq!(stem.len(), 64);
        assert!(!stem.contains(':'));
        assert!(!stem.contains('/'));
    }

    #[test]
    fn storage_path_contains_mizu_storage_dir() {
        let d = ValidatedDomain::from_raw("example.com");
        let p = mizu_storage_path(&d);
        let s = p.to_string_lossy();
        assert!(s.contains("mizu") && s.contains("storage"));
    }


    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = [0xABu8; 32];
        let plaintext = b"hello, mizu encrypted storage!";
        let blob = encrypt_storage(&key, plaintext).unwrap();
        let recovered = decrypt_storage(&key, &blob).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn encrypt_produces_different_blobs_each_call() {
        let key = [0x42u8; 32];
        let pt = b"same plaintext";
        let b1 = encrypt_storage(&key, pt).unwrap();
        let b2 = encrypt_storage(&key, pt).unwrap();
        assert_ne!(b1, b2);
    }

    #[test]
    fn decrypt_wrong_key_returns_error() {
        let key1 = [0x11u8; 32];
        let key2 = [0x22u8; 32];
        let blob = encrypt_storage(&key1, b"secret").unwrap();
        assert!(decrypt_storage(&key2, &blob).is_err());
    }

    #[test]
    fn decrypt_truncated_blob_returns_error() {
        let key = [0x00u8; 32];
        assert!(decrypt_storage(&key, &[0u8; 8]).is_err());
    }

    #[test]
    fn decrypt_tampered_ciphertext_returns_error() {
        let key = [0x77u8; 32];
        let mut blob = encrypt_storage(&key, b"data").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(decrypt_storage(&key, &blob).is_err());
    }


    #[test]
    fn write_storage_atomic_creates_file() {
        let tmp_dir = std::env::temp_dir().join("mizu_test_atomic_write");
        std::fs::create_dir_all(&tmp_dir).expect("temp dir");
        let target = tmp_dir.join("test.enc");

        write_bytes_atomic(&target, b"test payload").expect("atomic write must succeed");

        assert!(
            target.exists(),
            "target file must exist after atomic write: {}",
            target.display()
        );

        assert!(
            !tmp_dir.join(".test.enc.tmp").exists(),
            ".tmp file must not remain after successful rename"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn write_then_read_roundtrip() {
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let key = [0x55u8; 32];
        let mut data: HashMap<String, Value> = HashMap::new();
        data.insert("hello".to_string(), Value::from("world"));
        data.insert("answer".to_string(), Value::Int(42));

        let mut inner: BTreeMap<Arc<str>, Value> = BTreeMap::new();
        inner.insert(Arc::from("nested_int"), Value::Int(7));
        inner.insert(Arc::from("nested_str"), Value::from("mizu"));
        data.insert("record_key".to_string(), Value::Record(Arc::new(inner)));

        let tmp_dir = std::env::temp_dir().join("mizu_test_roundtrip");
        std::fs::create_dir_all(&tmp_dir).expect("temp dir");
        let path = tmp_dir.join("storage.enc");

        let json_map: serde_json::Map<String, serde_json::Value> =
            data.iter().map(|(k, v)| (k.clone(), to_json(v))).collect();
        let plaintext =
            serde_json::to_vec(&serde_json::Value::Object(json_map)).expect("json encode");
        let blob = encrypt_storage(&key, &plaintext).expect("encrypt");
        write_bytes_atomic(&path, &blob).expect("atomic write");

        let read_blob = std::fs::read(&path).expect("read file");
        let read_plain = decrypt_storage(&key, &read_blob).expect("decrypt");
        let json: serde_json::Value = serde_json::from_slice(&read_plain).expect("json decode");
        let read_data: HashMap<String, Value> = match json {
            serde_json::Value::Object(map) => {
                map.into_iter().map(|(k, v)| (k, from_json(&v))).collect()
            }
            _ => panic!("expected top-level JSON object"),
        };

        assert_eq!(read_data.get("hello"), Some(&Value::from("world")));
        assert_eq!(read_data.get("answer"), Some(&Value::Int(42)));

        match read_data.get("record_key") {
            Some(Value::Record(map)) => {
                assert_eq!(map.get("nested_int"), Some(&Value::Int(7)));
                assert_eq!(
                    map.get("nested_str"),
                    Some(&Value::String(Arc::from("mizu")))
                );
            }
            other => panic!("expected Value::Record for 'record_key', got: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }


    #[test]
    #[cfg(windows)]
    fn storage_path_uses_appdata_on_windows() {
        let d = ValidatedDomain::from_raw("example.com");
        let p = mizu_storage_path(&d);

        if let Ok(appdata) = std::env::var("APPDATA") {
            assert!(
                p.starts_with(&appdata),
                "Windows path must start with %APPDATA% ({appdata}): {}",
                p.display()
            );
        }

        let s = p.to_string_lossy();
        assert!(
            s.contains("mizu") && s.contains("storage"),
            "Windows path must contain mizu\\storage hierarchy: {s}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn storage_path_uses_xdg_base_on_unix() {
        let d = ValidatedDomain::from_raw("example.com");
        let p = mizu_storage_path(&d);
        let s = p.to_string_lossy();

        let expected_base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|home| format!("{home}/.local/share"))
                .unwrap_or_else(|_| "./mizu_storage".to_owned())
        });

        assert!(
            s.starts_with(&expected_base),
            "Unix path must start with XDG base ({expected_base}): {s}"
        );
        assert!(
            s.contains("mizu") && s.contains("storage"),
            "Unix path must contain mizu/storage hierarchy: {s}"
        );
        assert!(
            !s.contains("AppData"),
            "Unix path must not contain AppData: {s}"
        );
    }

    #[test]
    fn parse_master_key_valid_hex() {
        let key = parse_master_key_hex(&"aa".repeat(32))
            .expect("valid 64-char hex must decode successfully");
        assert_eq!(key, [0xaa_u8; 32], "decoded bytes must match hex input");
    }

    #[test]
    fn parse_master_key_all_zeros() {
        let key = parse_master_key_hex(&"00".repeat(32))
            .expect("all-zeros hex must decode successfully");
        assert_eq!(key, [0x00_u8; 32]);
    }

    #[test]
    fn parse_master_key_rejects_invalid_hex() {
        let result = parse_master_key_hex("not-valid-hex!");
        assert!(result.is_err(), "invalid hex must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("MIZU_MASTER_KEY"),
            "error must mention MIZU_MASTER_KEY: {msg}"
        );
    }

    #[test]
    fn parse_master_key_rejects_too_short() {
        let result = parse_master_key_hex(&"aa".repeat(31));
        assert!(result.is_err(), "31-byte key must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("32 bytes"),
            "error must mention required length: {msg}"
        );
    }

    #[test]
    fn parse_master_key_rejects_too_long() {
        let result = parse_master_key_hex(&"aa".repeat(33));
        assert!(result.is_err(), "33-byte key must return Err");
    }


    #[test]
    fn derive_domain_key_isolates_tenants() {
        let master = [0xBEu8; 32];
        let da = ValidatedDomain::from_raw("app-a.mizu");
        let db = ValidatedDomain::from_raw("app-b.mizu");
        let ka = derive_domain_key(&master, &da).unwrap();
        let kb = derive_domain_key(&master, &db).unwrap();
        assert_ne!(
            ka, kb,
            "distinct domains must derive distinct encryption keys from the same master"
        );
    }

    #[test]
    fn derive_domain_key_is_deterministic() {
        let master = [0xFEu8; 32];
        let domain = ValidatedDomain::from_raw("deterministic.mizu");
        let k1 = derive_domain_key(&master, &domain).unwrap();
        let k2 = derive_domain_key(&master, &domain).unwrap();
        assert_eq!(k1, k2, "derived key must be deterministic");
    }

    #[test]
    fn derive_domain_key_differs_from_master() {
        let master = [0xAAu8; 32];
        let domain = ValidatedDomain::from_raw("any.mizu");
        let derived = derive_domain_key(&master, &domain).unwrap();
        assert_ne!(
            derived, master,
            "derived key must be cryptographically distinct from the master"
        );
    }


    /// When a `.enc` file exists but the keyring entry is absent,
    /// `fail_if_desync` must return `Err` to prevent overwriting the user's data.
    #[test]
    fn fail_if_desync_errors_when_storage_file_exists() {
        let tmp_dir = std::env::temp_dir().join("mizu_test_desync_present");
        std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
        let fake_enc = tmp_dir.join("fake_storage.enc");
        std::fs::write(&fake_enc, b"ciphertext").expect("write fake storage file");

        let result = fail_if_desync(&fake_enc);

        assert!(
            result.is_err(),
            "fail_if_desync must return Err when the storage file exists"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("integrity violation") || msg.contains("compromised"),
            "error message must describe the integrity violation: {msg}"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// When no `.enc` file exists (genuine fresh install), `fail_if_desync`
    /// must return `Ok` so that key generation is allowed to proceed.
    #[test]
    fn fail_if_desync_ok_on_genuine_fresh_install() {
        let path = std::env::temp_dir()
            .join("mizu_test_desync_absent_zxcvbnm_definitely_not_here.enc");
        let _ = std::fs::remove_file(&path);

        let result = fail_if_desync(&path);

        assert!(
            result.is_ok(),
            "fail_if_desync must return Ok when no storage file exists: {result:?}"
        );
    }

    /// Verifies the exact error variant so call-sites can pattern-match.
    #[test]
    fn fail_if_desync_returns_execution_error_variant() {
        let tmp = std::env::temp_dir().join("mizu_test_desync_variant.enc");
        std::fs::write(&tmp, b"data").expect("write test file");

        let err = fail_if_desync(&tmp).unwrap_err();
        assert!(
            matches!(err, MizuError::ExecutionError(_)),
            "fail_if_desync must return MizuError::ExecutionError, got: {err:?}"
        );

        let _ = std::fs::remove_file(&tmp);
    }
}
