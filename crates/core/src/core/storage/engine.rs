//! `open_db` (redb database open, with concurrent-open semantics documented
//! at length), [`StorageEngine`] (the per-domain cached handle: key +
//! database + read/write operations), and `read_storage` (a convenience
//! one-shot accessor).

use std::collections::HashMap;

use redb::ReadableTable;
use zeroize::Zeroizing;

use crate::core::errors::MizuError;
use crate::core::types::{Value, from_json_slice, to_json};

use super::crypto::{decrypt_record, encrypt_record};
use super::domain::{ValidatedDomain, mizu_storage_path};
use super::keys::derive_or_create_key;

/// Storage plaintext is trusted input for [`from_json_slice`]'s node cap: it
/// is a record this build wrote, encrypted under a key only this build holds
/// and authenticated on the way back in. Capping it would mean a value the
/// evaluator was allowed to persist could silently fail to load later.
const STORAGE_IS_TRUSTED: bool = true;

/// The single table definition for redb storage.
/// Key: Variable name (`&str`)
/// Value: `nonce || ciphertext` (`&[u8]`)
pub const STORAGE_TABLE: redb::TableDefinition<&str, &[u8]> =
    redb::TableDefinition::new("mizu_storage");

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
    let write_txn = db
        .begin_write()
        .map_err(|e| MizuError::ExecutionError(format!("redb begin_write: {e}")))?;
    {
        let _ = write_txn
            .open_table(STORAGE_TABLE)
            .map_err(|e| MizuError::ExecutionError(format!("redb open_table: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| MizuError::ExecutionError(format!("redb commit: {e}")))?;

    Ok(db)
}

/// The engine maintains an open database and the master key for O(1) mutations.
pub struct StorageEngine {
    pub(super) db: redb::Database,
    /// RM-10: `StoragePool` caches engines for the life of the process (see
    /// `StoragePool`'s doc comment below) rather than reopening them per
    /// command, so this key would otherwise sit in memory — reachable via
    /// swap, a core dump, or a debugger — for the entire process lifetime.
    /// `Zeroizing` scrubs it the moment the engine (and this field) is
    /// dropped instead of leaving it for the allocator to hand out verbatim.
    pub(super) master_key: Zeroizing<[u8; 32]>,
    /// RM-12: counts `write_batch` calls (one per `redb` write transaction),
    /// so tests can assert that debounced batching in `network::worker`
    /// actually reduces the number of transactions/fsyncs instead of just
    /// asserting on the end state. Not read on any production path.
    pub(super) write_batch_calls: std::sync::atomic::AtomicUsize,
}

impl StorageEngine {
    pub fn open(domain: &ValidatedDomain) -> Result<Self, MizuError> {
        let master_key = derive_or_create_key(domain)?;
        let db = open_db(domain)?;
        Ok(Self {
            db,
            master_key,
            write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Builds an engine directly from an already-open database and key,
    /// bypassing the keyring and `mizu_storage_path`. For tests only.
    pub fn from_parts(db: redb::Database, master_key: [u8; 32]) -> Self {
        Self {
            db,
            master_key: Zeroizing::new(master_key),
            write_batch_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Number of `write_batch` calls (== number of `redb` write transactions)
    /// made against this engine so far. Test-only introspection used to
    /// verify that debounced batching actually reduces transaction count.
    pub fn write_batch_call_count(&self) -> usize {
        self.write_batch_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Decrypts and parses every record in this domain's table into an
    /// in-memory map.
    ///
    /// This is a full-table scan, `O(records)` in time and peak memory —
    /// not the `O(1)` point lookup `redb`'s B-tree is otherwise good for.
    /// That's intentionally not fixed by lazy per-key reads triggered by
    /// first `Symbol` access during evaluation: **S1** (write-only storage,
    /// see `SECURITY-INVARIANTS.md`) deliberately exposes no `read_local`
    /// path back into the evaluator, so there is no "first access" event to
    /// hook — reintroducing one here would quietly rebuild exactly the
    /// read-back path S1 rules out, without the load-time flow checker ever
    /// being taught about the new taint source. Currently a non-issue in
    /// practice too: no production load path calls this (`initial_variables`
    /// is seeded from the live in-memory store, not from disk) — only tests
    /// exercise it today.
    pub fn read_all(&self) -> Result<HashMap<String, Value>, MizuError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| MizuError::ExecutionError(format!("redb begin_read: {e}")))?;

        let table = match read_txn.open_table(STORAGE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
            Err(e) => return Err(MizuError::ExecutionError(format!("redb open_table: {e}"))),
        };

        let mut map = HashMap::new();
        let iter = table
            .iter()
            .map_err(|e| MizuError::ExecutionError(format!("redb iter: {e}")))?;
        for result in iter {
            let (k, v) =
                result.map_err(|e| MizuError::ExecutionError(format!("redb iter item: {e}")))?;
            let key_str = k.value();
            let blob = v.value();

            match decrypt_record(&self.master_key, key_str, blob) {
                Ok(plaintext) => match from_json_slice(&plaintext, STORAGE_IS_TRUSTED) {
                    Ok(value) => {
                        map.insert(key_str.to_string(), value);
                    }
                    Err(e) => tracing::warn!(
                        "failed to convert json to Value for storage key '{}': {}",
                        key_str,
                        e
                    ),
                },
                Err(e) => tracing::warn!("failed to decrypt storage key '{}': {}", key_str, e),
            }
        }
        Ok(map)
    }

    /// Writes every `(key, value)` pair in `records` in a single `redb`
    /// transaction. Nonces are drawn from `OsRng` directly inside `encrypt_record`.
    pub fn write_batch<'a, I>(&self, records: I) -> Result<(), MizuError>
    where
        I: IntoIterator<Item = (&'a str, &'a Value)>,
    {
        self.write_batch_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| MizuError::ExecutionError(format!("redb begin_write: {e}")))?;
        {
            let mut table = write_txn
                .open_table(STORAGE_TABLE)
                .map_err(|e| MizuError::ExecutionError(format!("redb open_table: {e}")))?;
            for (key, value) in records {
                let json = to_json(value)?;
                let plaintext = serde_json::to_vec(&json)
                    .map_err(|e| MizuError::ExecutionError(format!("json encode: {e}")))?;
                let blob = encrypt_record(&self.master_key, key, &plaintext)?;
                table
                    .insert(key, blob.as_slice())
                    .map_err(|e| MizuError::ExecutionError(format!("redb insert: {e}")))?;
            }
        }
        write_txn
            .commit()
            .map_err(|e| MizuError::ExecutionError(format!("redb commit: {e}")))?;
        Ok(())
    }
}

/// Convenience accessor for reading the initial state of a domain.
pub fn read_storage(domain: &ValidatedDomain) -> Result<HashMap<String, Value>, MizuError> {
    let engine = StorageEngine::open(domain)?;
    engine.read_all()
}
