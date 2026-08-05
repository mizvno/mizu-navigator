//! [`TabSession`]: one document's logic state, and the pure state machine
//! that drives it.
//!
//! # Why this is separate from [`super::worker::LogicWorker`]
//!
//! A `TabSession` is a *single tab's* document state plus the synchronous
//! function that advances it: feed it a [`UiEvent`], get back a
//! [`WorkerResponse`]. It owns no channels, spawns no threads, and knows
//! nothing about how the event reached it or where the response is going.
//!
//! Everything transport- and routing-shaped lives one level up in
//! [`LogicWorker`](super::worker::LogicWorker): the `TabId` → session map,
//! the open-tab ceiling, `CloseTab` (which destroys a session rather than
//! being handled by one), and the `mpsc` endpoints. That split is what lets
//! the same evaluation logic be driven either by the in-process worker thread
//! or, out of process, by an IPC frame loop — neither of which this module
//! has to know exists.
//!
//! # The gesture flag is derived here, not passed in
//!
//! [`WorkerResponse::gesture`] is gate G1's runtime half, and it is computed
//! inside [`TabSession::apply_event`] from the event variant itself —
//! `Click` and `SubmitForm` are user agency, everything else is document
//! agency. No caller supplies it, so no caller can forge it: a transport that
//! wanted to claim a timer tick was a click would have to fabricate a
//! `UiEvent::Click`, which is exactly the thing the UI layer alone can emit.
//! Keeping the derivation in here is what makes that true for the IPC path as
//! much as the thread path.

use std::collections::HashMap;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value, VariableStore};
use crate::messages::{ReloadPayload, StateUpdate, UiEvent, WorkerResponse};
use crate::parser::logic::{
    CompReverseIndex, ComputedBinding, build_comp_reverse_index, recompute_computed_bindings,
};
use crate::parser::{Action, MizuFunction, UrlRegistry, execute_action};

use super::helpers::resolve_endpoint_url;

/// Node budget for a single old-versus-new variable comparison in
/// [`TabSession::build_response`].
///
/// Bounds the cost of change detection independently of the evaluator's
/// instruction budget: this comparison runs once per mutated variable on every
/// update cycle, over values whose size the document controls, so it needs a
/// ceiling of its own rather than a share of one the script is also spending.
pub(super) const EQ_BUDGET_PER_VARIABLE: u64 = 10_000;

/// One open document's complete logic state.
///
/// Construct with [`TabSession::new`] (an empty document, what a tab looks
/// like before its first `Reload`) and advance with
/// [`TabSession::apply_event`].
#[derive(Debug, Default)]
pub struct TabSession {
    /// Global variables plus this document's frozen interner.
    pub store: VariableStore,
    /// Compiled logic functions, keyed by interned name.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// `click -> …` actions, keyed by the triggering node's id.
    pub click_actions: HashMap<u32, Action>,
    /// `submit -> …` actions, keyed by the submitting node's id.
    pub submit_actions: HashMap<u32, Action>,
    /// Root-level `timer …` actions, in declaration order.
    pub root_timer_actions: Vec<Action>,
    /// Compile-time endpoint alias table from the document's `urls` block.
    pub url_registry: UrlRegistry,
    /// Domain of the current document, used to compose `api` endpoint URLs.
    pub document_domain: String,
    /// Computed (derived) bindings, in topological order.
    pub computed_vars: Vec<ComputedBinding>,
    /// Dependency → dependents index, rebuilt once per reload.
    pub computed_reverse_index: CompReverseIndex,
    /// When set, endpoint aliases are left **unresolved** in emitted actions:
    /// `NetworkCall` and `DownloadAlias` are handed out as-is instead of
    /// being converted to `ResolvedCall`/`DownloadMedia`.
    ///
    /// # Why this exists
    ///
    /// Who resolves an alias is a trust question, not a convenience one.
    ///
    /// * **In-process** (`LogicWorker`), the session shares the broker's
    ///   address space and trust domain, so resolving here is free and safe.
    ///   That is the default (`false`).
    ///
    /// * **Out-of-process** (`mizu-worker`), it is not: a URL the worker
    ///   claims to have derived is exactly what a compromised worker would
    ///   forge, so the broker refuses pre-resolved calls outright and insists
    ///   on resolving them itself against its own `UrlRegistry`. A worker
    ///   that resolves anyway produces actions the broker then discards —
    ///   which silently breaks every network request the document makes.
    ///
    /// Set it via [`defer_alias_resolution`](Self::defer_alias_resolution).
    defer_alias_resolution: bool,
}

