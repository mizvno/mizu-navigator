//! Logic worker module executing the state machine on a dedicated background thread.

#![forbid(unsafe_code)]

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value, VariableStore};
use crate::network::RuntimeAction;
use crate::network::messages::{StateUpdate, UiEvent, WorkerResponse};
use crate::parser::logic::{ComputedBinding, recompute_computed_bindings};
use crate::parser::{Action, EndpointKind, MizuFunction, UrlEndpoint, UrlRegistry, execute_action};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

/// LogicWorker thread that wraps the StateMachine and handles evaluations.
pub struct LogicWorker {
    /// Crate-internal variable store.
    pub store: VariableStore,
    /// Logic functions mapped by symbol.
    pub logic_fns: FxHashMap<Symbol, MizuFunction>,
    /// Click action mappings for layout nodes.
    pub click_actions: HashMap<u32, Action>,
    /// Timer (every) action mappings for layout nodes.
    pub every_actions: HashMap<u32, Action>,
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
    /// Receiving channel for UI events.
    rx: Receiver<UiEvent>,
    /// Sending channel for state updates, capability actions, or timeout errors.
    tx: Sender<Result<WorkerResponse, MizuError>>,
}

impl LogicWorker {
    /// Spawns a permanent native thread executing the LogicWorker.
    pub fn spawn(
        rx: Receiver<UiEvent>,
        tx: Sender<Result<WorkerResponse, MizuError>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut worker = Self {
                store: VariableStore::new(),
                logic_fns: FxHashMap::default(),
                click_actions: HashMap::new(),
                every_actions: HashMap::new(),
                submit_actions: HashMap::new(),
                root_timer_actions: Vec::new(),
                url_registry: FxHashMap::default(),
                document_domain: String::new(),
                computed_vars: Vec::new(),
                rx,
                tx,
            };
            worker.run_loop();
        })
    }

    fn run_loop(&mut self) {
        while let Ok(event) = self.rx.recv() {
            match event {
                UiEvent::Reload(payload) => {
                    self.logic_fns = payload.logic_fns;
                    self.click_actions = payload.click_actions;
                    self.every_actions = payload.every_actions;
                    self.submit_actions = payload.submit_actions;
                    self.root_timer_actions = payload.root_timer_actions;
                    self.url_registry = payload.url_registry;
                    self.document_domain = payload.document_domain;

                    self.store = VariableStore::new();
                    // `payload.interner` is a frozen clone of the UI-thread interner.
                    // Clone now preserves `frozen = true`, so both threads share the
                    // same immutable Symbol(u32) → String mapping after this point.
                    self.store.interner = payload.interner;
                    // `initial_variables` are from the UI thread's global store; every
                    // name is guaranteed to be in the frozen interner already, so
                    // set_runtime (which uses get not get_or_intern) is safe.
                    for (k, v) in payload.initial_variables {
                        self.store.set_runtime(&k, v);
                    }
                    self.store.state_machine.undo_log.clear();

                    // Load computed bindings and register their symbols as read-only.
                    self.computed_vars = payload.computed_bindings;
                    let comp_syms: FxHashSet<Symbol> =
                        self.computed_vars.iter().map(|cb| cb.name).collect();
                    self.store.state_machine.computed_var_syms = comp_syms;

                    // Initial evaluation of zero-parameter logic functions.
                    // instruction_count is reset per function so each gets its own budget.
                    for (&sym, func) in &self.logic_fns {
                        if func.params.is_empty() {
                            self.store.state_machine.instruction_count = 0;
                            if let Ok(val) = self.store.state_machine.evaluate(
                                &func.body,
                                0,
                                &self.logic_fns,
                                &self.store.interner,
                            ) {
                                self.store.set_symbol(sym, val);
                            }
                        }
                    }

                    // Initial evaluation of comp vars: treat every global as mutated.
                    let all_syms: FxHashSet<Symbol> = self
                        .store
                        .state_machine
                        .global_store
                        .keys()
                        .copied()
                        .collect();
                    let computed = self.computed_vars.clone();
                    recompute_computed_bindings(
                        &mut self.store,
                        &computed,
                        &self.logic_fns,
                        &all_syms,
                    );

                    self.send_response();
                }

                UiEvent::Click { node_id } => {
                    if let Some(action) = self.click_actions.get(&node_id).cloned() {
                        self.execute_and_respond(&action);
                    }
                }

                UiEvent::Timer { node_id } => {
                    if let Some(action) = self.every_actions.get(&node_id).cloned() {
                        self.execute_and_respond(&action);
                    }
                }

                UiEvent::RootTimer { index } => {
                    if let Some(action) = self.root_timer_actions.get(index as usize).cloned() {
                        self.execute_and_respond(&action);
                    }
                }

                UiEvent::SubmitForm {
                    submitter_node_id,
                    fields,
                } => {
                    self.store.state_machine.undo_log.clear();
                    // Populate the `$form` magic record first, so the submit
                    // action can read `$form.<field>` regardless of whether
                    // the individual field names are declared variables.
                    let record: std::collections::BTreeMap<std::sync::Arc<str>, Value> = fields
                        .iter()
                        .map(|(k, v)| (std::sync::Arc::from(k.as_str()), v.clone()))
                        .collect();
                    self.store
                        .set_runtime("$form", Value::Record(std::sync::Arc::new(record)));
                    for (field_name, field_value) in fields {
                        // Use set_runtime (not set) so that form field names
                        // not declared in the logic block never create new
                        // symbols in the frozen interner.  Declared fields are
                        // updated normally; undeclared ones are logged + dropped.
                        self.store.set_runtime(&field_name, field_value);
                    }
                    // Execute the submit button's declared action (e.g.
                    // `submit -> name = $form.who`).  Field mutations above
                    // must reach the UI even if the action itself fails, so
                    // the undo log is NOT cleared here.
                    if let Some(action) = self.submit_actions.get(&submitter_node_id).cloned()
                        && let Err(e) = execute_action(&action, &mut self.store, &self.logic_fns)
                    {
                        tracing::warn!(error = %e, "form submit action failed");
                    }
                    self.recompute_after_mutation();
                    self.send_response();
                }

                UiEvent::UpdateVariable { name, value } => {
                    self.store.state_machine.undo_log.clear();
                    // `name` comes from a network response target variable; it must
                    // have been declared in the logic block to be meaningful in the
                    // UI.  Use set_runtime to guard the frozen interner.
                    self.store.set_runtime(&name, value);
                    self.recompute_after_mutation();
                    self.send_response();
                }
            }
        }
    }

    fn send_response(&mut self) {
        let mut mutated_variables = Vec::new();
        let mut original_values = HashMap::new();
        for &(sym, ref val) in &self.store.state_machine.undo_log {
            original_values.entry(sym).or_insert_with(|| val.clone());
        }
        for (sym, old_val) in original_values {
            let cur_val = self.store.state_machine.get_global(sym);
            if &old_val != cur_val
                && let Some(name) = self.store.interner.resolve(sym)
            {
                mutated_variables.push((name.to_string(), cur_val.clone()));
            }
        }
        self.store.state_machine.undo_log.clear();
        // Resolve NetworkCall → ResolvedCall and DownloadAlias → DownloadMedia.
        let document_domain = &self.document_domain;
        let url_registry = &self.url_registry;
        let raw_actions = std::mem::take(&mut self.store.state_machine.accumulated_actions);
        let mut runtime_actions: Vec<RuntimeAction> = Vec::with_capacity(raw_actions.len());
        // Unresolved aliases surface a readable error in the call's bound
        // variable instead of silently dropping the action — the user must
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
                } => {
                    let sym = crate::core::types::Symbol(endpoint_symbol);
                    if let Some(ep) = url_registry.get(&sym) {
                        let url = resolve_endpoint_url(document_domain, ep, path_param.as_deref());
                        runtime_actions.push(RuntimeAction::ResolvedCall {
                            method: method.as_str().to_owned(),
                            url,
                            payload,
                            target_variable,
                        });
                    } else {
                        let alias = self
                            .store
                            .interner
                            .resolve(sym)
                            .unwrap_or("<unknown>")
                            .to_owned();
                        tracing::warn!(
                            alias = %alias,
                            target = %target_variable,
                            "NetworkCall alias not found in the urls block; surfacing error"
                        );
                        alias_errors.push((
                            target_variable,
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
            if self.store.interner.get(&name).is_some() {
                self.store.set_runtime(&name, val.clone());
                mutated_variables.push((name, val));
            }
        }
        if let Err(e) = self.tx.send(Ok(WorkerResponse {
            state_update: StateUpdate { mutated_variables },
            runtime_actions,
        })) {
            tracing::warn!(error = %e, "UI response channel closed; state update dropped");
        }
    }
}


/// Composes the concrete URL for a resolved network call.
///
/// * `Api` endpoints: prepends `mizu://{domain}` to the relative path stored
///   in `raw_target` (which always starts with `/`).
/// * `Media` endpoints: uses `raw_target` as-is (already an absolute `mizu://`
///   URL).
///
/// If `path_param` is `Some` and the URL contains a `{…}` placeholder, the
/// first placeholder is replaced with the param value.  Otherwise the param is
/// appended after a `/`.
pub(crate) fn resolve_endpoint_url(
    document_domain: &str,
    ep: &UrlEndpoint,
    path_param: Option<&str>,
) -> String {
    let base_url = match ep.kind {
        EndpointKind::Api => {
            // raw_target starts with `/`; trim it so there is no double slash.
            let path = ep.raw_target.trim_start_matches('/');
            format!("mizu://{}/{}", document_domain, path)
        }
        EndpointKind::Media => ep.raw_target.clone(),
    };
    if let Some(pp) = path_param {
        // Replace the first `{…}` placeholder if present, otherwise append.
        if let Some(open) = base_url.find('{')
            && let Some(rel_close) = base_url[open..].find('}')
        {
            let close = open + rel_close + 1;
            return format!("{}{}{}", &base_url[..open], pp, &base_url[close..]);
        }
        format!("{}/{}", base_url.trim_end_matches('/'), pp)
    } else {
        base_url
    }
}

impl LogicWorker {
    fn recompute_after_mutation(&mut self) {
        if self.computed_vars.is_empty() {
            return;
        }
        let mutated: FxHashSet<Symbol> = self
            .store
            .state_machine
            .undo_log
            .iter()
            .map(|(sym, _)| *sym)
            .collect();
        let computed = self.computed_vars.clone();
        recompute_computed_bindings(&mut self.store, &computed, &self.logic_fns, &mutated);
    }

    fn execute_and_respond(&mut self, action: &Action) {
        self.store.state_machine.undo_log.clear();
        let initial_actions_len = self.store.state_machine.accumulated_actions.len();

        match execute_action(action, &mut self.store, &self.logic_fns) {
            Ok(_) => {
                self.recompute_after_mutation();
                self.send_response();
            }
            Err(e) => {
                for (sym, old_val) in self.store.state_machine.undo_log.drain(..).rev() {
                    self.store.state_machine.global_store.insert(sym, old_val);
                }
                self.store
                    .state_machine
                    .accumulated_actions
                    .truncate(initial_actions_len);
                self.store.state_machine.undo_log.clear();
                if let Err(send_err) = self.tx.send(Err(e)) {
                    tracing::warn!(error = %send_err, "UI response channel closed; action error dropped");
                }
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
        store.interner.freeze();

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
        store.interner.freeze();

        let frozen_size = store.interner.vec.len();

        store.set_runtime("products", Value::Int(5));
        store.set_runtime("unregistered_response_key", Value::Int(99));

        assert_eq!(
            store.interner.vec.len(),
            frozen_size,
            "interner must not grow via UpdateVariable for unknown names"
        );
        assert_eq!(*store.get("products").unwrap(), Value::Int(5));
        assert!(store.get("unregistered_response_key").is_err());
    }

    #[test]
    fn api_endpoint_gets_full_mizu_url() {
        let url = resolve_endpoint_url("example.com", &api("/v1/products"), None);
        assert_eq!(url, "mizu://example.com/v1/products");
    }

    #[test]
    fn api_path_leading_slash_not_doubled() {
        // raw_target always starts with `/`; the composed URL must not have `//`.
        let url = resolve_endpoint_url("host.mizu", &api("/health"), None);
        assert_eq!(url, "mizu://host.mizu/health");
        assert!(
            !url.contains("//health"),
            "double slash must not appear: {url}"
        );
    }

    #[test]
    fn api_endpoint_path_param_appended_when_no_placeholder() {
        let url = resolve_endpoint_url("example.com", &api("/v1/products"), Some("42"));
        assert_eq!(url, "mizu://example.com/v1/products/42");
    }

    #[test]
    fn api_endpoint_placeholder_substituted() {
        let url = resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("99"));
        assert_eq!(url, "mizu://api.local/v1/items/99");
    }

    #[test]
    fn api_endpoint_nested_placeholder_substituted() {
        let url = resolve_endpoint_url("api.local", &api("/v1/users/{uid}/posts/{pid}"), Some("7"));
        // Only the first placeholder is replaced.
        assert_eq!(url, "mizu://api.local/v1/users/7/posts/{pid}");
    }

    #[test]
    fn media_endpoint_uses_raw_target_unchanged() {
        let url = resolve_endpoint_url(
            "ignored.com",
            &media("mizu://cdn.example.com/logo.png"),
            None,
        );
        assert_eq!(url, "mizu://cdn.example.com/logo.png");
    }

    #[test]
    fn media_endpoint_path_param_appended_when_no_placeholder() {
        let url = resolve_endpoint_url(
            "ignored.com",
            &media("mizu://cdn.example.com/assets"),
            Some("icon.png"),
        );
        assert_eq!(url, "mizu://cdn.example.com/assets/icon.png");
    }
}
