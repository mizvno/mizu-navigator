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

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use redb::ReadableTable;
use zeroize::{Zeroize, Zeroizing};

type HmacSha256 = Hmac<Sha256>;

use crate::core::errors::MizuError;
use crate::core::types::{Value, from_json, to_json};

/// The single table definition for redb storage.
/// Key: Variable name (`&str`)
/// Value: `nonce || ciphertext` (`&[u8]`)
pub const STORAGE_TABLE: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("mizu_storage");


/// A validated, opaque domain identifier whose inner value is the lowercase
/// SHA-256 hex digest of the normalised raw domain string.
pub struct ValidatedDomain(String);

impl ValidatedDomain {
    pub fn from_raw(domain: &str) -> Self {
        let normalised = domain.trim().to_lowercase();
        let mut hasher = Sha256::new();
        hasher.update(normalised.as_bytes());
        let digest = hasher.finalize();
        ValidatedDomain(hex::encode(digest))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}


/// Returns the path where the encrypted storage file for `domain` will live.
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

pub(crate) fn fail_if_desync(storage_path: &std::path::Path) -> Result<(), MizuError> {
    if storage_path.exists() {
        return Err(MizuError::ExecutionError(
            "keyring integrity violation: a storage file exists for this domain but the \
             corresponding keyring entry is missing — environment integrity has been \
             compromised. Restore the OS keyring entry or set MIZU_MASTER_KEY to recover access."
                .to_owned(),
        ));
    }
    Ok(())
}

/// Decodes a hex-encoded 32-byte key into a self-scrubbing buffer.
///
/// RM-10: `hex::decode` allocates a heap `Vec<u8>` holding the raw key bytes;
/// deallocating it does not scrub the memory, so it is explicitly zeroized
/// before it drops instead of being left for the allocator to reuse verbatim.
/// The returned `Zeroizing<[u8; 32]>` likewise scrubs itself when it goes out
/// of scope, however that happens (early `drop`, error return, or normal
/// end-of-scope).
fn hex_decode_key_32(hex_str: &str, ctx: &str) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    let mut bytes = hex::decode(hex_str)
        .map_err(|e| MizuError::ExecutionError(format!("{ctx} decode: {e}")))?;
    let result = if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Zeroizing::new(arr))
    } else {
        Err(MizuError::ExecutionError(format!(
            "{ctx} must be exactly 32 bytes (64 hex chars)"
        )))
    };
    bytes.zeroize();
    result
}

fn parse_master_key_hex(hex: &str) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    hex_decode_key_32(hex, "MIZU_MASTER_KEY")
}

fn derive_domain_key(
    master_key: &[u8; 32],
    domain: &ValidatedDomain,
) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(master_key)
        .map_err(|e| MizuError::ExecutionError(format!("HMAC init: {e}")))?;
    mac.update(domain.as_str().as_bytes());
    let digest: [u8; 32] = mac.finalize().into_bytes().into();
    Ok(Zeroizing::new(digest))
}

pub fn derive_or_create_key(domain: &ValidatedDomain) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    if let Ok(hex) = std::env::var("MIZU_MASTER_KEY") {
        // RM-10: `master` (the raw domain-wide master key) is only needed to
        // derive this domain's key below; it scrubs itself (`Zeroizing`) the
        // moment this call returns, rather than lingering in memory for the
        // life of the process the way the *result* of `derive_or_create_key`
        // does inside `StorageEngine`.
        let master = parse_master_key_hex(&hex)?;
        return derive_domain_key(&master, domain);
    }

    let entry = match keyring::Entry::new(KEYRING_SERVICE, domain.as_str()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("keyring unavailable ({}); set MIZU_MASTER_KEY for headless operation", e);
            return Err(MizuError::ExecutionError(format!("keyring open: {e}")));
        }
    };

    match entry.get_password() {
        Ok(hex) => hex_decode_key_32(&hex, "keyring key"),
        Err(keyring::Error::NoEntry) => {
            fail_if_desync(&mizu_storage_path(domain))?;
            let raw_key = Aes256Gcm::generate_key(OsRng);
            let hex_key = hex::encode(raw_key.as_slice());
            entry
                .set_password(&hex_key)
                .map_err(|e| MizuError::ExecutionError(format!("keyring save: {e}")))?;
            Ok(Zeroizing::new(raw_key.into()))
        }
        Err(e) => {
            tracing::warn!("keyring read failed ({}); set MIZU_MASTER_KEY for headless operation", e);
            Err(MizuError::ExecutionError(format!("keyring read: {e}")))
        }
    }
}

