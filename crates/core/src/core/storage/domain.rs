//! [`ValidatedDomain`] (the validated, opaque per-domain identifier) and
//! `mizu_storage_path` (its on-disk location under `%APPDATA%`).

use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::core::errors::MizuError;

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

pub(super) const KEYRING_SERVICE: &str = "mizu_storage";

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
