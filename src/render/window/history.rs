//! In-memory session history: a bounded two-stack model for the chrome
//! Back/Forward buttons, plus a larger persistent log that backs the
//! history sidebar panel (ux-history).
//!
//! ## Two separate data structures, deliberately not one
//!
//! * [`HistoryStack`] — per-tab, in-memory only, bounded at
//!   [`MAX_HISTORY_ENTRIES`]. Its [`HistoryEntry`] carries a URL and the
//!   scroll offset to restore, and nothing else: a back/forward step has no
//!   use for a timestamp, and the stack is never serialized.
//!
//! * [`HistoryLog`] — window-level, persisted across launches, bounded at
//!   [`MAX_LOG_ENTRIES`]. Its [`VisitRecord`] carries a URL, the document
//!   title, and a wall-clock timestamp, and has no use for a scroll offset.
//!   One record is appended per *arrival* on a page (see
//!   `super::navigate::handle_navigate_success`), which is what makes the
//!   page currently on screen appear in the sidebar at all.
//!
//! ## Security / privacy note
//!
//! Browsing history is exactly the kind of data a local attacker mines, so
//! the log is encrypted at rest (AES-256-GCM, key in the OS keyring) rather
//! than left as readable JSON — see the persistence section below.
//!
//! ## Deliberately minimal Back/Forward scope (retained from ux-4 guard)
//!
//! A history step is still a full top-level navigation — it must go through
//! the same [`super::navigate::navigate_to_url`] choke point as any other
//! navigation (`SECURITY-INVARIANTS.md` N2); this module only tracks
//! *which URL to navigate to next*, never navigates itself and never stores
//! document state or tainted values.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::SystemTime;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum entries kept per stack (`back` and `forward` independently). Caps
/// memory for a long-lived session; the oldest entry is dropped when the cap
/// is exceeded (consistent with the project's other named, bounded budgets —
/// see `SECURITY-INVARIANTS.md` §2 L1).
pub(crate) const MAX_HISTORY_ENTRIES: usize = 100;

/// Maximum entries in the persistent [`HistoryLog`]. Larger than the
/// back/forward stacks because the log covers the full session history shown
/// in the sidebar, not just undo-able steps.
pub(crate) const MAX_LOG_ENTRIES: usize = 5_000;

// ── HistoryEntry (back/forward) ────────────────────────────────────────────────

/// A single back/forward entry: the resolved URL and the vertical scroll
/// offset at the moment the page was left.
///
/// Deliberately just these two fields — never document state, form values,
/// or anything tainted. Restoring a history entry re-navigates to `url`
/// through the normal navigation choke point exactly like a fresh
/// navigation; `scroll_y` is cosmetic restoration applied after the page
/// reloads.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// The resolved `mizu://` or `file://` URL of the page.
    pub url: String,
    /// Vertical scroll offset (logical pixels) at the moment this page was
    /// left, restored after navigating back/forward to it.
    pub scroll_y: f32,
}

// ── VisitRecord (sidebar log) ─────────────────────────────────────────────────

/// One visited page, as displayed by the history sidebar.
///
/// Carries only what the sidebar renders and groups by. This is the only
/// history type that is ever written to disk, so every field added here is a
/// field persisted about the user's browsing — keep it minimal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisitRecord {
    /// The resolved `mizu://` or `file://` URL of the page.
    pub url: String,
    /// Document title from the `doc "…"` attribute; empty when the document
    /// has none, in which case the sidebar falls back to the URL.
    #[serde(default)]
    pub title: String,
    /// Wall-clock instant of the visit, in seconds since the Unix epoch.
    /// `0` means "unknown" (a record written before this field existed).
    #[serde(default)]
    pub timestamp_secs: u64,
}

