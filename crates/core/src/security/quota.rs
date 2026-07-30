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
        let window_expired = self.window_start.elapsed().as_secs() >= 1;
        if window_expired {
            self.window_start = Instant::now();
        }
        Self::check_write_budget(
            &mut self.bytes_stored,
            self.quota_bytes,
            &mut self.write_count_this_second,
            window_expired,
            byte_count,
        )
    }

    /// The time-free core of [`check_storage_write`](Self::check_storage_write):
    /// everything the decision depends on, with the clock reduced to the single
    /// `window_expired` bool.
    ///
    /// Split out for the Kani harnesses. `Instant::now()` and
    /// `Instant::elapsed` bottom out in `clock_gettime`, a foreign C call Kani
    /// cannot model — it reports the construct as reachable and fails the
    /// harness outright. `Instant` also has no public constructor, so a harness
    /// cannot even build a `CapabilityPolicy` without calling the clock. Taking
    /// the fields by reference keeps the proofs on the real logic instead of a
    /// reimplementation of it.
    fn check_write_budget(
        bytes_stored: &mut usize,
        quota_bytes: usize,
        write_count_this_second: &mut u32,
        window_expired: bool,
        byte_count: usize,
    ) -> Result<(), MizuError> {
        if window_expired {
            *write_count_this_second = 0;
        }
        if *write_count_this_second >= STORAGE_RATE_LIMIT_WRITES_PER_SEC {
            #[cfg(not(kani))]
            let msg = format!("storage rate limit exceeded: max {STORAGE_RATE_LIMIT_WRITES_PER_SEC} writes/s");
            #[cfg(kani)]
            let msg = String::from("rate limit exceeded");
            return Err(MizuError::SecurityViolation(msg));
        }
        let new_total = bytes_stored.saturating_add(byte_count);
        if new_total > quota_bytes {
            #[cfg(not(kani))]
            let msg = format!("storage quota exceeded: {} / {} bytes", new_total, quota_bytes);
            #[cfg(kani)]
            let msg = String::from("quota exceeded");
            return Err(MizuError::SecurityViolation(msg));
        }
        *bytes_stored = new_total;
        *write_count_this_second += 1;
        Ok(())
    }
}

// Kani harnesses for `check_storage_write` — see `SECURITY-INVARIANTS.md`
// §8, and the L1-budget candidate it mirrors (a monotonically-bounded
// counter).
//
// They drive `check_write_budget` rather than `check_storage_write`: the
// latter reads the clock, and `Instant` is unrepresentable in Kani (see that
// function's doc comment). `window_expired` is symbolic, so both the
// steady-state and the window-reset branch are covered.
//
// Every harness needs an explicit `#[kani::unwind]`, even though
// `check_write_budget` contains no loop. `MizuError::MultipleErrors` holds a
// `Vec<MizuError>`, which makes `drop_in_place::<MizuError>` *recursive*, and
// CBMC unwinds that recursion without a bound — this is what made an
// unannotated harness here run forever rather than fail. Every error these
// harnesses can construct is a leaf variant, so the recursion is one level
// deep on any reachable path and a small bound proves out.
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(2)]
    fn check_storage_write_never_panics() {
        // Use u32 instead of usize: kani::any::<usize>() spans the full 64-bit
        // range and triggers assume(false) in Kani's ToISize model on every
        // path where the value exceeds isize::MAX, killing all proof paths.
        let mut bytes_stored = kani::any::<u32>() as usize;
        let quota_bytes = kani::any::<u32>() as usize;
        let mut write_count_this_second: u32 = kani::any();
        let byte_count = kani::any::<u32>() as usize;
        let window_expired: bool = kani::any();

        let _ = CapabilityPolicy::check_write_budget(
            &mut bytes_stored,
            quota_bytes,
            &mut write_count_this_second,
            window_expired,
            byte_count,
        );
    }

    /// The invariant the quota exists to enforce: a write that is accepted
    /// never leaves the origin over its limit.
    #[kani::proof]
    #[kani::unwind(2)]
    fn check_storage_write_ok_implies_within_quota() {
        // u32 keeps symbolic values within isize::MAX so Kani's ToISize model
        // never kills paths before the proof logic runs.  A
        // `kani::assume(quota_bytes <= 1_000_000)` on a `usize` would be a
        // post-hoc constraint that still lets CBMC spawn and then discard ~2^64
        // paths first.
        let mut bytes_stored = kani::any::<u32>() as usize;
        let quota_bytes = kani::any::<u32>() as usize;
        let mut write_count_this_second: u32 = kani::any();
        let byte_count = kani::any::<u32>() as usize;
        let window_expired: bool = kani::any();

        // Steady state: already within quota before this write.
        kani::assume(bytes_stored <= quota_bytes);

        let accepted = CapabilityPolicy::check_write_budget(
            &mut bytes_stored,
            quota_bytes,
            &mut write_count_this_second,
            window_expired,
            byte_count,
        )
        .is_ok();

        if accepted {
            assert!(bytes_stored <= quota_bytes);
        }
    }

    /// A rejected write leaves no trace: neither the byte counter nor the
    /// rate counter may advance on the error paths, or a caller retrying
    /// after a denial would be charged for writes that never happened.
    #[kani::proof]
    #[kani::unwind(2)]
    fn rejected_write_does_not_advance_counters() {
        let mut bytes_stored = kani::any::<u32>() as usize;
        let quota_bytes = kani::any::<u32>() as usize;
        let mut write_count_this_second: u32 = kani::any();
        let byte_count = kani::any::<u32>() as usize;

        let before_bytes = bytes_stored;
        // The window-reset branch legitimately zeroes the rate counter, so this
        // property is stated for a live window.
        let before_count = write_count_this_second;

        let rejected = CapabilityPolicy::check_write_budget(
            &mut bytes_stored,
            quota_bytes,
            &mut write_count_this_second,
            false,
            byte_count,
        )
        .is_err();

        if rejected {
            assert_eq!(bytes_stored, before_bytes);
            assert_eq!(write_count_this_second, before_count);
        }
    }

    /// The rate limit is a hard ceiling: no accepted write can push the
    /// per-second counter above it.
    #[kani::proof]
    #[kani::unwind(2)]
    fn accepted_write_never_exceeds_rate_limit() {
        let mut bytes_stored = kani::any::<u32>() as usize;
        let quota_bytes = kani::any::<u32>() as usize;
        let mut write_count_this_second: u32 = kani::any();
        let byte_count = kani::any::<u32>() as usize;
        let window_expired: bool = kani::any();

        let accepted = CapabilityPolicy::check_write_budget(
            &mut bytes_stored,
            quota_bytes,
            &mut write_count_this_second,
            window_expired,
            byte_count,
        )
        .is_ok();

        if accepted {
            assert!(write_count_this_second <= STORAGE_RATE_LIMIT_WRITES_PER_SEC);
        }
    }
}
