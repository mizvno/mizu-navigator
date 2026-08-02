//! [`LogicWorkerTabState`]: one document's worker-side state, plus its
//! `SPAWN_COUNT`/`MAX_WORKER_TABS` process-wide constants.

use std::collections::HashMap;

use rustc_hash::FxHashMap;

use crate::core::types::{Symbol, VariableStore};
use crate::parser::logic::{CompReverseIndex, ComputedBinding};
use crate::parser::{Action, MizuFunction, UrlRegistry};

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

/// One document's worker-side state.
///
/// Exactly the fields a `UiEvent::Reload` replaces wholesale, which is why the
/// extraction is mechanical: reloading a tab is `insert`, closing it is
/// `remove`.
pub struct LogicWorkerTabState {
    /// Crate-internal variable store.
    pub store: VariableStore,
    /// Logic functions mapped by symbol.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// Click action mappings for layout nodes.
    pub click_actions: HashMap<u32, Action>,
    /// Submit action mappings, keyed by the submit button's node id.
    pub submit_actions: HashMap<u32, Action>,
    /// Root-level `timer` actions from the `logic` block, in declaration order.
    pub root_timer_actions: Vec<Action>,
    /// URL registry for resolving compile-time endpoint aliases at runtime.
    pub url_registry: UrlRegistry,
    /// Domain of the current document, used to compose `mizu://` URLs for `api` endpoints.
    pub document_domain: String,
    /// Computed (derived) variable bindings in topological order.
    pub computed_vars: Vec<ComputedBinding>,
    /// Reverse index (symbol → dependent binding indices) over `computed_vars`,
    /// rebuilt once whenever `computed_vars` is (re)loaded so
    /// `recompute_computed_bindings` never has to scan every binding per event.
    pub computed_reverse_index: CompReverseIndex,
}

impl LogicWorkerTabState {
    /// An empty document: what a tab looks like before its first `Reload`.
    pub(super) fn new() -> Self {
        Self {
            store: VariableStore::default(),
            logic_fns: FxHashMap::default(),
            click_actions: HashMap::new(),
            submit_actions: HashMap::new(),
            root_timer_actions: Vec::new(),
            url_registry: FxHashMap::default(),
            document_domain: String::new(),
            computed_vars: Vec::new(),
            computed_reverse_index: FxHashMap::default(),
        }
    }
}