/// Seconds since the Unix epoch, or `0` if the system clock is unreadable.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl VisitRecord {
    /// Records a visit to `url` with `title`, stamped with the current time.
    pub fn new(url: String, title: String) -> Self {
        Self {
            url,
            title,
            timestamp_secs: now_secs(),
        }
    }

    /// The label the sidebar shows for this visit: the title when non-empty,
    /// otherwise the URL.
    pub fn display_label(&self) -> &str {
        if self.title.is_empty() {
            &self.url
        } else {
            &self.title
        }
    }

    /// Whole days elapsed since this visit (0 = today, 1 = yesterday, …),
    /// on UTC midnight boundaries.
    ///
    /// UTC rather than local midnight is a deliberate simplification: the
    /// grouping is a convenience, and pulling in a timezone database to move
    /// a label by a few hours is not worth the dependency. A visit with an
    /// unknown timestamp, or one dated in the future by clock skew, counts
    /// as today.
    pub fn days_ago(&self) -> u64 {
        if self.timestamp_secs == 0 {
            return 0;
        }
        const DAY: u64 = 86_400;
        let now = now_secs();
        if now < self.timestamp_secs {
            return 0;
        }
        now / DAY - self.timestamp_secs / DAY
    }

    /// Human-readable day label for the sidebar's group headers.
    pub fn day_label(&self) -> String {
        /// `1 → "1 <unit> ago"`, otherwise `"n <unit>s ago"`.
        fn ago(n: u64, unit: &str) -> String {
            if n == 1 {
                format!("1 {unit} ago")
            } else {
                format!("{n} {unit}s ago")
            }
        }
        match self.days_ago() {
            0 => "Today".to_string(),
            1 => "Yesterday".to_string(),
            d if d < 7 => ago(d, "day"),
            d if d < 30 => ago(d / 7, "week"),
            d if d < 365 => ago(d / 30, "month"),
            d => ago(d / 365, "year"),
        }
    }
}

// ── HistoryStack (per-tab, in-memory back/forward) ────────────────────────────

/// Pushes `entry` onto `stack`, dropping the oldest entry if the push would
/// exceed [`MAX_HISTORY_ENTRIES`].
fn push_capped(stack: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    stack.push(entry);
    if stack.len() > MAX_HISTORY_ENTRIES {
        stack.remove(0);
    }
}

/// The bounded two-stack session history model.
///
/// `back` holds pages navigated away from, oldest first; `forward` holds
/// pages "undone" by a Back step, oldest first — both capped at
/// [`MAX_HISTORY_ENTRIES`] with oldest-first eviction.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HistoryStack {
    back: Vec<HistoryEntry>,
    forward: Vec<HistoryEntry>,
}