impl TabSession {
    /// An empty document: what a tab looks like before its first `Reload`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Leaves endpoint aliases unresolved in emitted actions, so the broker
    /// resolves them instead.
    ///
    /// Every out-of-process worker must call this. See the field docs on
    /// [`defer_alias_resolution`](Self#structfield.defer_alias_resolution)
    /// for why resolving in an untrusted process makes the resulting actions
    /// unusable rather than merely redundant.
    pub fn defer_alias_resolution(&mut self) {
        self.defer_alias_resolution = true;
    }

    /// Builds a session directly from a compiled document.
    ///
    /// Equivalent to [`TabSession::new`] followed by
    /// [`apply_reload`](Self::apply_reload), discarding the initial response.
    /// Provided for callers that want a ready-to-use session and do not need
    /// the first state update (the IPC worker does need it, and uses
    /// `apply_reload`).
    #[must_use]
    pub fn from_reload(payload: ReloadPayload) -> Self {
        let mut session = Self::new();
        let _ = session.apply_reload(payload);
        session
    }

    /// Advances this session by one event.
    ///
    /// Returns `None` when the event addressed nothing in this document — a
    /// click on a node with no `click ->` binding, or a timer index past the
    /// end of the declared timers. That is a real outcome and not an error:
    /// the pre-refactor loop sent nothing in those cases, and collapsing it
    /// into an empty `Ok(WorkerResponse)` would turn "nothing happened" into
    /// a state update the UI would process on every stray click.
    ///
    /// Returns `Some(Err(..))` when an action failed. The session's store is
    /// rolled back to its pre-action state first, so a failed action leaves
    /// no partial mutation behind.
    ///
    /// # Panics
    ///
    /// Never on [`UiEvent::CloseTab`] — it returns `None`. Tab destruction is
    /// the owner's job (a session cannot delete itself out of the map that
    /// holds it), so this method treats it as a no-op rather than pretending
    /// to handle it.
    pub fn apply_event(&mut self, event: UiEvent) -> Option<Result<WorkerResponse, MizuError>> {
        match event {
            // Not this type's decision — see the method docs.
            UiEvent::CloseTab => None,

            UiEvent::Reload(payload) => Some(Ok(self.apply_reload(*payload))),

            // ── Gate G1, runtime half ────────────────────────────────────
            // `Click`/`SubmitForm` are emitted only by
            // `dispatch_click_gesture`/`dispatch_form_submit`, i.e. only by a
            // real mouse click, keyboard activation, or form submission — so
            // the event variant *is* the agency, and it travels back on the
            // response that carries this action's consequences. Every other
            // variant is document agency: `RootTimer` fires on a clock,
            // `UpdateVariable` on a network response, `Reload` on document
            // load. None of them may inherit a gesture that happened to
            // arrive nearby in time.
            UiEvent::Click { node_id } => {
                let action = self.click_actions.get(&node_id).cloned()?;
                Some(self.execute_action_and_build(&action, true))
            }

            UiEvent::RootTimer { index } => {
                let action = self.root_timer_actions.get(index as usize).cloned()?;
                Some(self.execute_action_and_build(&action, false))
            }

            UiEvent::SubmitForm {
                submitter_node_id,
                fields,
            } => {
                self.store.evaluator.undo_log.clear();
                // Populate the `$form` magic record first, so the submit
                // action can read `$form.<field>` regardless of whether the
                // individual field names are declared variables.
                self.store.set_runtime(
                    "$form",
                    Value::record_from_unsorted(
                        fields.iter().map(|(k, v)| (k.as_str(), v.clone())),
                    ),
                );
                for (field_name, field_value) in fields {
                    // `set_runtime` (not `set`) so form field names not
                    // declared in the logic block never create new symbols in
                    // the frozen interner. Declared fields are updated
                    // normally; undeclared ones are logged + dropped.
                    self.store.set_runtime(&field_name, field_value);
                }
                // Execute the submit button's declared action (e.g.
                // `submit -> name = $form.who`). Field mutations above must
                // reach the UI even if the action itself fails, so the undo
                // log is NOT cleared here.
                if let Some(action) = self.submit_actions.get(&submitter_node_id).cloned()
                    && let Err(e) = execute_action(&action, &mut self.store, &self.logic_fns)
                {
                    tracing::warn!(error = %e, "form submit action failed");
                }
                self.recompute_after_mutation();
                Some(Ok(self.build_response(true)))
            }

            UiEvent::UpdateVariable { name, value } => {
                self.store.evaluator.undo_log.clear();
                // `name` is a resolved string, not a pre-validated Symbol
                // (see the `UiEvent::UpdateVariable` doc comment): the
                // sender's interner clone and this session's are independent
                // post-freeze, so a Symbol computed on the other side has no
                // defined meaning here. `set_runtime` resolves the name
                // against this session's own frozen table and silently drops
                // it if the document never declared it — the frozen interner
                // is never grown by network-response-driven names.
                self.store.set_runtime(&name, value);
                self.recompute_after_mutation();
                Some(Ok(self.build_response(false)))
            }
        }
    }

