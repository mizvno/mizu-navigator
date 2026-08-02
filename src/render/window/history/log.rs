//! `HistoryLog`: the window-level, persisted-across-launches visit log that
//! backs the history sidebar.

use super::crypto::*;
use super::platform::*;
use super::*;

// ── HistoryLog (window-level, persistent sidebar log) ─────────────────────────

/// A window-level, persistable, bounded log of visited pages.
///
/// Unlike [`HistoryStack`], this structure is never mutated by Back/Forward
/// steps — it only grows (or is cleared). It is the data source for the
/// history sidebar panel.
///
/// Records are stored newest-first and capped at [`MAX_LOG_ENTRIES`] with
/// oldest-record eviction. Persistence is an encrypted file in the platform's
/// application data directory, written atomically (write-then-rename) so a
/// crash during save never corrupts existing data.
#[derive(Debug, Default)]
pub struct HistoryLog {
    /// Records ordered newest-first (index 0 = most recent).
    records: VecDeque<VisitRecord>,
    /// Set by every mutation, cleared by [`Self::save_to_disk`]: saving is
    /// otherwise a full re-encrypt of up to [`MAX_LOG_ENTRIES`] records, and
    /// exit paths call it unconditionally.
    pub(super) dirty: bool,
    /// When the last successful save happened, throttling [`Self::autosave`].
    last_save: Option<std::time::Instant>,
}

/// Shortest interval between two [`HistoryLog::autosave`] writes.
const AUTOSAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

impl HistoryLog {
    /// Records a visit, newest-first, evicting the oldest record if
    /// [`MAX_LOG_ENTRIES`] would be exceeded.
    ///
    /// Re-visiting the page already at the front (a reload, or a Back step
    /// straight after a Forward) refreshes that record in place instead of
    /// stacking duplicates — matching what every browser's history list does
    /// and keeping the sidebar readable.
    pub fn push(&mut self, record: VisitRecord) {
        self.dirty = true;
        if let Some(front) = self.records.front_mut()
            && front.url == record.url
        {
            *front = record;
            return;
        }
        self.records.push_front(record);
        if self.records.len() > MAX_LOG_ENTRIES {
            self.records.pop_back();
        }
    }

    /// Returns the record at `index` in newest-first order.
    pub fn get(&self, index: usize) -> Option<&VisitRecord> {
        self.records.get(index)
    }

    /// Total number of records in the log.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Removes every record from the log. Persist the cleared state by
    /// calling [`Self::save_to_disk`] afterwards.
    pub fn clear(&mut self) {
        self.records.clear();
        self.dirty = true;
    }

    /// Returns groups of records sharing a day-label, newest group first and
    /// newest record first within each group.
    ///
    /// Each element is `(label, records)`, where the records are borrowed
    /// from the log — no copying of URLs or titles for a repaint.
    pub fn grouped_by_day(&self) -> Vec<(String, Vec<&VisitRecord>)> {
        let mut groups: Vec<(String, Vec<&VisitRecord>)> = Vec::new();
        for record in &self.records {
            let label = record.day_label();
            if let Some(last) = groups.last_mut()
                && last.0 == label
            {
                last.1.push(record);
            } else {
                groups.push((label, vec![record]));
            }
        }
        groups
    }

    /// Returns up to `limit` unique history records matching `query`,
    /// prioritizing prefix matches of the URL over substring matches.
    pub fn autocomplete(&self, query: &str, limit: usize) -> Vec<VisitRecord> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower = query.to_lowercase();

        let mut results = Vec::new();
        let mut seen_urls = std::collections::HashSet::new();

        for record in &self.records {
            if seen_urls.contains(&record.url) {
                continue;
            }
            let url_lower = record.url.to_lowercase();
            let title_lower = record.title.to_lowercase();

            let without_scheme = url_lower
                .strip_prefix("mizu://")
                .or_else(|| url_lower.strip_prefix("file://"))
                .or_else(|| url_lower.strip_prefix("https://"))
                .or_else(|| url_lower.strip_prefix("http://"))
                .unwrap_or(&url_lower);

            let is_prefix =
                url_lower.starts_with(&query_lower) || without_scheme.starts_with(&query_lower);

            if is_prefix || url_lower.contains(&query_lower) || title_lower.contains(&query_lower) {
                results.push((is_prefix, record.clone()));
                seen_urls.insert(record.url.clone());
            }
        }

