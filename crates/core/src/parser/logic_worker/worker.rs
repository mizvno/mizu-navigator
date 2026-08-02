//! [`LogicWorker`]: wraps the `Evaluator` and its per-tab state map, spawns
//! the dedicated background thread, and dispatches each `UiEvent` to the
//! owning tab's state.

use std::sync::mpsc::{Receiver, Sender};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value, VariableStore};
use crate::messages::{TabId, UiEvent, WorkerResponse};
use crate::parser::execute_action;
use crate::parser::logic::{build_comp_reverse_index, recompute_computed_bindings};

use super::helpers::{execute_and_respond, recompute_after_mutation, send_response};
use super::types::{LogicWorkerTabState, MAX_WORKER_TABS, SPAWN_COUNT};

/// LogicWorker thread that wraps the Evaluator and handles evaluations.
///
/// One thread serves every tab: opening a tab allocates a map entry, never a
/// thread. The consequence is head-of-line blocking — a slow evaluation in a
/// background tab delays the foreground tab's next event — which is bounded by
/// the per-event instruction budget in [`crate::core::types::Evaluator`].
pub struct LogicWorker {
    /// Per-document state, keyed by the tab that owns it.
    ///
    /// Every `Symbol` in a message for a given key is meaningful only against
    /// *that* entry's frozen interner; routing by `TabId` (never reused) is
    /// what keeps that sound with several documents resident.
    tabs: FxHashMap<TabId, LogicWorkerTabState>,
    /// Receiving channel for UI events, tagged with their destination tab.
    rx: Receiver<(TabId, UiEvent)>,
    /// Sending channel for state updates, capability actions, or timeout
    /// errors, tagged with the tab that produced them.
    tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
}

impl LogicWorker {
    /// Explicit stack size for the dedicated evaluator thread, overriding the
    /// platform default (commonly ~1 MiB on Windows, ~2–8 MiB on Linux/macOS
    /// depending on `ulimit`/pthread defaults).
    ///
    /// `evaluate`/`evaluate_impl` recurse up to `MAX_EVAL_DEPTH` (256) levels
    /// deep (see [`crate::core::types::MAX_EVAL_DEPTH`]), and the depth guard
    /// itself only fires *after* one more nested call is already on the
    /// stack, so the worst case is ~257 stacked frames of a large, non-tail
    /// recursive function. Measured empirically via
    /// `core::types::tests::measure_stack_usage_at_max_eval_depth`, which
    /// drives a 300-level `evaluate()` chain (the same shape used by
    /// `core::types::tests::cross_function_composition_depth_guard`, which
    /// first caught this exact production gap: on the platform default stack
    /// size it crashed with a native stack overflow in debug builds before
    /// the depth guard could intervene) on threads with a fixed
    /// `stack_size`, doubling from 16 KiB until it survives:
    ///   - debug build:   smallest surviving `stack_size` = 4 MiB
    ///   - release build: smallest surviving `stack_size` = 256 KiB
    ///
    /// 16 MiB is ~4x the measured debug floor and ~64x the measured release
    /// floor, and matches the value `cross_function_composition_depth_guard`'s
    /// sibling test (`eval_depth_guard`) already relies on as proven-safe —
    /// a large margin against interpreter changes, platform stack-frame
    /// layout differences, and future growth of `evaluate_impl`'s frame size.
    ///
    /// Shared by every tab, and correctly so: recursion depth is a property of
    /// the single event being evaluated, not of how many documents are
    /// resident, and events are processed one at a time on this thread.
    pub const STACK_SIZE_BYTES: usize = 16 * 1024 * 1024;

    /// Spawns a permanent native thread executing the LogicWorker.
    ///
    /// Fails only if the OS refuses the thread/stack allocation (real
    /// resource exhaustion) — propagated as [`MizuError::IoError`] instead of
    /// panicking, so the caller can surface a real error instead of aborting
    /// the process on an opaque message.
    pub fn spawn(
        rx: Receiver<(TabId, UiEvent)>,
        tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    ) -> Result<std::thread::JoinHandle<()>, MizuError> {
        SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let handle = std::thread::Builder::new()
            .name("logic-worker".to_owned())
            .stack_size(Self::STACK_SIZE_BYTES)
            .spawn(move || {
                let mut worker = Self {
                    tabs: FxHashMap::default(),
                    rx,
                    tx,
                };
                worker.run_loop();
            })?;
        Ok(handle)
    }

