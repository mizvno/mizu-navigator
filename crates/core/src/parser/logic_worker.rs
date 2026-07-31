//! Logic worker module executing the state machine on a dedicated background thread.

#![forbid(unsafe_code)]

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value, VariableStore};
use crate::messages::RuntimeAction;
use crate::messages::{StateUpdate, TabId, UiEvent, WorkerResponse};
use crate::parser::logic::{
    CompReverseIndex, ComputedBinding, build_comp_reverse_index, path_param_ok,
    recompute_computed_bindings,
};
use crate::parser::{Action, EndpointKind, MizuFunction, UrlEndpoint, UrlRegistry, execute_action};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

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
    /// Reverse index (symbol â†’ dependent binding indices) over `computed_vars`,
    /// rebuilt once whenever `computed_vars` is (re)loaded so
    /// `recompute_computed_bindings` never has to scan every binding per event.
    pub computed_reverse_index: CompReverseIndex,
}

impl LogicWorkerTabState {
    /// An empty document: what a tab looks like before its first `Reload`.
    fn new() -> Self {
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
    /// platform default (commonly ~1 MiB on Windows, ~2â€“8 MiB on Linux/macOS
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
    /// sibling test (`eval_depth_guard`) already relies on as proven-safe â€”
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
    /// resource exhaustion) â€” propagated as [`MizuError::IoError`] instead of
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

                    send_response(tab, tx, tab_id);
                }

                UiEvent::Click { node_id } => {
                    if let Some(action) = tab.click_actions.get(&node_id).cloned() {
                        execute_and_respond(tab, tx, tab_id, &action);
                    }
                }

                UiEvent::RootTimer { index } => {
                    if let Some(action) = tab.root_timer_actions.get(index as usize).cloned() {
                        execute_and_respond(tab, tx, tab_id, &action);
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
                    send_response(tab, tx, tab_id);
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
                    // declared it â€” the frozen interner is never grown by
                    // network-response-driven names.
                    tab.store.set_runtime(&name, value);
                    recompute_after_mutation(tab);
                    send_response(tab, tx, tab_id);
                }
            }
        }
    }
}

/// Collects this tab's mutations and resolved actions into one response and
/// sends it back tagged with `tab_id`.
///
/// A free function rather than a method: the caller holds `&mut` on one map
/// entry and `&` on the shared sender, which are disjoint borrows of the
/// worker only when they are passed separately.
fn send_response(
    tab: &mut LogicWorkerTabState,
    tx: &Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    tab_id: TabId,
) {
    let mut mutated_variables = Vec::new();
    let mut original_values = HashMap::new();
    for &(sym, ref val) in &tab.store.evaluator.undo_log {
        original_values.entry(sym).or_insert_with(|| val.clone());
    }
    for (sym, old_val) in original_values {
        let (_is_eq, cur_val_cloned) = {
            let cur_val = tab.store.evaluator.get_global(sym);
            let mut eq_budget = 0;
            let is_eq = old_val.budget_eq(cur_val, &mut eq_budget, 10_000).unwrap_or(false);
            (is_eq, if !is_eq { Some(cur_val.clone()) } else { None })
        };
        if let Some(val) = cur_val_cloned {
            mutated_variables.push((sym, val));
        }
    }
    tab.store.evaluator.undo_log.clear();
    // Resolve NetworkCall â†’ ResolvedCall and DownloadAlias â†’ DownloadMedia.
    let document_domain = &tab.document_domain;
    let url_registry = &tab.url_registry;
    let raw_actions = std::mem::take(&mut tab.store.evaluator.accumulated_actions);
    let mut runtime_actions: Vec<RuntimeAction> = Vec::with_capacity(raw_actions.len());
    // Unresolved aliases surface a readable error in the call's bound
    // variable instead of silently dropping the action â€” the user must
    // see *why* nothing happened.
    let mut alias_errors: Vec<(String, Value)> = Vec::new();
    for action in raw_actions {
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
                let sym = crate::core::types::Symbol(endpoint_symbol);
                if let Some(ep) = url_registry.get(&sym) {
                    match resolve_endpoint_url(document_domain, ep, path_param.as_deref()) {
                        Ok(url) => {
                            runtime_actions.push(RuntimeAction::ResolvedCall {
                                method: method.as_str().to_owned(),
                                url,
                                payload,
                                target_variable,
                                format,
                                headers,
                            });
                        }
                        Err(e) => {
                            let name = tab
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
                            runtime_actions.push(RuntimeAction::None);
                        }
                    }
                } else {
                    let alias = tab
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
                    runtime_actions.push(RuntimeAction::None);
                }
            }
            RuntimeAction::DownloadAlias { endpoint_symbol } => {
                let sym = crate::core::types::Symbol(endpoint_symbol);
                if let Some(ep) = url_registry.get(&sym) {
                    runtime_actions.push(RuntimeAction::DownloadMedia {
                        url: ep.raw_target.clone(),
                    });
                } else {
                    tracing::warn!(
                        endpoint_symbol,
                        "DownloadAlias could not be resolved at runtime"
                    );
                    runtime_actions.push(RuntimeAction::None);
                }
            }
            other => runtime_actions.push(other),
        }
    }
    for (name, val) in alias_errors {
        // Only surface into declared variables: the frozen interner must
        // not grow, and an undeclared target could never be displayed.
        if let Some(sym) = tab.store.interner.get(&name) {
            tab.store.set_runtime(&name, val.clone());
            mutated_variables.push((sym, val.clone()));
        }
    }
    if let Err(e) = tx.send((
        tab_id,
        Ok(WorkerResponse {
            state_update: StateUpdate { mutated_variables },
            runtime_actions,
        }),
    )) {
        tracing::warn!(error = %e, "UI response channel closed; state update dropped");
    }
}