impl HistoryStack {
    /// Whether a Back step is available.
    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether a Forward step is available.
    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Records a fresh top-level navigation away from `leaving` (i.e. one
    /// that is neither a history step nor a redirect continuation of one).
    ///
    /// Clears `forward` — standard browser semantics: a fresh navigation
    /// invalidates whatever was "undone". Pushes `leaving` onto `back`,
    /// capped at [`MAX_HISTORY_ENTRIES`].
    pub fn record_navigation(&mut self, leaving: HistoryEntry) {
        self.forward.clear();
        push_capped(&mut self.back, leaving);
    }

    /// Pops the most recent `back` entry to navigate to, pushing `leaving`
    /// (the page being left) onto `forward`.
    ///
    /// Returns `None` — and leaves both stacks untouched — when `back` is
    /// empty, so a click on a disabled Back button is a guaranteed no-op
    /// rather than a silent wrong navigation.
    pub fn go_back(&mut self, leaving: HistoryEntry) -> Option<HistoryEntry> {
        let target = self.back.pop()?;
        push_capped(&mut self.forward, leaving);
        Some(target)
    }

    /// Symmetric to [`Self::go_back`]: pops the most recent `forward` entry,
    /// pushing `leaving` onto `back`.
    pub fn go_forward(&mut self, leaving: HistoryEntry) -> Option<HistoryEntry> {
        let target = self.forward.pop()?;
        push_capped(&mut self.back, leaving);
        Some(target)
    }
}

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
    dirty: bool,
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
    fn from_newest_first(records: Vec<VisitRecord>) -> Self {
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

// ── AEAD envelope ─────────────────────────────────────────────────────────────

/// Length of the AES-GCM nonce prefixed to every stored blob.
const NONCE_LEN: usize = 12;
/// Length of the AES-GCM authentication tag appended by `encrypt`.
const TAG_LEN: usize = 16;

/// Seals `plaintext` under `key` as `nonce || ciphertext || tag`.
///
/// A fresh 96-bit nonce is drawn from the OS CSPRNG on every call: reusing a
/// nonce under one key is the single fatal mistake in GCM, and saving the
/// same log twice must never produce the same blob.
fn encrypt_blob(key: &[u8; 32], plaintext: &[u8]) -> Option<Vec<u8>> {
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
fn decrypt_blob(key: &[u8; 32], blob: &[u8]) -> Option<Vec<u8>> {
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
fn get_or_create_history_key() -> Option<[u8; 32]> {
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
fn get_or_create_history_key() -> Option<[u8; 32]> {
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
fn key_to_hex32(key: &[u8; 32]) -> String {
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
fn hex_to_key32(s: &str) -> Option<[u8; 32]> {
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
fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ── Platform data directory ────────────────────────────────────────────────────

/// Returns the platform-specific application data directory for Mizu.
///
/// | Platform | Path                                       |
/// |----------|--------------------------------------------|
/// | Windows  | `%APPDATA%\mizu`                           |
/// | macOS    | `~/Library/Application Support/mizu`       |
/// | Linux    | `$XDG_DATA_HOME/mizu` or `~/.local/share/mizu` |
fn data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(base).join("mizu")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("mizu")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{home}/.local/share")
        });
        std::path::PathBuf::from(base).join("mizu")
    }
}

// ── HistorySidebarState ───────────────────────────────────────────────────────

/// UI state for the history sidebar panel.
///
/// Window-level, like the panel itself and like [`HistoryLog`]: the sidebar
/// shows every tab's history, so it cannot live on a `TabState` the way the
/// inspector's state does.
#[derive(Debug, Default, Clone)]
pub struct HistorySidebarState {
    /// Whether the panel is currently visible.
    pub open: bool,
    /// Vertical scroll offset of the panel's content (logical pixels).
    pub scroll_offset: f32,
    /// Index (newest-first) of the record under the cursor, if any.
    pub hovered: Option<usize>,
}

impl HistorySidebarState {
    /// Shows or hides the panel, returning the new visibility.
    ///
    /// Opening starts at the top of the list — the newest visits, which is
    /// what a user opening the history is looking for — and closing drops
    /// the hover highlight so it cannot flash stale on the next open.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.scroll_offset = 0.0;
        self.hovered = None;
        self.open
    }

    /// Hides the panel, clearing transient state. A no-op when already
    /// closed, so callers can use it as an unconditional "dismiss".
    pub fn close(&mut self) {
        self.open = false;
        self.scroll_offset = 0.0;
        self.hovered = None;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str) -> HistoryEntry {
        HistoryEntry {
            url: url.to_string(),
            scroll_y: 0.0,
        }
    }

    /// A visit `days` whole days in the past, with no title.
    fn visit(url: &str, days: u64) -> VisitRecord {
        VisitRecord {
            url: url.to_string(),
            title: String::new(),
            timestamp_secs: now_secs() - days * 86_400,
        }
    }

    fn urls(stack: &[HistoryEntry]) -> Vec<&str> {
        stack.iter().map(|e| e.url.as_str()).collect()
    }

    #[test]
    fn navigate_a_b_c_then_back_and_forward() {
        // Pure stack logic, no window needed: navigate A -> B -> C, then
        // walk back to A and forward to B, then a fresh navigation to D.
        let mut h = HistoryStack::default();

        // Start at A, navigate to B: push A.
        h.record_navigation(entry("A"));
        assert_eq!(urls(&h.back), vec!["A"]);
        assert!(!h.can_go_forward());

        // At B, navigate to C: push B.
        h.record_navigation(entry("B"));
        assert_eq!(urls(&h.back), vec!["A", "B"]);
        assert!(!h.can_go_forward());

        // At C, go back: target must be B; back=[A], forward=[C].
        let target = h.go_back(entry("C")).expect("back must be available");
        assert_eq!(target.url, "B");
        assert_eq!(urls(&h.back), vec!["A"]);
        assert_eq!(urls(&h.forward), vec!["C"]);

        // At B, go back again: target must be A; back=[], forward=[C,B].
        let target = h.go_back(entry("B")).expect("back must be available");
        assert_eq!(target.url, "A");
        assert!(urls(&h.back).is_empty());
        assert_eq!(urls(&h.forward), vec!["C", "B"]);

        // At A, go forward: target must be B; back=[A], forward=[C].
        let target = h.go_forward(entry("A")).expect("forward must be available");
        assert_eq!(target.url, "B");
        assert_eq!(urls(&h.back), vec!["A"]);
        assert_eq!(urls(&h.forward), vec!["C"]);

        // At B, a FRESH navigation to D: forward is cleared, back=[A,B].
        h.record_navigation(entry("B"));
        assert_eq!(urls(&h.back), vec!["A", "B"]);
        assert!(
            !h.can_go_forward(),
            "a fresh navigation must clear the forward stack"
        );
    }

    #[test]
    fn go_back_on_empty_stack_is_a_no_op() {
        let mut h = HistoryStack::default();
        assert!(!h.can_go_back());
        let result = h.go_back(entry("current"));
        assert!(result.is_none());
        assert!(
            h.forward.is_empty(),
            "a no-op back must not push onto forward either"
        );
    }

    #[test]
    fn go_forward_on_empty_stack_is_a_no_op() {
        let mut h = HistoryStack::default();
        assert!(!h.can_go_forward());
        let result = h.go_forward(entry("current"));
        assert!(result.is_none());
        assert!(h.back.is_empty());
    }

    #[test]
    fn back_stack_is_capped_oldest_dropped() {
        let mut h = HistoryStack::default();
        for i in 0..(MAX_HISTORY_ENTRIES + 1) {
            h.record_navigation(entry(&format!("page-{i}")));
        }
        assert_eq!(
            h.back.len(),
            MAX_HISTORY_ENTRIES,
            "back stack must be capped"
        );
        assert_eq!(
            h.back.first().unwrap().url,
            "page-1",
            "oldest entry (page-0) must have been dropped"
        );
        assert_eq!(
            h.back.last().unwrap().url,
            format!("page-{MAX_HISTORY_ENTRIES}")
        );
    }

    #[test]
    fn forward_stack_is_capped_oldest_dropped() {
        let mut h = HistoryStack::default();
        // Build a deep back stack, then walk it all the way back to fill forward.
        for i in 0..(MAX_HISTORY_ENTRIES + 1) {
            h.record_navigation(entry(&format!("page-{i}")));
        }
        let mut current = format!("page-{MAX_HISTORY_ENTRIES}");
        while h.can_go_back() {
            let target = h.go_back(entry(&current)).unwrap();
            current = target.url;
        }
        assert_eq!(
            h.forward.len(),
            MAX_HISTORY_ENTRIES,
            "forward stack must be capped"
        );
    }

    #[test]
    fn scroll_y_round_trips_through_a_history_step() {
        let mut h = HistoryStack::default();
        h.record_navigation(HistoryEntry {
            url: "A".to_string(),
            scroll_y: 420.0,
        });
        let target = h
            .go_back(HistoryEntry {
                url: "B".to_string(),
                scroll_y: 0.0,
            })
            .unwrap();
        assert_eq!(target.scroll_y, 420.0);
    }

    // ── HistoryLog tests ──────────────────────────────────────────────────────

    #[test]
    fn log_push_capped_oldest_dropped() {
        let mut log = HistoryLog::default();
        for i in 0..(MAX_LOG_ENTRIES + 5) {
            log.push(visit(&format!("page-{i}"), 0));
        }
        assert_eq!(log.len(), MAX_LOG_ENTRIES);
        assert_eq!(
            log.get(0).unwrap().url,
            format!("page-{}", MAX_LOG_ENTRIES + 4),
            "the newest visit must be at index 0"
        );
    }

    #[test]
    fn log_push_collapses_a_repeat_of_the_current_page() {
        let mut log = HistoryLog::default();
        log.push(VisitRecord::new("A".into(), String::new()));
        log.push(VisitRecord::new("A".into(), "Title".into()));
        assert_eq!(log.len(), 1, "reloading a page must not stack duplicates");
        assert_eq!(
            log.get(0).unwrap().title,
            "Title",
            "the repeat must refresh the record, not be discarded"
        );

        log.push(VisitRecord::new("B".into(), String::new()));
        log.push(VisitRecord::new("A".into(), String::new()));
        assert_eq!(log.len(), 3, "A → B → A is three distinct visits");
    }

    #[test]
    fn log_clear_empties_records() {
        let mut log = HistoryLog::default();
        log.push(visit("A", 0));
        log.push(visit("B", 0));
        assert!(!log.is_empty());
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn log_groups_by_day_newest_group_first() {
        let mut log = HistoryLog::default();
        log.push(visit("old", 2));
        log.push(visit("yesterday", 1));
        log.push(visit("today-a", 0));
        log.push(visit("today-b", 0));

        let groups = log.grouped_by_day();
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["Today", "Yesterday", "2 days ago"]);
        assert_eq!(
            groups[0].1.len(),
            2,
            "same-day visits collapse into one group"
        );
        assert_eq!(
            groups[0].1[0].url, "today-b",
            "the newest visit leads its group"
        );
    }

    #[test]
    fn day_label_is_singular_for_one_unit() {
        let mut log = HistoryLog::default();
        log.push(visit("a", 7));
        log.push(visit("b", 31));
        let labels: Vec<String> = log.grouped_by_day().into_iter().map(|(l, _)| l).collect();
        assert!(
            labels.contains(&"1 week ago".to_string())
                && labels.contains(&"1 month ago".to_string()),
            "got {labels:?}"
        );
    }

    #[test]
    fn encrypted_blob_round_trips_and_detects_tampering() {
        let key = [7u8; 32];
        let records = vec![
            VisitRecord {
                url: "mizu://test/a".into(),
                title: "A".into(),
                timestamp_secs: 1_000,
            },
            VisitRecord {
                url: "mizu://test/b".into(),
                title: String::new(),
                timestamp_secs: 2_000,
            },
        ];
        let plaintext = serde_json::to_vec(&records).unwrap();

        let blob = encrypt_blob(&key, &plaintext).expect("encryption must succeed");
        assert!(
            !blob.windows(5).any(|w| w == b"mizu:"),
            "URLs must not be readable in the stored blob"
        );

        let opened = decrypt_blob(&key, &blob).expect("decryption must succeed");
        let loaded = HistoryLog::from_newest_first(serde_json::from_slice(&opened).unwrap());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get(0).unwrap().url, "mizu://test/a");
        assert_eq!(loaded.get(1).unwrap().timestamp_secs, 2_000);
        assert!(!loaded.dirty, "a freshly loaded log has nothing to save");

        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 0x01;
        assert!(
            decrypt_blob(&key, &tampered).is_none(),
            "a modified blob must fail authentication"
        );
        assert!(
            decrypt_blob(&[8u8; 32], &blob).is_none(),
            "the wrong key must fail authentication"
        );
        assert!(decrypt_blob(&key, b"short").is_none());
    }

    #[test]
    fn encryption_never_reuses_a_nonce() {
        let key = [7u8; 32];
        let a = encrypt_blob(&key, b"same plaintext").unwrap();
        let b = encrypt_blob(&key, b"same plaintext").unwrap();
        assert_ne!(
            a[..NONCE_LEN],
            b[..NONCE_LEN],
            "GCM nonces must be fresh per save"
        );
    }

    #[test]
    fn saving_is_skipped_until_the_log_changes() {
        let mut log = HistoryLog::default();
        assert!(!log.dirty, "an empty log has nothing to write");
        log.push(visit("A", 0));
        assert!(log.dirty);
    }

    #[test]
    fn visit_display_label_prefers_title() {
        let titled = VisitRecord::new("mizu://x/page".into(), "My Page".into());
        assert_eq!(titled.display_label(), "My Page");
        let untitled = VisitRecord::new("mizu://x/other".into(), String::new());
        assert_eq!(untitled.display_label(), "mizu://x/other");
    }

    #[test]
    fn unknown_or_future_timestamps_count_as_today() {
        let unknown = VisitRecord {
            url: "a".into(),
            title: String::new(),
            timestamp_secs: 0,
        };
        assert_eq!(unknown.day_label(), "Today");
        let future = VisitRecord {
            url: "b".into(),
            title: String::new(),
            timestamp_secs: now_secs() + 86_400,
        };
        assert_eq!(
            future.day_label(),
            "Today",
            "clock skew must not invent a group"
        );
    }

    #[test]
    fn sidebar_state_toggle_resets_transient_state() {
        let mut state = HistorySidebarState::default();
        assert!(state.toggle(), "first toggle opens");
        state.scroll_offset = 120.0;
        state.hovered = Some(3);
        assert!(!state.toggle(), "second toggle closes");
        assert_eq!(state.scroll_offset, 0.0);
        assert_eq!(state.hovered, None);
    }
}
