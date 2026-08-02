//! Per-action worker helpers: `send_response` (collects one tab's mutations
//! and resolved actions into a response), `recompute_after_mutation`
//! (re-evaluates computed bindings after a mutating action), and
//! `execute_and_respond` (runs one action, rolling the store back on error).

use std::sync::mpsc::Sender;

use std::collections::HashMap;

use rustc_hash::FxHashSet;

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value};
use crate::messages::RuntimeAction;
use crate::messages::{StateUpdate, TabId, WorkerResponse};
use crate::parser::logic::{path_param_ok, recompute_computed_bindings};
use crate::parser::{Action, EndpointKind, UrlEndpoint, execute_action};

use super::types::LogicWorkerTabState;
use super::worker::EQ_BUDGET_PER_VARIABLE;

/// sends it back tagged with `tab_id`.
///
/// A free function rather than a method: the caller holds `&mut` on one map
/// entry and `&` on the shared sender, which are disjoint borrows of the
/// worker only when they are passed separately.
pub(super) fn send_response(
    tab: &mut LogicWorkerTabState,
    tx: &Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    tab_id: TabId,
    gesture: bool,
) {
    let mut mutated_variables = Vec::new();
    let mut original_values = HashMap::new();
    for &(sym, ref val) in &tab.store.evaluator.undo_log {
        original_values.entry(sym).or_insert_with(|| val.clone());
    }
    for (sym, old_val) in original_values {
        let changed = {
            let cur_val = tab.store.evaluator.get_global(sym);
            // A budget of its own, per variable, rather than a slice of the
            // evaluator's: change detection is bookkeeping this function does
            // on its own behalf, and charging it to the document's instruction
            // budget would let a large-but-legitimate variable starve the
            // script that produced it.
            let mut eq_budget = 0;
            match old_val.budget_eq(cur_val, &mut eq_budget, EQ_BUDGET_PER_VARIABLE) {
                Ok(is_eq) => !is_eq,
                // Undecided within budget. Reporting "changed" is the safe
                // direction — a spurious update repaints, a missed one leaves
                // the UI showing a stale value — but it is not free, so say so
                // rather than letting `unwrap_or(false)` bury a value that will
                // re-report on every single tick from here on.
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
            let cur_val = tab.store.evaluator.get_global(sym).clone();
            mutated_variables.push((sym, cur_val));
        }
    }
    tab.store.evaluator.undo_log.clear();
    // Resolve NetworkCall → ResolvedCall and DownloadAlias → DownloadMedia.
    let document_domain = &tab.document_domain;
    let url_registry = &tab.url_registry;
    let raw_actions = std::mem::take(&mut tab.store.evaluator.accumulated_actions);
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
            gesture,
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
/// If `path_param` is `Some` and the URL contains a `{…}` placeholder, the
/// first placeholder is replaced with the percent-encoded param value. Otherwise the
/// encoded param is appended after a `/`. Note: only the first placeholder is replaced;
/// a second `{…}` is left literal (this is the intended behavior).
///
/// `path_param` is re-validated against the same gate as `execute_action` in
/// `logic.rs` before it is ever substituted into the URL — this is the last
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

        // Replace the first `{…}` placeholder if present, otherwise append.
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
pub(super) fn recompute_after_mutation(tab: &mut LogicWorkerTabState) {
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
pub(super) fn execute_and_respond(
    tab: &mut LogicWorkerTabState,
    tx: &Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    tab_id: TabId,
    action: &Action,
    gesture: bool,
) {
    tab.store.evaluator.undo_log.clear();
    let initial_actions_len = tab.store.evaluator.accumulated_actions.len();

    match execute_action(action, &mut tab.store, &tab.logic_fns) {
        Ok(_) => {
            recompute_after_mutation(tab);
            send_response(tab, tx, tab_id, gesture);
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
