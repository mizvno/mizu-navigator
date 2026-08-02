//! [`StoragePool`]: a thread-safe, process-lifetime cache of open
//! [`StorageEngine`]s, keyed by the validated (hashed) domain string.

use std::collections::HashMap;

use crate::core::errors::MizuError;
use crate::core::types::Value;

use super::domain::ValidatedDomain;
use super::engine::StorageEngine;

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
    pub fn get_or_open(
        &self,
        domain: &ValidatedDomain,
    ) -> Result<std::sync::Arc<StorageEngine>, MizuError> {
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
    pub fn write_record(
        &self,
        domain: &ValidatedDomain,
        key: &str,
        value: &Value,
    ) -> Result<(), MizuError> {
        let engine = self.get_or_open(domain)?;
        engine.write_batch(std::iter::once((key, value)))
    }

    /// Seeds the pool's cache with a pre-built engine, bypassing the keyring
    /// and `redb::Database::create`. Lets tests outside this module exercise
    /// `write_record`/`get_or_open` against an isolated in-memory-backed
    /// engine without touching the real OS keyring or storage directory.
    pub fn insert_for_test(&self, domain: &ValidatedDomain, engine: std::sync::Arc<StorageEngine>) {
        self.engines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(domain.as_str().to_string(), engine);
    }
}