    /// Replaces this session's document with `payload`, returning the initial
    /// state update.
    ///
    /// The response carries `gesture: false`: a document load is never user
    /// agency, no matter what the user did to trigger the navigation that
    /// produced it.
    pub fn apply_reload(&mut self, payload: ReloadPayload) -> WorkerResponse {
        self.logic_fns = payload.logic_fns;
        self.click_actions = payload.click_actions;
        self.submit_actions = payload.submit_actions;
        self.root_timer_actions = payload.root_timer_actions;
        self.url_registry = payload.url_registry;
        self.document_domain = payload.document_domain;

        self.store = VariableStore::with_interner(payload.interner);
        // `initial_variables` come from the UI thread's global store; every
        // name is guaranteed to be in the frozen interner already, so
        // `set_runtime` (which uses `get`, not `get_or_intern`) is safe.
        for (k, v) in payload.initial_variables {
            self.store.set_runtime(&k, v);
        }
        self.store.evaluator.undo_log.clear();

        // Load computed bindings and register their symbols as read-only.
        self.computed_vars = payload.computed_bindings;
        // Built once per reload; `recompute_after_mutation` reuses it on
        // every subsequent event instead of rescanning `computed_vars`.
        self.computed_reverse_index = build_comp_reverse_index(&self.computed_vars);
        let comp_syms: FxHashSet<Symbol> =
            self.computed_vars.iter().map(|cb| cb.name).collect();
        self.store.evaluator.computed_var_syms = comp_syms;

        // Initial evaluation of zero-parameter logic functions.
        // `instruction_count` is reset per function so each gets its own budget.
        for (&sym, func) in &self.logic_fns {
            if func.params.is_empty() {
                self.store.evaluator.instruction_count = 0;
                if let Ok(val) = self.store.evaluator.evaluate(
                    func.body.root(),
                    0,
                    &self.logic_fns,
                    &self.store.interner,
                    &func.body.arena,
                ) {
                    self.store.set_symbol(sym, val);
                }
            }
        }

        // Initial evaluation of comp vars: treat every global as mutated.
        let all_syms: FxHashSet<Symbol> =
            self.store.evaluator.global_store.keys().copied().collect();
        let computed = self.computed_vars.clone();
        recompute_computed_bindings(
            &mut self.store,
            &computed,
            &self.logic_fns,
            &all_syms,
            &self.computed_reverse_index,
        );

        self.build_response(false)
    }

    /// Runs one action, rolling the store back to its pre-action state if it
    /// fails, and builds the resulting response.
    fn execute_action_and_build(
        &mut self,
        action: &Action,
        gesture: bool,
    ) -> Result<WorkerResponse, MizuError> {
        self.store.evaluator.undo_log.clear();
        let initial_actions_len = self.store.evaluator.accumulated_actions.len();

        match execute_action(action, &mut self.store, &self.logic_fns) {
            Ok(_) => {
                self.recompute_after_mutation();
                Ok(self.build_response(gesture))
            }
            Err(e) => {
                for (sym, old_val) in self.store.evaluator.undo_log.drain(..).rev() {
                    self.store.evaluator.global_store.insert(sym, old_val);
                }
                self.store
                    .evaluator
                    .accumulated_actions
                    .truncate(initial_actions_len);
                self.store.evaluator.undo_log.clear();
                Err(e)
            }
        }
    }

    /// Re-evaluates this session's computed bindings against the symbols the
    /// last action mutated.
    fn recompute_after_mutation(&mut self) {
        if self.computed_vars.is_empty() {
            return;
        }
        let mutated: FxHashSet<Symbol> = self
            .store
            .evaluator
            .undo_log
            .iter()
            .map(|(sym, _)| *sym)
            .collect();
        let computed = self.computed_vars.clone();
        recompute_computed_bindings(
            &mut self.store,
            &computed,
            &self.logic_fns,
            &mutated,
            &self.computed_reverse_index,
        );
    }

