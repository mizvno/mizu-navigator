//! `StorageWriteDebouncer` (S2 invariant).

use std::sync::{Arc, LazyLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Storage dispatch: debounced, per-domain batched redb writes (RM-12)
//
// `NetworkCmd::StorageStore` commands are no longer written straight through
// to `crate::core::storage::StoragePool` one record (one `redb` write
// transaction) at a time. Instead they are queued in a
// [`StorageWriteDebouncer`]: commands for the same domain arriving within
// `STORAGE_DEBOUNCE_WINDOW` are coalesced (last-write-wins per key) and
// committed together via `StorageEngine::write_batch` in a *single* `redb`
// write transaction — instead of one transaction (and, absent a `Durability`
// override in `open_db`, likely one `fsync`) per key. A domain that
// accumulates `STORAGE_BATCH_MAX_KEYS` distinct keys before the window
// elapses is flushed immediately rather than continuing to buffer, bounding
// both memory and worst-case write latency under sustained writes (e.g.
// `store_local` called once per keystroke or per animation frame).
//
// DURABILITY TRADEOFF — read before touching this: a write queued by the
// debouncer is only guaranteed durable once its batch's `redb` transaction
// commits, up to `STORAGE_DEBOUNCE_WINDOW` (or until `STORAGE_BATCH_MAX_KEYS`
// is reached) after the document's `store_local(key, value)` call returns
// control to the evaluator. If the process terminates abnormally (crash,
// `kill -9`, power loss) inside that window, the write is lost even though
// `store_local` already "completed" from the document's perspective. This is
// an accepted, explicit tradeoff for this runtime:
//   * Mizu documents have no `read_local` (invariant S1, `SECURITY-INVARIANTS.md`)
//     and therefore cannot observe whether a given write has landed — there is
//     no *document-visible* correctness invariant this weakens, only the
//     informal expectation that storage survives a clean-ish exit, and the
//     window is short (low hundreds of ms) either way.
//   * It does NOT apply to authentication tokens: `VaultEntry`
//     (`network::vault`) never goes through `StoragePool`/`redb` at all — every
//     `save()` writes straight to the OS keyring — so bearer tokens keep the
//     pre-existing immediate-write guarantee unconditionally, unaffected by
//     this change.
//   * Any future caller that needs a guaranteed-immediate, non-debounced write
//     to redb-backed local storage can call `StoragePool::write_record`
//     directly (this file's own tests do exactly that) — it bypasses the
//     debouncer entirely and remains a single, immediately-durable
//     transaction, as documented on `write_record` itself.
//
// Dispatching each flush via `tokio::task::spawn_blocking` keeps the keyring
// IPC and filesystem I/O off both the UI thread and the async dispatch loop
// below, exactly as before this change.
// ---------------------------------------------------------------------------

/// Debounce window for [`StorageWriteDebouncer`]: writes to the same domain
/// arriving within this window of the first buffered (unflushed) write are
/// batched into one `redb` transaction. Chosen from the middle of the
/// 100–250ms range suggested for this kind of UI-driven debounce: long enough
/// to coalesce a burst of per-keystroke/per-frame writes, short enough that
/// the durability window above stays unnoticeable in practice.
pub(crate) static STORAGE_DEBOUNCE_WINDOW: LazyLock<Duration> = LazyLock::new(|| {
    Duration::from_millis(crate::core::config::CONFIG.storage_debounce_window_ms)
});

/// Maximum number of distinct keys buffered for one domain before a flush is
/// forced immediately, regardless of how much of `STORAGE_DEBOUNCE_WINDOW`
/// remains. Without this, a document writing continuously (a new key every
/// frame, never repeating) would keep resetting into "still within the
/// window" forever and accumulate unboundedly.
pub(crate) static STORAGE_BATCH_MAX_KEYS: LazyLock<usize> =
    LazyLock::new(|| crate::core::config::CONFIG.storage_batch_max_keys);

/// Batches [`NetworkCmd::StorageStore`] writes to the same domain that arrive
/// within [`STORAGE_DEBOUNCE_WINDOW`] of each other into a single
/// `StorageEngine::write_batch` transaction. See the "Storage dispatch" doc
/// comment above for the durability tradeoff this introduces and why it is
/// scoped to non-credential local storage only.
///
/// One instance is shared (via `Clone`, which shares the inner `Arc`) across
/// every `StorageStore` dispatch for the lifetime of the network thread.
#[derive(Clone)]
pub(crate) struct StorageWriteDebouncer {
    /// Per-domain (keyed by `ValidatedDomain::as_str()`, the SHA-256 hex
    /// digest) buffer of not-yet-flushed writes. `HashMap<key, value>` so
    /// that repeated writes to the same key within one window collapse to
    /// last-write-wins instead of encrypting/inserting each one individually
    /// when the batch is eventually committed.
    pending: Arc<std::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, crate::core::types::Value>>>>,
    window: Duration,
    max_keys: usize,
}

impl StorageWriteDebouncer {
    pub(crate) fn new() -> Self {
        Self::with_params(*STORAGE_DEBOUNCE_WINDOW, *STORAGE_BATCH_MAX_KEYS)
    }

    /// Like [`Self::new`], but with explicit window/threshold — used by tests
    /// that need short, deterministic timing instead of waiting out the
    /// production window.
    pub(crate) fn with_params(window: Duration, max_keys: usize) -> Self {
        Self {
            pending: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            window,
            max_keys,
        }
    }

    /// Queues one `(key, value)` write for `domain`. Returns immediately —
    /// the actual `redb` transaction happens later, on a spawned task, once
    /// this domain's batch is flushed (by the debounce timer elapsing or by
    /// hitting `max_keys`).
    pub(crate) fn submit(
        &self,
        storage_pool: crate::core::storage::StoragePool,
        domain: crate::core::storage::ValidatedDomain,
        key: String,
        value: crate::core::types::Value,
    ) {
        let domain_key = domain.as_str().to_string();
        let mut should_spawn_timer = false;
        let mut immediate_flush = None;
        {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            let entry = pending.entry(domain_key.clone()).or_default();
            let was_empty = entry.is_empty();
            entry.insert(key, value);
            if entry.len() >= self.max_keys {
                immediate_flush = Some(std::mem::take(entry));
                pending.remove(&domain_key);
            } else if was_empty {
                should_spawn_timer = true;
            }
            // else: batch already non-empty and below threshold — a timer
            // from the first write into this batch is already scheduled and
            // will pick up this write when it fires. No new timer needed.
        }

        if let Some(records) = immediate_flush {
            Self::spawn_flush(storage_pool, domain, records);
        } else if should_spawn_timer {
            let pending_arc = self.pending.clone();
            let window = self.window;
            tokio::spawn(async move {
                tokio::time::sleep(window).await;
                let records = {
                    let mut pending = pending_arc.lock().unwrap_or_else(|p| p.into_inner());
                    pending.remove(&domain_key)
                };
                if let Some(records) = records
                    && !records.is_empty()
                {
                    Self::spawn_flush(storage_pool, domain, records);
                }
            });
        }
    }

    /// Commits one batch as a single `redb` write transaction on the
    /// blocking thread pool, so filesystem I/O never blocks the async
    /// dispatch loop or the debounce timer tasks above.
    fn spawn_flush(
        storage_pool: crate::core::storage::StoragePool,
        domain: crate::core::storage::ValidatedDomain,
        records: std::collections::HashMap<String, crate::core::types::Value>,
    ) {
        tokio::task::spawn_blocking(move || {
            let engine = match storage_pool.get_or_open(&domain) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        domain = %domain.as_str(),
                        "storage batch flush: failed to open engine"
                    );
                    return;
                }
            };
            if let Err(e) = engine.write_batch(records.iter().map(|(k, v)| (k.as_str(), v))) {
                tracing::warn!(error = %e, domain = %domain.as_str(), "storage batch flush failed");
            }
        });
    }
}