/// Derives a 32-byte encryption key for a specific record from the domain master key.
/// Uses HKDF-SHA256 with the variable name as the `info` parameter.
///
/// RM-10: the returned key is only ever needed for a single encrypt/decrypt
/// call, so it is wrapped in `Zeroizing` — both call sites (`encrypt_record`,
/// `decrypt_record`) drop it explicitly right after building the cipher from
/// it, rather than letting it sit on the stack until the end of the function.
pub fn derive_record_key(master_key: &[u8; 32], variable_name: &str) -> Result<Zeroizing<[u8; 32]>, MizuError> {
    let hk = Hkdf::<Sha256>::new(None, master_key);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(variable_name.as_bytes(), out.as_mut())
        .map_err(|e| MizuError::ExecutionError(format!("HKDF expand: {e}")))?;
    Ok(out)
}

/// Encrypts `plaintext` with AES-256-GCM using a record-specific key and returns `nonce || ciphertext`.
pub fn encrypt_record(master_key: &[u8; 32], variable_name: &str, plaintext: &[u8]) -> Result<Vec<u8>, MizuError> {
    let key = derive_record_key(master_key, variable_name)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_ref()));
    drop(key); // record key is single-use; scrub it now instead of at function end.
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| MizuError::ExecutionError(format!("AES-GCM encrypt: {e}")))?;

    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts a blob produced by `encrypt_record`.
