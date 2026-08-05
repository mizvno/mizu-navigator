//! Process-wide constants for the logic worker:
//! `SPAWN_COUNT` and `MAX_WORKER_TABS`.

/// Number of logic-worker threads spawned in this process.
///
/// Test-only instrumentation backing the "opening tabs spawns no threads"
/// guarantee: tabs share one worker, so this must stay flat as tabs are
/// opened. Counted here rather than read from the OS (`/proc/self/task` is
/// Linux-only; this project's primary target is Windows).
///
/// Always compiled rather than `#[cfg(test)]`: the guarantee is asserted from
/// the *navigator* crate's tests, which link this crate in its non-test
/// configuration. One relaxed atomic increment per process is not worth a
/// feature flag.
pub static SPAWN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Hard cap on the number of documents one worker keeps resident.
///
/// Mirrors the UI's own tab cap. The worker enforces it independently because
/// it must not trust its input channel: a `Reload` beyond this bound is logged
/// and dropped rather than growing the map without limit, since each entry
/// pins a whole `VariableStore` plus a frozen interner.
pub const MAX_WORKER_TABS: usize = 32;