    /// Collects this cycle's variable mutations and resolved actions into a
    /// [`WorkerResponse`].
    ///
    /// Also the point where `NetworkCall` → `ResolvedCall` and
    /// `DownloadAlias` → `DownloadMedia` alias resolution happens, against
    /// this document's own `url_registry`.
    fn build_response(&mut self, gesture: bool) -> WorkerResponse {
        let mut mutated_variables = Vec::new();
        let mut original_values = HashMap::new();
        for &(sym, ref val) in &self.store.evaluator.undo_log {
            original_values.entry(sym).or_insert_with(|| val.clone());
        }
        for (sym, old_val) in original_values {
            let changed = {
                let cur_val = self.store.evaluator.get_global(sym);
                // A budget of its own, per variable, rather than a slice of
                // the evaluator's: change detection is bookkeeping this
                // function does on its own behalf, and charging it to the
                // document's instruction budget would let a large-but-
                // legitimate variable starve the script that produced it.
                let mut eq_budget = 0;
                match old_val.budget_eq(cur_val, &mut eq_budget, EQ_BUDGET_PER_VARIABLE) {
                    Ok(is_eq) => !is_eq,
                    // Undecided within budget. Reporting "changed" is the
                    // safe direction — a spurious update repaints, a missed
                    // one leaves the UI showing a stale value — but it is not
                    // free, so say so rather than letting `unwrap_or(false)`
                    // bury a value that will re-report on every tick from
                    // here on.
                    Err(_) => {
                        tracing::warn!(
                            symbol = sym.0,
                            budget = EQ_BUDGET_PER_VARIABLE,
                            "variable too large to compare within budget; \
                             treating it as mutated on every update"
                        );
                        true
                    }
                }
            };
            if changed {
                let cur_val = self.store.evaluator.get_global(sym).clone();
                mutated_variables.push((sym, cur_val));
            }
        }
        self.store.evaluator.undo_log.clear();

        let raw_actions = std::mem::take(&mut self.store.evaluator.accumulated_actions);
        let mut runtime_actions = Vec::with_capacity(raw_actions.len());
        // Unresolved aliases surface a readable error in the call's bound
        // variable instead of silently dropping the action — the user must
        // see *why* nothing happened.
        let mut alias_errors: Vec<(String, Value)> = Vec::new();
        for action in raw_actions {
            self.resolve_one_action(action, &mut runtime_actions, &mut alias_errors);
        }
        for (name, val) in alias_errors {
            // Only surface into declared variables: the frozen interner must
            // not grow, and an undeclared target could never be displayed.
            if let Some(sym) = self.store.interner.get(&name) {
                self.store.set_runtime(&name, val.clone());
                mutated_variables.push((sym, val));
            }
        }

        WorkerResponse {
            state_update: StateUpdate { mutated_variables },
            runtime_actions,
            gesture,
        }
    }

    /// Resolves one accumulated action's compile-time alias into a concrete
    /// URL, or records a user-visible error if the alias is undeclared.
    ///
    /// Skipped entirely when [`defer_alias_resolution`](Self::defer_alias_resolution)
    /// is set: an out-of-process worker must leave aliases unresolved so the
    /// broker can resolve them itself.
    fn resolve_one_action(
        &self,
        action: crate::messages::RuntimeAction,
        out: &mut Vec<crate::messages::RuntimeAction>,
        alias_errors: &mut Vec<(String, Value)>,
    ) {
        use crate::messages::RuntimeAction;
        if self.defer_alias_resolution {
            // Hand `NetworkCall`/`DownloadAlias` through untouched. Anything
            // this session resolved itself would be indistinguishable, to the
            // broker, from a compromised worker inventing a URL — and would
            // be rejected as such, silently dropping every request the
            // document made.
            out.push(action);
            return;
        }
        match action {
            RuntimeAction::NetworkCall {
                method,
                endpoint_symbol,
                payload,
                path_param,
                target_variable,
                format,
                headers,
            } => {
                let sym = Symbol(endpoint_symbol);
                let Some(ep) = self.url_registry.get(&sym) else {
                    let alias = self
                        .store
                        .interner
                        .resolve(sym)
                        .unwrap_or("<unknown>")
                        .to_owned();
                    tracing::warn!(
                        alias = %alias,
                        target = %target_variable.0,
                        "NetworkCall alias not found in the urls block; surfacing error"
                    );
                    alias_errors.push((
                        target_variable.0.to_string(),
                        Value::from(format!(
                            "error: endpoint alias `{alias}` is not declared in the urls block"
                        )),
                    ));
                    out.push(RuntimeAction::None);
                    return;
                };
                match resolve_endpoint_url(&self.document_domain, ep, path_param.as_deref()) {
                    Ok(url) => out.push(RuntimeAction::ResolvedCall {
                        method: method.as_str().to_owned(),
                        url,
                        payload,
                        target_variable,
                        format,
                        headers,
                    }),
                    Err(e) => {
                        let name = self
                            .store
                            .interner
                            .resolve(target_variable)
                            .unwrap_or("<unknown>")
                            .to_owned();
                        tracing::warn!(
                            target = %name,
                            error = %e,
                            "NetworkCall path_param failed validation; surfacing error"
                        );
                        alias_errors.push((name, Value::from(format!("error: {e}"))));
                        out.push(RuntimeAction::None);
                    }
                }
            }
            RuntimeAction::DownloadAlias { endpoint_symbol } => {
                let sym = Symbol(endpoint_symbol);
                if let Some(ep) = self.url_registry.get(&sym) {
                    out.push(RuntimeAction::DownloadMedia {
                        url: ep.raw_target.clone(),
                    });
                } else {
                    tracing::warn!(
                        endpoint_symbol,
                        "DownloadAlias could not be resolved at runtime"
                    );
                    out.push(RuntimeAction::None);
                }
            }
            other => out.push(other),
        }
    }
}

