//! Storage quota checking logic.

use crate::core::errors::MizuError;
use std::time::Instant;

/// Maximum bytes a remote-origin document may store on disk (512 KiB).
pub const STORAGE_QUOTA_BYTES_REMOTE: usize = 512 * 1024;
/// Maximum bytes a local-file-origin document may store on disk (1 MiB).
pub const STORAGE_QUOTA_BYTES_LOCAL_FILE: usize = 1024 * 1024;
/// Maximum bytes a localhost document may store on disk (10 MiB).
pub const STORAGE_QUOTA_BYTES_LOCALHOST: usize = 10 * 1024 * 1024;
/// Maximum `StorageStore` writes allowed within a single one-second window.
pub const STORAGE_RATE_LIMIT_WRITES_PER_SEC: u32 = 10;

/// Per-origin capability budget and rate-limiting state.
///
/// One instance lives on `MizuWindowManager` and is
/// reset every time the user navigates to a new URL.
pub struct CapabilityPolicy {
    /// Accumulated storage bytes written by the current origin.
    pub bytes_stored: usize,
    /// Hard quota limit (bytes).  Derived from origin type at construction.
    pub quota_bytes: usize,
    /// Number of storage writes in the current one-second sliding window.
    write_count_this_second: u32,
    /// Start of the current one-second window.
    window_start: Instant,
}

impl CapabilityPolicy {
    /// Creates a fresh policy sized to the origin type of `chrome_url`.
    ///
    /// Quota tier is determined by parsed origin, not by raw substring search:
    /// `mizu://attacker.com/?host=localhost` must NOT receive the localhost quota.
    pub fn new(chrome_url: &str) -> Self {
        let quota_bytes = if chrome_url.starts_with("file://") {
            // file:// origins always get the local-file quota regardless of path content.
            STORAGE_QUOTA_BYTES_LOCAL_FILE
        } else if let Ok(uri) = crate::core::uri::MizuUri::parse(chrome_url) {
            // Use the structurally-extracted domain, not raw substring matching, to
            // avoid `mizu://evil.com?host=localhost` bypassing the remote quota.
            if crate::security::network::is_local_host(&uri.domain) {
                STORAGE_QUOTA_BYTES_LOCALHOST
            } else {
                STORAGE_QUOTA_BYTES_REMOTE
            }
        } else {
            STORAGE_QUOTA_BYTES_REMOTE
        };
        Self {
            bytes_stored: 0,
            quota_bytes,
            write_count_this_second: 0,
            window_start: Instant::now(),
        }
    }

    /// Checks and records a storage write of `byte_count` bytes.
    ///
    /// Advances `bytes_stored` and `write_count_this_second` on success.
    /// Returns [`MizuError::SecurityViolation`] if either the rate limit or
    /// the total quota would be exceeded.
    pub fn check_storage_write(&mut self, byte_count: usize) -> Result<(), MizuError> {
        if self.window_start.elapsed().as_secs() >= 1 {
            self.write_count_this_second = 0;
            self.window_start = Instant::now();
        }
        if self.write_count_this_second >= STORAGE_RATE_LIMIT_WRITES_PER_SEC {
            return Err(MizuError::SecurityViolation(format!(
                "storage rate limit exceeded: max {STORAGE_RATE_LIMIT_WRITES_PER_SEC} writes/s"
            )));
        }
        let new_total = self.bytes_stored.saturating_add(byte_count);
        if new_total > self.quota_bytes {
            return Err(MizuError::SecurityViolation(format!(
                "storage quota exceeded: {} / {} bytes",
                new_total, self.quota_bytes
            )));
        }
        self.bytes_stored = new_total;
        self.write_count_this_second += 1;
        Ok(())
    }
}

// Kani harnesses for `check_storage_write` — see `SECURITY-INVARIANTS.md`
// §8, and the L1-budget candidate it mirrors (a monotonically-bounded
// counter). `CapabilityPolicy` is constructed directly (same-module access
// to its private fields) with a concrete `Instant::now()` — deterministically
// keeping `elapsed() < 1s` true so the harness exercises the steady-state
// rate/quota check rather than the window-reset branch, which is simple
// enough (`write_count_this_second = 0`) not to need its own proof.
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn check_storage_write_never_panics() {
        let policy_bytes_stored: usize = kani::any();
        let quota_bytes: usize = kani::any();
        let write_count_this_second: u32 = kani::any();
        let byte_count: usize = kani::any();
        let mut policy = CapabilityPolicy {
            bytes_stored: policy_bytes_stored,
            quota_bytes,
            write_count_this_second,
            window_start: Instant::now(),
        };
        let _ = policy.check_storage_write(byte_count);
    }

    #[kani::proof]
    fn check_storage_write_ok_implies_within_quota() {
        let bytes_stored: usize = kani::any();
        let quota_bytes: usize = kani::any();
        let write_count_this_second: u32 = kani::any();
        let byte_count: usize = kani::any();

        // Steady state: already within quota/rate-limit before this write.
        // Bounds keep the arithmetic small and tractable for CBMC.
        kani::assume(quota_bytes <= 1_000_000);
        kani::assume(bytes_stored <= quota_bytes);
        kani::assume(byte_count <= 1_000_000);

        let mut policy = CapabilityPolicy {
            bytes_stored,
            quota_bytes,
            write_count_this_second,
            window_start: Instant::now(),
        };

        if policy.check_storage_write(byte_count).is_ok() {
            assert!(policy.bytes_stored <= policy.quota_bytes);
        }
    }
}