/// Composes the concrete URL for a resolved network call.
///
/// * `Api` endpoints: prepends `mizu://{domain}` to the relative path stored
///   in `raw_target` (which always starts with `/`).
/// * `Media` endpoints: uses `raw_target` as-is (already an absolute `mizu://`
///   URL).
///
/// If `path_param` is `Some` and the URL contains a `{â€¦}` placeholder, the
/// first placeholder is replaced with the percent-encoded param value. Otherwise the
/// encoded param is appended after a `/`. Note: only the first placeholder is replaced;
/// a second `{â€¦}` is left literal (this is the intended behavior).
///
/// `path_param` is re-validated against the same gate as `execute_action` in
/// `logic.rs` before it is ever substituted into the URL â€” this is the last
/// consumption point before the value leaves the process, so it must not be
/// possible to reach this function with an unvalidated `path_param` via a
/// different code path.
pub(crate) fn resolve_endpoint_url(
    document_domain: &str,
    ep: &UrlEndpoint,
    path_param: Option<&str>,
) -> Result<String, MizuError> {
    let base_url = match ep.kind {
        EndpointKind::Api => {
            // raw_target starts with `/`; trim it so there is no double slash.
            let path = ep.raw_target.trim_start_matches('/');
            format!("mizu://{}/{}", document_domain, path)
        }
        EndpointKind::Media => ep.raw_target.clone(),
    };
    if let Some(pp) = path_param {
        if !path_param_ok(pp) {
            return Err(MizuError::ExecutionError(
                "path_param must be a single path segment".to_string(),
            ));
        }
        // Percent-encode the path param
        let mut encoded = String::with_capacity(pp.len());
        for b in pp.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    encoded.push(b as char);
                }
                _ => {
                    encoded.push('%');
                    let hex = b"0123456789ABCDEF";
                    encoded.push(hex[(b >> 4) as usize] as char);
                    encoded.push(hex[(b & 0xF) as usize] as char);
                }
            }
        }
        let pp = &encoded;

        // Replace the first `{â€¦}` placeholder if present, otherwise append.
        if let Some(open) = base_url.find('{')
            && let Some(rel_close) = base_url[open..].find('}')
        {
            let close = open + rel_close + 1;
            return Ok(format!("{}{}{}", &base_url[..open], pp, &base_url[close..]));
        }
        Ok(format!("{}/{}", base_url.trim_end_matches('/'), pp))
    } else {
        Ok(base_url)
    }
}