// ── Evaluator stack budget ───────────────────────────────────────────────────

/// Stack size required by any thread that evaluates document logic.
///
/// `evaluate`/`evaluate_impl` recurse up to [`MAX_EVAL_DEPTH`] levels, and the
/// depth guard only fires *after* one more nested call is already on the
/// stack, so the worst case is ~257 stacked frames of a large, non-tail
/// recursive function. Measured empirically via
/// `core::types::tests::eval::measure_stack_usage_at_max_eval_depth`, which
/// drives a 300-level `evaluate()` chain on threads with a fixed
/// `stack_size`, doubling from 16 KiB until it survives:
///   - debug build:   smallest surviving `stack_size` = 4 MiB
///   - release build: smallest surviving `stack_size` = 256 KiB
///
/// 16 MiB is ~4x the measured debug floor and ~64x the measured release
/// floor — a large margin against interpreter changes, platform stack-frame
/// layout differences, and future growth of `evaluate_impl`'s frame size.
///
/// # Who must honour this
///
/// Every thread that calls [`TabSession::apply_event`], on either side of the
/// process boundary:
///
/// * the in-process `LogicWorker` thread (see `LogicWorker::STACK_SIZE_BYTES`,
///   which aliases this constant), and
/// * the `mizu-worker` binary's evaluation thread.
///
/// It lives here, next to the state machine that does the recursing, rather
/// than on either transport — the requirement is a property of evaluation,
/// and it must outlive any particular way of reaching it.
pub const EVALUATOR_STACK_SIZE_BYTES: usize = 16 * 1024 * 1024;

/// Debug-build stack cost of one `evaluate`/`evaluate_impl` frame pair,
/// derived from the measured debug floor documented on
/// [`EVALUATOR_STACK_SIZE_BYTES`]: 4 MiB survived a chain hitting
/// `MAX_EVAL_DEPTH` (256), which recurses as 257 `evaluate` + 256
/// `evaluate_impl` frames — approximated as `2 * MAX_EVAL_DEPTH` frames for a
/// round, slightly conservative number.
///
/// Exists only so the assertion below can check the coupling at compile time;
/// it is not a general "bytes per Rust stack frame" fact.
const MEASURED_DEBUG_BYTES_PER_FRAME: usize = 8 * 1024;

/// Enforces the coupling documented on [`EVALUATOR_STACK_SIZE_BYTES`]: the
/// stack must stay at least as large as the measured debug-mode floor for the
/// *current* [`MAX_EVAL_DEPTH`]. Neither constant can be derived from the
/// other by a real formula — the per-frame cost is an empirical,
/// compiler/profile-dependent number — but once measured, the *relationship*
/// is exactly checkable, so raising `MAX_EVAL_DEPTH` without raising the stack
/// (or shrinking the stack without shrinking the depth) fails the build
/// instead of silently reintroducing a stack-overflow race with the depth
/// guard.
const _: () = assert!(
    EVALUATOR_STACK_SIZE_BYTES
        >= (crate::core::types::MAX_EVAL_DEPTH as usize) * 2 * MEASURED_DEBUG_BYTES_PER_FRAME,
    "EVALUATOR_STACK_SIZE_BYTES no longer covers the measured debug-mode \
     stack floor for MAX_EVAL_DEPTH frame pairs -- if you changed either \
     constant, re-run core::types::tests::eval::measure_stack_usage_at_max_eval_depth \
     (see its doc comment) and update both together"
);