pub fn decrypt_record(master_key: &[u8; 32], variable_name: &str, blob: &[u8]) -> Result<Vec<u8>, MizuError> {
    if blob.len() < 12 {
        return Err(MizuError::ExecutionError("storage blob too short (missing nonce)".to_owned()));
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


/// Opens the redb database for the given domain.
///
/// ## Multi-process concurrency (INV-02)
///
/// `mizu-navigator` has no single-instance guard (`main.rs` has no lock
/// file, PID check, or IPC "activate existing window" mechanism — every
/// `cargo run`/binary launch is an independent OS process with its own
/// window, exactly like a browser's separate processes). So more than one
/// process legitimately *can* call `open_db` for the same domain at the
/// same time (e.g. the user launches the navigator twice, or twice against
/// documents that happen to share a `mizu://` origin). This is not
/// prevented, and is not this file's job to prevent — redb itself already
/// serializes it:
///
/// `redb::Database::create`/`open` (via `FileBackend::new`, `redb` 2.6.3)
/// takes an OS-level, non-blocking, exclusive advisory lock on the
/// underlying file the moment it's opened (`flock(fd, LOCK_EX | LOCK_NB)`
/// on Unix, `LockFile` on Windows — see `redb`'s `tree_store/page_store/
/// file_backend/{unix,windows}.rs`), held for the lifetime of the
/// `Database` value and released on `Drop`. A second process (or a second,
/// independent `File` handle within the same process) trying to open the
/// same path while the first is still holding it gets
/// `Err(DatabaseError::DatabaseAlreadyOpen)` immediately — never a hang,
/// never silent corruption, never a torn write. `open_db` below already
/// propagates that error through the normal `Result` chain like any other
/// redb failure, so this fails safely (a warning-logged, non-fatal error
/// surfaced to the caller) with no additional code needed here. See
/// `tests::concurrent_process_open_is_serialized_by_redb_flock` for a
/// same-machine, two-real-process regression test of this exact guarantee,
/// and `walkthrough.md`'s "INV-02" entry for the full investigation.
///
/// **Do not add an application-level file lock (`fd-lock` or similar) on
/// top of this** — it would be redundant with redb's own locking and add
/// complexity without closing any gap.
pub fn open_db(domain: &ValidatedDomain) -> Result<redb::Database, MizuError> {
    let path = mizu_storage_path(domain);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let db = redb::Database::create(&path)
        .map_err(|e| MizuError::ExecutionError(format!("redb create: {e}")))?;

    // Ensure the table is created
    let write_txn = db.begin_write()
        .map_err(|e| MizuError::ExecutionError(format!("redb begin_write: {e}")))?;
    {
        let _ = write_txn.open_table(STORAGE_TABLE)
            .map_err(|e| MizuError::ExecutionError(format!("redb open_table: {e}")))?;
    }
    write_txn.commit()
        .map_err(|e| MizuError::ExecutionError(format!("redb commit: {e}")))?;

    Ok(db)
}

/// The engine maintains an open database and the master key for O(1) mutations.
pub struct StorageEngine {
    db: redb::Database,
    /// RM-10: `StoragePool` caches engines for the life of the process (see
    /// `StoragePool`'s doc comment below) rather than reopening them per
    /// command, so this key would otherwise sit in memory — reachable via
    /// swap, a core dump, or a debugger — for the entire process lifetime.
    /// `Zeroizing` scrubs it the moment the engine (and this field) is
    /// dropped instead of leaving it for the allocator to hand out verbatim.
    master_key: Zeroizing<[u8; 32]>,
    /// RM-12: counts `write_batch` calls (one per `redb` write transaction),
    /// so tests can assert that debounced batching in `network::worker`
    /// actually reduces the number of transactions/fsyncs instead of just
    /// asserting on the end state. Not read on any production path.
    #[cfg(test)]
    write_batch_calls: std::sync::atomic::AtomicUsize,
}

impl StorageEngine {
    pub fn open(domain: &ValidatedDomain) -> Result<Self, MizuError> {
        let master_key = derive_or_create_key(domain)?;
        let db = open_db(domain)?;
        Ok(Self {
            db,
            master_key,
            #[cfg(test)]
            write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Builds an engine directly from an already-open database and key,
    /// bypassing the keyring and `mizu_storage_path`. For tests only.
    #[cfg(test)]
    pub(crate) fn from_parts(db: redb::Database, master_key: [u8; 32]) -> Self {
        Self {
            db,
            master_key: Zeroizing::new(master_key),
            write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Number of `write_batch` calls (== number of `redb` write transactions)
    /// made against this engine so far. Test-only introspection used to
    /// verify that debounced batching actually reduces transaction count.
    #[cfg(test)]
    pub(crate) fn write_batch_call_count(&self) -> usize {
        self.write_batch_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn read_all(&self) -> Result<HashMap<String, Value>, MizuError> {
        let read_txn = self.db.begin_read()
            .map_err(|e| MizuError::ExecutionError(format!("redb begin_read: {e}")))?;
        
        let table = match read_txn.open_table(STORAGE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
            Err(e) => return Err(MizuError::ExecutionError(format!("redb open_table: {e}"))),
        };

        let mut map = HashMap::new();
        let iter = table.iter().map_err(|e| MizuError::ExecutionError(format!("redb iter: {e}")))?;
        for result in iter {
            let (k, v) = result.map_err(|e| MizuError::ExecutionError(format!("redb iter item: {e}")))?;
            let key_str = k.value();
            let blob = v.value();

            match decrypt_record(&self.master_key, key_str, blob) {
                Ok(plaintext) => {
                    match serde_json::from_slice::<serde_json::Value>(&plaintext) {
                        Ok(json) => match from_json(&json) {
                            Ok(value) => {
                                map.insert(key_str.to_string(), value);
                            }
                            Err(e) => tracing::warn!(
                                "failed to convert json to Value for storage key '{}': {}",
                                key_str, e
                            ),
                        },
                        Err(e) => tracing::warn!("failed to decode json for storage key '{}': {}", key_str, e),
                    }
                }
                Err(e) => tracing::warn!("failed to decrypt storage key '{}': {}", key_str, e),
            }
        }
        Ok(map)
    }

    pub fn write_batch<'a, I>(&self, records: I) -> Result<(), MizuError>
    where
        I: IntoIterator<Item = (&'a str, &'a Value)>,
    {
        #[cfg(test)]
        self.write_batch_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let write_txn = self.db.begin_write()
            .map_err(|e| MizuError::ExecutionError(format!("redb begin_write: {e}")))?;
        {
            let mut table = write_txn.open_table(STORAGE_TABLE)
                .map_err(|e| MizuError::ExecutionError(format!("redb open_table: {e}")))?;
            for (key, value) in records {
                let json = to_json(value);
                let plaintext = serde_json::to_vec(&json)
                    .map_err(|e| MizuError::ExecutionError(format!("json encode: {e}")))?;
                let blob = encrypt_record(&self.master_key, key, &plaintext)?;
                table.insert(key, blob.as_slice())
                    .map_err(|e| MizuError::ExecutionError(format!("redb insert: {e}")))?;
            }
        }
        write_txn.commit()
            .map_err(|e| MizuError::ExecutionError(format!("redb commit: {e}")))?;
        Ok(())
    }
}

/// Convenience accessor for reading the initial state of a domain.
pub fn read_storage(domain: &ValidatedDomain) -> Result<HashMap<String, Value>, MizuError> {
    let engine = StorageEngine::open(domain)?;
    engine.read_all()
}

/// Thread-safe pool of open [`StorageEngine`]s, keyed by the validated
/// (hashed) domain string.
///
/// Opening an engine costs a keyring IPC round-trip (or `MIZU_MASTER_KEY`
/// parse) plus opening the `redb` database file, so engines are cached for
/// the lifetime of the process instead of being re-opened on every
/// `StorageStore` command. `redb::Database` is internally synchronised, so a
/// cached engine can be shared across concurrent blocking tasks via `Arc`.
///
/// This `Mutex` only serialises access *within this process*. Cross-process
/// concurrent access to the same domain (a legitimate scenario — see
/// `open_db`'s doc comment, INV-02) is a separate concern, already handled
/// by `redb`'s own OS-level file locking; nothing extra is needed here.
#[derive(Clone, Default)]
pub struct StoragePool {
    engines: std::sync::Arc<std::sync::Mutex<HashMap<String, std::sync::Arc<StorageEngine>>>>,
}

impl StoragePool {
    /// Creates an empty pool with no open engines.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cached engine for `domain`, opening (and caching) it on
    /// first access.
    pub fn get_or_open(&self, domain: &ValidatedDomain) -> Result<std::sync::Arc<StorageEngine>, MizuError> {
        let mut engines = self.engines.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(engine) = engines.get(domain.as_str()) {
            return Ok(engine.clone());
        }
        let engine = std::sync::Arc::new(StorageEngine::open(domain)?);
        engines.insert(domain.as_str().to_string(), engine.clone());
        Ok(engine)
    }

    /// Encrypts and writes a single record directly against `redb`, in its
    /// own write transaction. The write is durable (via `redb`'s WAL) by the
    /// time this call returns — no write-behind cache, no debounce — and
    /// each record is encrypted with its own HKDF-derived key, so other
    /// records are unaffected by this write.
    ///
    /// RM-12: `network::worker`'s `NetworkCmd::StorageStore` dispatch no
    /// longer calls this directly for every write — it batches closely-spaced
    /// writes to the same domain via `StorageEngine::write_batch` instead
    /// (see the "Storage dispatch" doc comment in `worker.rs` for the
    /// resulting durability tradeoff). This method remains the immediate,
    /// non-debounced write primitive for any caller that needs a single
    /// write to be durable the instant it returns.
    pub fn write_record(&self, domain: &ValidatedDomain, key: &str, value: &Value) -> Result<(), MizuError> {
        let engine = self.get_or_open(domain)?;
        engine.write_batch(std::iter::once((key, value)))
    }

    /// Seeds the pool's cache with a pre-built engine, bypassing the keyring
    /// and `redb::Database::create`. Lets tests outside this module exercise
    /// `write_record`/`get_or_open` against an isolated in-memory-backed
    /// engine without touching the real OS keyring or storage directory.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, domain: &ValidatedDomain, engine: std::sync::Arc<StorageEngine>) {
        self.engines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(domain.as_str().to_string(), engine);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_domain_normalises_case_and_whitespace() {
        let a = ValidatedDomain::from_raw("  Example.COM  ");
        let b = ValidatedDomain::from_raw("example.com");
        assert_eq!(a.as_str(), b.as_str());
    }

    #[test]
    fn validated_domain_distinct_inputs_yield_distinct_digests() {
        let a = ValidatedDomain::from_raw("app-a.mizu");
        let b = ValidatedDomain::from_raw("app-b.mizu");
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = [0xABu8; 32];
        let pt = b"hello, mizu encrypted storage!";
        let blob = encrypt_record(&key, "my_var", pt).unwrap();
        let recovered = decrypt_record(&key, "my_var", &blob).unwrap();
        assert_eq!(recovered, pt);
    }

    #[test]
    fn hkdf_isolates_variable_keys() {
        let key = [0x11u8; 32];
        let pt = b"secret";
        let blob_a = encrypt_record(&key, "var_a", pt).unwrap();
        // Trying to decrypt var_a's blob using var_b's derived key should fail
        assert!(decrypt_record(&key, "var_b", &blob_a).is_err());
    }

    /// RM-10 acceptance test: a compile-time proof that every function which
    /// produces key material now returns a type that scrubs itself on drop,
    /// rather than a runtime memory-inspection test — this module is
    /// `#![forbid(unsafe_code)]`, and reading freed stack memory to check for
    /// zeroing would itself require unsafe (and be UB besides). `Zeroizing<T>`
    /// implements `zeroize::ZeroizeOnDrop`; a plain `[u8; 32]` does not, so
    /// `assert_zeroize_on_drop` only compiles here because the return types
    /// of `derive_record_key`/`derive_domain_key`/`parse_master_key_hex`
    /// genuinely changed from `[u8; 32]` to `Zeroizing<[u8; 32]>`.
    #[test]
    fn derived_keys_are_self_zeroizing_on_drop() {
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>(_: &T) {}

        let master = [0x11u8; 32];
        let domain = ValidatedDomain::from_raw("zeroize-typecheck.local");

        let record_key = derive_record_key(&master, "var").unwrap();
        assert_zeroize_on_drop(&record_key);

        let domain_key = derive_domain_key(&master, &domain).unwrap();
        assert_zeroize_on_drop(&domain_key);

        let hex_master = hex::encode(master);
        let parsed = parse_master_key_hex(&hex_master).unwrap();
        assert_zeroize_on_drop(&parsed);
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp_dir = std::env::temp_dir().join("mizu_test_redb");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("test.com.enc");
        
        let db = redb::Database::create(&path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let _ = write_txn.open_table(STORAGE_TABLE).unwrap();
        }
        write_txn.commit().unwrap();
        
        let master_key = [0x42u8; 32];
        let engine = StorageEngine {
            db,
            master_key: Zeroizing::new(master_key),
            write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
        };

        let mut data: HashMap<String, Value> = HashMap::new();
        data.insert("hello".to_string(), Value::from("world"));
        data.insert("answer".to_string(), Value::Int(42));

        engine.write_batch(data.iter().map(|(k, v)| (k.as_str(), v))).expect("write_batch");

        let read_data = engine.read_all().expect("read_all");

        assert_eq!(read_data.get("hello"), Some(&Value::from("world")));
        assert_eq!(read_data.get("answer"), Some(&Value::Int(42)));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// RM-07: a single record whose stored JSON exceeds `MAX_JSON_DEPTH`
    /// must not abort `read_all` for the whole domain. Before the fix, the
    /// `from_json(&json)?` in the `Ok` branch propagated the depth-limit
    /// `SecurityViolation` out of `read_all` entirely, so one over-deep
    /// record made every other record in the domain unreadable too.
    #[test]
    fn read_all_skips_over_deep_record_but_returns_rest() {
        let tmp_dir = std::env::temp_dir().join("mizu_test_redb_deep");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("deep.enc");

        let db = redb::Database::create(&path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let _ = write_txn.open_table(STORAGE_TABLE).unwrap();
        }
        write_txn.commit().unwrap();

        let master_key = [0x99u8; 32];
        let engine = StorageEngine {
            db,
            master_key: Zeroizing::new(master_key),
            write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
        };

        // Build a Value nested well beyond MAX_JSON_DEPTH. `to_json`/`write_batch`
        // don't depth-check on the way in (only `from_json`, on the way out,
        // does), so this reproduces a record that was legitimately persisted
        // but can no longer be decoded back into a `Value`.
        let mut deep = Value::Int(1);
        for _ in 0..300 {
            deep = Value::List(std::sync::Arc::new(vec![deep]));
        }

        let mut data: HashMap<String, Value> = HashMap::new();
        data.insert("too_deep".to_string(), deep);
        data.insert("normal".to_string(), Value::from("still here"));

        engine
            .write_batch(data.iter().map(|(k, v)| (k.as_str(), v)))
            .expect("write_batch");

        let read_data = engine.read_all().expect("read_all must not fail for the whole domain");

        assert_eq!(read_data.get("normal"), Some(&Value::from("still here")));
        assert!(
            !read_data.contains_key("too_deep"),
            "over-deep record must be skipped (with a warning), not silently truncated or kept"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn storage_pool_reuses_cached_engine_and_writes_are_immediately_durable() {
        let tmp_dir = std::env::temp_dir().join("mizu_test_storage_pool");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("pool.enc");

        let db = redb::Database::create(&path).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let _ = write_txn.open_table(STORAGE_TABLE).unwrap();
            }
            write_txn.commit().unwrap();
        }
        let engine = std::sync::Arc::new(StorageEngine::from_parts(db, [0x77u8; 32]));

        let pool = StoragePool::new();
        let domain = ValidatedDomain::from_raw("pool-test.local");
        pool.insert_for_test(&domain, engine.clone());

        // A cached domain must return the exact same Arc, never re-opening
        // the keyring/redb file — this is what makes per-write dispatch cheap.
        let fetched = pool.get_or_open(&domain).expect("cached engine must be returned");
        assert!(
            std::sync::Arc::ptr_eq(&fetched, &engine),
            "get_or_open must reuse the cached engine, not open a new one"
        );

        // write_record persists through redb with no artificial delay: no
        // sleep is needed before the value is visible to a subsequent read.
        pool.write_record(&domain, "greeting", &Value::from("hi"))
            .expect("write_record");
        let data = engine.read_all().expect("read_all");
        assert_eq!(data.get("greeting"), Some(&Value::from("hi")));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// INV-02: two *real, independent OS processes* opening the same redb
    /// file for the same domain must be serialized safely — the second
    /// opener must be rejected (not hang, not corrupt, not silently
    /// succeed), and the lock must be genuinely released (not stuck) once
    /// the first process closes its handle.
    ///
    /// This re-execs the test binary itself as a child process, gated by an
    /// env var, following the same pattern already established by
    /// `core::types::tests::cross_function_composition_depth_guard` /
    /// `measure_stack_usage_at_max_eval_depth` for other process-level
    /// guarantees in this codebase — a genuine second process, not a mock.
    #[test]
    fn concurrent_process_open_is_serialized_by_redb_flock() {
        const CHILD_PATH_ENV: &str = "MIZU_STORAGE_LOCK_CHILD_PATH";
        const CHILD_OPENED: &str = "CHILD_OPENED_DB_OK";
        const CHILD_LOCKED_OUT: &str = "CHILD_GOT_DATABASE_ALREADY_OPEN";

        // Child mode: try to open the redb file at the path given via env
        // var, report the outcome on stdout, then exit. Real process exit,
        // real OS file lock — no simulation.
        if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
            match redb::Database::create(path) {
                Ok(_db) => println!("{CHILD_OPENED}"),
                Err(redb::DatabaseError::DatabaseAlreadyOpen) => println!("{CHILD_LOCKED_OUT}"),
                Err(e) => println!("CHILD_OTHER_ERROR: {e}"),
            }
            return;
        }

        // Parent mode.
        let tmp_dir = std::env::temp_dir().join("mizu_test_multiprocess_lock");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
        let path = tmp_dir.join("locked.redb");

        let exe = std::env::current_exe().expect("current_exe");
        let spawn_child = |exe: &std::path::Path, path: &std::path::Path| {
            std::process::Command::new(exe)
                .arg("core::storage::tests::concurrent_process_open_is_serialized_by_redb_flock")
                .arg("--exact")
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(CHILD_PATH_ENV, path)
                .output()
                .expect("failed to spawn child test process")
        };

        // Parent opens (and holds) the database first, exactly as one
        // running `mizu-navigator` process would while a document with that
        // domain remains open.
        let db = redb::Database::create(&path).expect("parent opens db");

        // While the parent still holds it, a second, independent process
        // trying to open the exact same file must be rejected immediately —
        // not hang waiting for the lock, not corrupt the file, not silently
        // proceed as if nothing else had it open.
        let child1 = spawn_child(&exe, &path);
        let stdout1 = String::from_utf8_lossy(&child1.stdout);
        assert!(
            stdout1.contains(CHILD_LOCKED_OUT),
            "a second process opening the same redb file while the first \
             still holds it must get DatabaseAlreadyOpen; stdout: {stdout1} \
             stderr: {}",
            String::from_utf8_lossy(&child1.stderr)
        );

        // Release the parent's handle and confirm the lock was genuinely
        // released (not stuck forever) — a subsequent process must now be
        // able to open the file cleanly.
        drop(db);
        let child2 = spawn_child(&exe, &path);
        let stdout2 = String::from_utf8_lossy(&child2.stdout);
        assert!(
            stdout2.contains(CHILD_OPENED),
            "after the holder closes the database, a new process must be \
             able to open it; stdout: {stdout2} stderr: {}",
            String::from_utf8_lossy(&child2.stderr)
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