        // Stable sort to keep newest-first order, but prioritize prefix matches.
        results.sort_by(|a, b| match (a.0, b.0) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        });

        results.into_iter().take(limit).map(|(_, r)| r).collect()
    }

    // ── Encrypted persistence ─────────────────────────────────────────────────
    //
    // On-disk format:  `nonce (12 B) || ciphertext || GCM-tag (16 B)`
    // Key storage:     OS keyring via the `keyring` crate (DPAPI on Windows,
    //                  Keychain on macOS, Secret Service on Linux).
    // Key lifetime:    Generated once on first run with the OS CSPRNG
    //                  (`ChaCha20Rng::from_entropy`), then retrieved from the
    //                  keyring on every subsequent launch.
    // Failure policy:  Any error (keyring unavailable, AEAD tag mismatch,
    //                  JSON parse failure) returns an empty log and logs a
    //                  warning — history loss is unfortunate but never fatal.

    /// Returns the path to the encrypted history file (`history.bin`),
    /// creating parent directories if needed.
    fn file_path() -> Option<std::path::PathBuf> {
        let base = data_dir();
        std::fs::create_dir_all(&base).ok()?;
        Some(base.join("history.bin"))
    }

    /// Loads the history log from disk, decrypting with the OS-keyring key.
    ///
    /// Returns an empty log on any error (first run, keyring unavailable,
    /// authentication failure, parse error) — never panics.
    pub fn load_from_disk() -> Self {
        let Some(path) = Self::file_path() else {
            return Self::default();
        };
        let Some(key) = get_or_create_history_key() else {
            return Self::default();
        };
        let Ok(blob) = std::fs::read(&path) else {
            // Normal on first launch — no file yet.
            return Self::default();
        };

        let Some(plaintext) = decrypt_blob(&key, &blob) else {
            // Authentication failure: the file is corrupt or tampered with.
            // The AEAD error itself is deliberately not logged — it says
            // nothing useful and everything it could say is about the key.
            tracing::warn!(
                "history: AEAD authentication failed — file corrupt or tampered; \
                 starting fresh (previous file kept as history.bin.bad)"
            );
            let _ = std::fs::rename(&path, path.with_extension("bin.bad"));
            return Self::default();
        };

        match serde_json::from_slice::<Vec<VisitRecord>>(&plaintext) {
            Ok(records) => Self::from_newest_first(records),
            Err(e) => {
                tracing::warn!(error = %e, "history: JSON parse failed after decrypt; starting fresh");
                Self::default()
            }
        }
    }

    /// Builds a log from records already in newest-first order, keeping the
    /// newest [`MAX_LOG_ENTRIES`] of them.
    pub(super) fn from_newest_first(records: Vec<VisitRecord>) -> Self {
        let mut deque: VecDeque<VisitRecord> = records.into_iter().collect();
        deque.truncate(MAX_LOG_ENTRIES);
        Self {
            records: deque,
            dirty: false,
            last_save: None,
        }
    }

    /// Saves the log if it has changed and [`AUTOSAVE_INTERVAL`] has passed
    /// since the last write.
    ///
    /// Called from the idle handler so that a crash, a power cut, or a kill
    /// costs at most half a minute of history instead of the whole session —
    /// the exit-time save alone would lose all of it.
    pub fn autosave(&mut self) {
        if !self.dirty {
            return;
        }
        let due = self
            .last_save
            .is_none_or(|at| at.elapsed() >= AUTOSAVE_INTERVAL);
        if due {
            self.save_to_disk();
        }
    }

    /// Encrypts the log and writes it atomically to disk, unless nothing has
    /// changed since the last save.
    ///
    /// A failure is logged but never propagated — the browser keeps running
    /// without persistence rather than crashing on the way out.
    pub fn save_to_disk(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = Self::file_path() else {
            tracing::warn!("history: cannot determine data directory; history will not persist");
            return;
        };
        let Some(key) = get_or_create_history_key() else {
            // Warning already emitted inside get_or_create_history_key.
            return;
        };

        // Compact JSON — it is about to be encrypted, so nobody reads it.
        let records: Vec<&VisitRecord> = self.records.iter().collect();
        let plaintext = match serde_json::to_vec(&records) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "history: serialization failed");
                return;
            }
        };
        let Some(blob) = encrypt_blob(&key, &plaintext) else {
            tracing::warn!("history: AES-GCM encryption failed");
            return;
        };

        // Atomic write: fill a sibling `.tmp` file, then rename over the
        // target, so an interrupted save can never truncate real history.
        let tmp = path.with_extension("bin.tmp");
        if let Err(e) = std::fs::write(&tmp, &blob) {
            tracing::warn!(error = %e, "history: failed to write temp file");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            tracing::warn!(error = %e, "history: failed to rename temp file into place");
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        self.dirty = false;
        self.last_save = Some(std::time::Instant::now());
    }
}