    fn run_loop(&mut self) {
        while let Ok((tab_id, event)) = self.rx.recv() {
            // `Reload` creates a tab's state; `CloseTab` destroys it. Every
            // other event addresses state that must already exist — if it does
            // not, the tab was closed while the event was in flight and the
            // event is dropped. Never fall back to "some other tab": that is
            // exactly the cross-document write this routing exists to prevent.
            if matches!(event, UiEvent::CloseTab) {
                self.tabs.remove(&tab_id);
                continue;
            }
            if matches!(event, UiEvent::Reload(_))
                && !self.tabs.contains_key(&tab_id)
                && self.tabs.len() >= MAX_WORKER_TABS
            {
                tracing::warn!(
                    tab = tab_id.0,
                    open = self.tabs.len(),
                    "refusing Reload: worker tab limit reached"
                );
                continue;
            }
            let tx = &self.tx;
            let tab = if matches!(event, UiEvent::Reload(_)) {
                self.tabs
                    .entry(tab_id)
                    .or_insert_with(LogicWorkerTabState::new)
            } else {
                match self.tabs.get_mut(&tab_id) {
                    Some(t) => t,
                    None => {
                        tracing::debug!(tab = tab_id.0, "event for unknown tab; dropped");
                        continue;
                    }
                }
            };
            match event {
                UiEvent::CloseTab => unreachable!("handled above"),
                UiEvent::Reload(payload) => {
                    tab.logic_fns = payload.logic_fns;
                    tab.click_actions = payload.click_actions;
                    tab.submit_actions = payload.submit_actions;
                    tab.root_timer_actions = payload.root_timer_actions;
                    tab.url_registry = payload.url_registry;
                    tab.document_domain = payload.document_domain;

                    tab.store = VariableStore::with_interner(payload.interner);
                    // `initial_variables` are from the UI thread's global store; every
                    // name is guaranteed to be in the frozen interner already, so
                    // set_runtime (which uses get not get_or_intern) is safe.
                    for (k, v) in payload.initial_variables {
                        tab.store.set_runtime(&k, v);
                    }
                    tab.store.evaluator.undo_log.clear();

                    // Load computed bindings and register their symbols as read-only.
                    tab.computed_vars = payload.computed_bindings;
                    // Built once per reload; `recompute_after_mutation` reuses it on
                    // every subsequent event instead of rescanning `computed_vars`.
                    tab.computed_reverse_index = build_comp_reverse_index(&tab.computed_vars);
                    let comp_syms: FxHashSet<Symbol> =
                        tab.computed_vars.iter().map(|cb| cb.name).collect();
                    tab.store.evaluator.computed_var_syms = comp_syms;

                    // Initial evaluation of zero-parameter logic functions.
                    // instruction_count is reset per function so each gets its own budget.
                    for (&sym, func) in &tab.logic_fns {
                        if func.params.is_empty() {
                            tab.store.evaluator.instruction_count = 0;
                            if let Ok(val) = tab.store.evaluator.evaluate(
                                func.body.root(),
                                0,
                                &tab.logic_fns,
                                &tab.store.interner,
                                &func.body.arena,
                            ) {
                                tab.store.set_symbol(sym, val);
                            }
                        }
                    }

                    // Initial evaluation of comp vars: treat every global as mutated.
                    let all_syms: FxHashSet<Symbol> =
                        tab.store.evaluator.global_store.keys().copied().collect();
                    let computed = tab.computed_vars.clone();
                    recompute_computed_bindings(
                        &mut tab.store,
                        &computed,
                        &tab.logic_fns,
                        &all_syms,
                        &tab.computed_reverse_index,
                    );

                    send_response(tab, tx, tab_id, false);
                }

                // Gate G1, runtime half: `Click`/`SubmitForm` are emitted only
                // by `dispatch_click_gesture`/`dispatch_form_submit`, i.e.
                // only by a real mouse click, keyboard activation, or form
                // submission — so the event variant *is* the agency, and it
                // travels back on the response that carries this action's
                // consequences. Every other variant is document agency:
                // `RootTimer` fires on a clock, `UpdateVariable` on a network
                // response, `Reload` on document load. None of them may
                // inherit a gesture that happened to arrive nearby in time.
                UiEvent::Click { node_id } => {
                    if let Some(action) = tab.click_actions.get(&node_id).cloned() {
                        execute_and_respond(tab, tx, tab_id, &action, true);
                    }
                }

                UiEvent::RootTimer { index } => {
                    if let Some(action) = tab.root_timer_actions.get(index as usize).cloned() {
                        execute_and_respond(tab, tx, tab_id, &action, false);
                    }
                }

                UiEvent::SubmitForm {
                    submitter_node_id,
                    fields,
                } => {
                    tab.store.evaluator.undo_log.clear();
                    // Populate the `$form` magic record first, so the submit
                    // action can read `$form.<field>` regardless of whether
                    // the individual field names are declared variables.
                    tab.store.set_runtime(
                        "$form",
                        Value::record_from_unsorted(
                            fields.iter().map(|(k, v)| (k.as_str(), v.clone())),
                        ),
                    );
                    for (field_name, field_value) in fields {
                        // Use set_runtime (not set) so that form field names
                        // not declared in the logic block never create new
                        // symbols in the frozen interner.  Declared fields are
                        // updated normally; undeclared ones are logged + dropped.
                        tab.store.set_runtime(&field_name, field_value);
                    }
                    // Execute the submit button's declared action (e.g.
                    // `submit -> name = $form.who`).  Field mutations above
                    // must reach the UI even if the action itself fails, so
                    // the undo log is NOT cleared here.
                    if let Some(action) = tab.submit_actions.get(&submitter_node_id).cloned()
                        && let Err(e) = execute_action(&action, &mut tab.store, &tab.logic_fns)
                    {
                        tracing::warn!(error = %e, "form submit action failed");
                    }
                    recompute_after_mutation(tab);
                    send_response(tab, tx, tab_id, true);
                }

                UiEvent::UpdateVariable { name, value } => {
                    tab.store.evaluator.undo_log.clear();
                    // `name` is a resolved string, not a pre-validated Symbol
                    // (see the UiEvent::UpdateVariable doc comment): the
                    // sender's interner clone and this worker's are
                    // independent post-freeze, so a Symbol computed on the
                    // other side has no defined meaning here. set_runtime
                    // resolves the name against this worker's own frozen
                    // table and silently drops it if the document never
                    // declared it — the frozen interner is never grown by
                    // network-response-driven names.
                    tab.store.set_runtime(&name, value);
                    recompute_after_mutation(tab);
                    send_response(tab, tx, tab_id, false);
                }
            }
        }
    }
}

/// Node budget for a single old-versus-new variable comparison in
/// [`send_response`].
///
/// Bounds the cost of change detection independently of the evaluator's
/// instruction budget: this comparison runs once per mutated variable on every
/// update cycle, over values whose size the document controls, so it needs a
/// ceiling of its own rather than a share of one the script is also spending.
pub(super) const EQ_BUDGET_PER_VARIABLE: u64 = 10_000;