/// Re-evaluates this tab's computed bindings against the symbols the last
/// action mutated.
fn recompute_after_mutation(tab: &mut LogicWorkerTabState) {
    if tab.computed_vars.is_empty() {
        return;
    }
    let mutated: FxHashSet<Symbol> = tab
        .store
        .evaluator
        .undo_log
        .iter()
        .map(|(sym, _)| *sym)
        .collect();
    let computed = tab.computed_vars.clone();
    recompute_computed_bindings(
        &mut tab.store,
        &computed,
        &tab.logic_fns,
        &mutated,
        &tab.computed_reverse_index,
    );
}

/// Runs one action against `tab` and responds, rolling the tab's store back to
/// its pre-action state if the action fails.
fn execute_and_respond(
    tab: &mut LogicWorkerTabState,
    tx: &Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    tab_id: TabId,
    action: &Action,
) {
    tab.store.evaluator.undo_log.clear();
    let initial_actions_len = tab.store.evaluator.accumulated_actions.len();

    match execute_action(action, &mut tab.store, &tab.logic_fns) {
        Ok(_) => {
            recompute_after_mutation(tab);
            send_response(tab, tx, tab_id);
        }
        Err(e) => {
            for (sym, old_val) in tab.store.evaluator.undo_log.drain(..).rev() {
                tab.store.evaluator.global_store.insert(sym, old_val);
            }
            tab.store
                .evaluator
                .accumulated_actions
                .truncate(initial_actions_len);
            tab.store.evaluator.undo_log.clear();
            if let Err(send_err) = tx.send((tab_id, Err(e))) {
                tracing::warn!(error = %send_err, "UI response channel closed; action error dropped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{StringInterner, Value, VariableStore};
    use crate::parser::urls::{EndpointKind, UrlEndpoint};

    fn api(path: &str) -> UrlEndpoint {
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: path.to_owned(),
        }
    }

    fn media(url: &str) -> UrlEndpoint {
        UrlEndpoint {
            kind: EndpointKind::Media,
            raw_target: url.to_owned(),
        }
    }

    /// Simulates `UiEvent::SubmitForm` with a mix of declared and undeclared
    /// field names.  After the fix, the interner must not grow for undeclared
    /// fields; declared fields must be updated.
    #[test]
    fn submit_form_with_unknown_field_does_not_grow_interner() {
        let mut store = VariableStore::new();
        store.set("username", Value::from("alice"));
        store.set("email", Value::from("alice@mizu"));
        let mut store = store.freeze();

        let frozen_size = store.interner.vec.len();

        let fields = vec![
            ("username".to_string(), Value::from("bob")),
            ("undeclared_field".to_string(), Value::from("ignored")),
            ("email".to_string(), Value::from("bob@mizu")),
        ];
        for (name, val) in fields {
            store.set_runtime(&name, val);
        }

        assert_eq!(
            store.interner.vec.len(),
            frozen_size,
            "interner must not grow when unknown fields arrive via SubmitForm"
        );
        assert_eq!(*store.get("username").unwrap(), Value::from("bob"));
        assert_eq!(*store.get("email").unwrap(), Value::from("bob@mizu"));
        assert!(
            store.get("undeclared_field").is_err(),
            "undeclared form field must not appear in the store"
        );
    }

    /// Simulates `UiEvent::UpdateVariable` for a declared and an undeclared name.
    #[test]
    fn update_variable_with_unknown_name_does_not_grow_interner() {
        let mut store = VariableStore::new();
        store.set("products", Value::Null);
        let mut store = store.freeze();

        let frozen_size = store.interner.vec.len();

        store.set_runtime("products", Value::Decimal(5));
        store.set_runtime("unregistered_response_key", Value::Decimal(99));

        assert_eq!(
            store.interner.vec.len(),
            frozen_size,
            "interner must not grow via UpdateVariable for unknown names"
        );
        assert_eq!(*store.get("products").unwrap(), Value::Decimal(5));
        assert!(store.get("unregistered_response_key").is_err());
    }

    #[test]
    fn api_endpoint_gets_full_mizu_url() {
        let url = resolve_endpoint_url("example.com", &api("/v1/products"), None).unwrap();
        assert_eq!(url, "mizu://example.com/v1/products");
    }

    #[test]
    fn api_path_leading_slash_not_doubled() {
        // raw_target always starts with `/`; the composed URL must not have `//`.
        let url = resolve_endpoint_url("host.mizu", &api("/health"), None).unwrap();
        assert_eq!(url, "mizu://host.mizu/health");
        assert!(
            !url.contains("//health"),
            "double slash must not appear: {url}"
        );
    }

    #[test]
    fn api_endpoint_path_param_appended_when_no_placeholder() {
        let url = resolve_endpoint_url("example.com", &api("/v1/products"), Some("42")).unwrap();
        assert_eq!(url, "mizu://example.com/v1/products/42");
    }

    #[test]
    fn api_endpoint_placeholder_substituted() {
        let url = resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("99")).unwrap();
        assert_eq!(url, "mizu://api.local/v1/items/99");
    }

    #[test]
    fn api_endpoint_nested_placeholder_substituted() {
        let url = resolve_endpoint_url("api.local", &api("/v1/users/{uid}/posts/{pid}"), Some("7"))
            .unwrap();
        // Only the first placeholder is replaced.
        assert_eq!(url, "mizu://api.local/v1/users/7/posts/{pid}");
    }

    #[test]
    fn media_endpoint_uses_raw_target_unchanged() {
        let url = resolve_endpoint_url(
            "ignored.com",
            &media("mizu://cdn.example.com/logo.png"),
            None,
        )
        .unwrap();
        assert_eq!(url, "mizu://cdn.example.com/logo.png");
    }

    #[test]
    fn media_endpoint_path_param_appended_when_no_placeholder() {
        let url = resolve_endpoint_url(
            "ignored.com",
            &media("mizu://cdn.example.com/assets"),
            Some("icon.png"),
        )
        .unwrap();
        assert_eq!(url, "mizu://cdn.example.com/assets/icon.png");
    }

    #[test]
    fn path_param_with_reserved_chars_percent_encoded() {
        let url = resolve_endpoint_url("api.local", &api("/v1/search/{query}"), Some("a b&c?d=1%"))
            .unwrap();
        assert_eq!(url, "mizu://api.local/v1/search/a%20b%26c%3Fd%3D1%25");
    }

    #[test]
    fn path_param_plain_segment_unchanged() {
        let url = resolve_endpoint_url(
            "api.local",
            &api("/v1/items/{id}"),
            Some("foo-bar_123.~baz"),
        )
        .unwrap();
        assert_eq!(url, "mizu://api.local/v1/items/foo-bar_123.~baz");
    }

    #[test]
    fn path_param_with_slash_rejected() {
        let err =
            resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("a/b")).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
    }

    #[test]
    fn path_param_with_traversal_rejected() {
        let err =
            resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("..")).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
    }

    #[test]
    fn path_param_with_control_char_rejected() {
        let err =
            resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("a\nb")).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
    }

    /// End-to-end regression test for the stack-size fix in
    /// `LogicWorker::spawn`: drives a real `LogicWorker` background thread
    /// (spawned exactly as production does, via `LogicWorker::spawn`) through
    /// a 300-level-deep expression â€” the same shape used by
    /// `core::types::tests::eval_depth_guard` and
    /// `cross_function_composition_depth_guard`, deep enough to exceed
    /// `MAX_EVAL_DEPTH` (256) â€” and asserts the worker returns the controlled
    /// "evaluation nesting too deep" error rather than the process crashing
    /// with a native stack overflow.
    ///
    /// Before `LogicWorker::spawn` used an explicit `stack_size`, this same
    /// scenario reliably overflowed the platform-default stack in debug
    /// builds (see `STACK_SIZE_BYTES`'s doc comment for the measurement that
    /// proved it). Because a real stack overflow aborts the whole process and
    /// cannot be caught with `catch_unwind`, this test re-execs the test
    /// binary as a child process and inspects its exit status â€” mirroring
    /// `cross_function_composition_depth_guard` in `core::types`.
    #[test]
    fn logic_worker_thread_survives_max_eval_depth_without_native_crash() {
        const CHILD_ENV: &str = "MIZU_LOGICWORKER_DEPTH_CHILD";
        const OK_MARKER: &str = "LOGICWORKER_DEPTH_GUARD_OK";

        if std::env::var_os(CHILD_ENV).is_some() {
            run_logic_worker_depth_guard_child(OK_MARKER);
            return;
        }

        let exe = std::env::current_exe().expect("current_exe");
        let output = std::process::Command::new(exe)
            .arg("parser::logic_worker::tests::logic_worker_thread_survives_max_eval_depth_without_native_crash")
            .arg("--exact")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_ENV, "1")
            .output()
            .expect("failed to spawn child test process");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success() && stdout.contains(OK_MARKER),
            "LogicWorker's dedicated thread must survive a MAX_EVAL_DEPTH=256+ \
             evaluation without a native stack overflow (status={:?}).\n\
             --- child stdout ---\n{}\n--- child stderr ---\n{}",
            output.status,
            stdout,
            stderr
        );
    }

    /// Runs the actual scenario on the current process: spawns a real
    /// `LogicWorker`, reloads it with a click action bound to a 300-level
    /// deep `BinaryOp` chain, fires the click, and prints `ok_marker` iff the
    /// worker responds with the expected controlled error instead of the
    /// process crashing.
    fn run_logic_worker_depth_guard_child(ok_marker: &str) {
        use crate::messages::{ReloadPayload, TabId, UiEvent};
        use crate::parser::logic::{BinOp, Expr, ExprArena, ExprTree};
        use std::sync::mpsc;

        let (tx_in, rx_in) = mpsc::channel::<(TabId, UiEvent)>();
        let (tx_out, rx_out) = mpsc::channel::<(TabId, Result<WorkerResponse, MizuError>)>();
        let tab = TabId(0);
        let _handle = LogicWorker::spawn(rx_in, tx_out).expect("logic worker thread must spawn");

        // 300-level deep chain: exceeds MAX_EVAL_DEPTH (256), same shape as
        // core::types::tests::eval_depth_guard /
        // cross_function_composition_depth_guard.
        let mut arena = ExprArena::new();
        let mut expr = Expr::Literal(Value::Decimal(0));
        for _ in 0..300 {
            let left = arena.alloc(expr);
            let right = arena.alloc(Expr::Literal(Value::Decimal(0)));
            expr = Expr::BinaryOp {
                left,
                op: BinOp::Add,
                right,
            };
        }
        let root = arena.alloc(expr);

        let mut click_actions = HashMap::new();
        click_actions.insert(0u32, Action::Eval(ExprTree { arena, root }));

        let mut interner = StringInterner::new();
        let interner = interner.freeze();

        tx_in
            .send((
                tab,
                UiEvent::Reload(Box::new(ReloadPayload {
                    logic_fns: FxHashMap::default(),
                    click_actions,
                    submit_actions: HashMap::new(),
                    root_timer_actions: Vec::new(),
                    interner,
                    initial_variables: Vec::new(),
                    url_registry: FxHashMap::default(),
                    document_domain: String::new(),
                    computed_bindings: Vec::new(),
                })),
            ))
            .expect("worker thread must be alive to receive Reload");
        rx_out
            .recv()
            .expect("worker must respond to Reload")
            .1
            .expect("reload must not error");

        tx_in
            .send((tab, UiEvent::Click { node_id: 0 }))
            .expect("worker thread must still be alive after Reload");

        match rx_out.recv().map(|(_, r)| r) {
            Ok(Err(MizuError::ExecutionError(msg))) if msg.contains("nesting too deep") => {
                println!("{ok_marker}");
            }
            // Also acceptable: the instruction budget could in principle be
            // exhausted first depending on constant tuning â€” still a clean,
            // bounded error, not a crash.
            Ok(Err(MizuError::Timeout)) => {
                println!("{ok_marker}");
            }
            other => {
                println!("UNEXPECTED_RESULT: {other:?}");
            }
        }
    }
}
