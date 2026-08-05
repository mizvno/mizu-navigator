//! # `wire::rehydrate` — untrusted wire → owned core types
//!
//! Phase 1 built the *outbound* direction (`&ReloadPayload` →
//! `WireReloadPayload`): a trusted broker describing a document it just
//! compiled. This module is the *inbound* direction, and the trust polarity
//! is reversed — these functions run in the worker on bytes that arrived over
//! IPC, and on the broker on a `WireWorkerEnvelope` an untrusted worker
//! produced.
//!
//! That is why every function here returns `Result` while its Phase 1
//! counterpart is a plain `From`. rkyv's `bytecheck` pass guarantees the
//! bytes decode to well-formed *Rust* values — a `u32` really is a `u32`, a
//! `Vec` length matches its data — but it knows nothing about Mizu's own
//! invariants. It cannot tell that `ExprId(9)` is meaningless in a 3-node
//! arena, that a `Symbol` must index the interner, or that an
//! `args_start + args_len` window must lie inside the argument pool. Those
//! are exactly the properties an attacker would forge, and each one is
//! re-derived here rather than assumed.
//!
//! ## The arena is rebuilt, not trusted
//!
//! [`rehydrate_expr_tree`] mints child references with
//! [`ExprId::from_index_unvalidated`] (unavoidable: nodes reference each
//! other by index, including forward references) and then calls
//! [`ExprArena::validate_references`] on the finished arena. Until that call
//! returns `Ok`, the arena can contain ids that would panic the evaluator;
//! after it, indexing is total. No path in this module returns an arena that
//! skipped that check.

#![forbid(unsafe_code)]

use std::sync::Arc;

use mizu_core::core::errors::MizuError;
use mizu_core::core::types::{FrozenInterner, Symbol, Value};
use mizu_core::parser::logic::{
    ComputedBinding, Expr, ExprArena, ExprId, ExprTree, NetworkMethod, PayloadFormat, ValueType,
};
use mizu_core::parser::{Action, EndpointKind, MizuFunction, UrlEndpoint, UrlRegistry};

use crate::wire::reload::{
    WireAction, WireBinOp, WireComputedBinding, WireExpr, WireExprTree, WireMizuFunction,
    WireNetworkMethodReload, WirePayloadFormatReload, WireReloadPayload, WireUrlEndpoint,
    WireUrlEndpointKind, WireValueType,
};
use crate::wire::value::WireValue;

/// Ceiling on nodes in a single rehydrated expression tree.
///
/// The archive is already length-capped by the frame limit, but a 64 MiB
/// frame can still describe millions of tiny nodes. This bound is what keeps
/// a single malicious `Reload` from turning into an unbounded allocation
/// inside the worker, independently of how the bytes got here.
pub const MAX_ARENA_NODES: usize = 1 << 20;

fn err(msg: impl Into<String>) -> MizuError {
    MizuError::ParseError(msg.into())
}

// ── Leaf types ───────────────────────────────────────────────────────────────

fn rehydrate_binop(op: &WireBinOp) -> mizu_core::parser::logic::BinOp {
    use mizu_core::parser::logic::BinOp;
    match op {
        WireBinOp::Add => BinOp::Add,
        WireBinOp::Sub => BinOp::Sub,
        WireBinOp::Mul => BinOp::Mul,
        WireBinOp::Div => BinOp::Div,
        WireBinOp::Eq => BinOp::Eq,
        WireBinOp::Ne => BinOp::Ne,
        WireBinOp::Lt => BinOp::Lt,
        WireBinOp::Gt => BinOp::Gt,
        WireBinOp::Le => BinOp::Le,
        WireBinOp::Ge => BinOp::Ge,
        WireBinOp::And => BinOp::And,
        WireBinOp::Or => BinOp::Or,
    }
}

fn rehydrate_method(m: &WireNetworkMethodReload) -> NetworkMethod {
    match m {
        WireNetworkMethodReload::Get => NetworkMethod::Get,
        WireNetworkMethodReload::Post => NetworkMethod::Post,
        WireNetworkMethodReload::Put => NetworkMethod::Put,
        WireNetworkMethodReload::Delete => NetworkMethod::Delete,
        WireNetworkMethodReload::Query => NetworkMethod::Query,
    }
}

fn rehydrate_format(f: &WirePayloadFormatReload) -> PayloadFormat {
    match f {
        WirePayloadFormatReload::Json => PayloadFormat::Json,
        WirePayloadFormatReload::Form => PayloadFormat::Form,
        WirePayloadFormatReload::Text => PayloadFormat::Text,
        WirePayloadFormatReload::Yaml => PayloadFormat::Yaml,
        WirePayloadFormatReload::Multipart => PayloadFormat::Multipart,
    }
}

/// `ValueType` is recursive, so this mirrors the depth guard the parser
/// applies: a deeply-nested type annotation would otherwise recurse until the
/// stack gives out.
fn rehydrate_value_type(vt: &WireValueType, depth: u32) -> Result<ValueType, MizuError> {
    const MAX_TYPE_DEPTH: u32 = 64;
    if depth > MAX_TYPE_DEPTH {
        return Err(err(format!(
            "type annotation nested deeper than {MAX_TYPE_DEPTH} levels"
        )));
    }
    Ok(match vt {
        WireValueType::Num => ValueType::Num,
        WireValueType::Str => ValueType::Str,
        WireValueType::Bool => ValueType::Bool,
        WireValueType::List(inner) => {
            ValueType::List(Box::new(rehydrate_value_type(inner, depth + 1)?))
        }
        WireValueType::Nullable(inner) => {
            ValueType::Nullable(Box::new(rehydrate_value_type(inner, depth + 1)?))
        }
        WireValueType::Record {
            field_names,
            field_types,
        } => {
            if field_names.len() != field_types.len() {
                return Err(err(format!(
                    "record type has {} field names but {} field types",
                    field_names.len(),
                    field_types.len()
                )));
            }
            let mut fields = Vec::with_capacity(field_names.len());
            for (name, ty) in field_names.iter().zip(field_types.iter()) {
                fields.push((Arc::from(name.as_str()), rehydrate_value_type(ty, depth + 1)?));
            }
            ValueType::Record(fields)
        }
    })
}

// ── Expression arena ─────────────────────────────────────────────────────────

/// Rebuilds one [`ExprTree`] from its flat wire encoding, then validates
/// every reference before handing it back.
///
/// The nodes are allocated in index order, so `alloc`'s sequential ids line
/// up exactly with the wire's indices; the whole argument pool is pushed as
/// one contiguous block so each `FunctionCall`'s `(start, len)` window keeps
/// the meaning it had on the sending side.
pub fn rehydrate_expr_tree(tree: &WireExprTree) -> Result<ExprTree, MizuError> {
    if tree.nodes.len() > MAX_ARENA_NODES {
        return Err(err(format!(
            "expression arena has {} nodes, exceeding the {MAX_ARENA_NODES} limit",
            tree.nodes.len()
        )));
    }

    let mut arena = ExprArena::new();

    // Pool first: `push_args` is the only way to fill it, and pushing it as a
    // single block makes the returned start index 0, preserving every
    // window the sender recorded.
    if !tree.args_pool.is_empty() {
        let ids: Vec<ExprId> = tree
            .args_pool
            .iter()
            .map(|i| ExprId::from_index_unvalidated(*i))
            .collect();
        let (start, len) = arena.push_args(&ids)?;
        if start != 0 || len as usize != tree.args_pool.len() {
            return Err(err(
                "argument pool did not round-trip contiguously from index 0",
            ));
        }
    }

    for (i, node) in tree.nodes.iter().enumerate() {
        let expr = rehydrate_expr(node)?;
        let id = arena.alloc(expr);
        // `alloc` is documented as append-only and sequential; assert the
        // property this reconstruction depends on rather than trusting it
        // silently, so a future change to `alloc` fails loudly here instead
        // of producing a subtly mis-linked tree.
        if id.index() as usize != i {
            return Err(err(format!(
                "arena allocation returned id {} for node {i}; \
                 sequential allocation is required for index-based rehydration",
                id.index()
            )));
        }
    }

    // Restores the invariant `ExprId::from_index_unvalidated` suspended.
    arena.validate_references()?;

    let node_count = tree.nodes.len() as u32;
    if tree.root >= node_count {
        return Err(err(format!(
            "expression tree root is node {} but the arena holds only {node_count} nodes",
            tree.root
        )));
    }

    Ok(ExprTree {
        arena,
        root: ExprId::from_index_unvalidated(tree.root),
    })
}

fn rehydrate_expr(node: &WireExpr) -> Result<Expr, MizuError> {
    let id = ExprId::from_index_unvalidated;
    Ok(match node {
        WireExpr::Literal(v) => Expr::Literal(rehydrate_value(v, 0)?),
        WireExpr::Variable(sym) => Expr::Variable(Symbol(*sym)),
        WireExpr::Not(inner) => Expr::Not(id(*inner)),
        WireExpr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: id(*left),
            op: rehydrate_binop(op),
            right: id(*right),
        },
        WireExpr::FunctionCall {
            name,
            args_start,
            args_len,
        } => Expr::FunctionCall {
            name: Symbol(*name),
            args_start: *args_start,
            args_len: *args_len,
        },
        WireExpr::Let { name, value, body } => Expr::Let {
            name: Symbol(*name),
            value: id(*value),
            body: id(*body),
        },
        WireExpr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => Expr::IfElse {
            condition: id(*condition),
            then_expr: id(*then_expr),
            else_expr: id(*else_expr),
        },
        WireExpr::FieldAccess {
            base,
            field,
            field_hash,
        } => Expr::FieldAccess {
            base: id(*base),
            field: Symbol(*field),
            field_hash: *field_hash,
        },
    })
}

// ── Values ───────────────────────────────────────────────────────────────────

/// Depth-guarded [`WireValue`] → [`Value`].
///
/// The `From<WireValue> for Value` impl from Phase 1 recurses without a
/// bound, which is fine for values this process built but not for a
/// `List`-of-`List` chain an attacker nested a million deep. This version is
/// what the untrusted paths use.
pub fn rehydrate_value(v: &WireValue, depth: u32) -> Result<Value, MizuError> {
    const MAX_VALUE_DEPTH: u32 = 128;
    if depth > MAX_VALUE_DEPTH {
        return Err(err(format!(
            "value nested deeper than {MAX_VALUE_DEPTH} levels"
        )));
    }
    Ok(match v {
        WireValue::Null => Value::Null,
        WireValue::Bool(b) => Value::Bool(*b),
        WireValue::Int(n) => Value::Int(*n),
        WireValue::Decimal(n) => Value::Decimal(*n),
        WireValue::Str(s) => Value::String(Arc::from(s.as_str())),
        WireValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(rehydrate_value(item, depth + 1)?);
            }
            Value::List(Arc::new(out))
        }
        WireValue::Record(fields) => {
            // Rebuilt through `record_from_unsorted` rather than trusting the
            // wire order: `Value::Record`'s lexicographic ordering is a
            // load-bearing invariant (structural equality zips the slices
            // pairwise), and a worker that shuffled the fields would
            // otherwise produce a record that compares unequal to an
            // identical one built locally.
            let mut pairs = Vec::with_capacity(fields.len());
            for f in fields {
                pairs.push((f.key.clone(), rehydrate_value(&f.value, depth + 1)?));
            }
            Value::record_from_unsorted(pairs)
        }
        // A token is not a file handle. Resolving it requires the broker's
        // capability table, which this function has no access to by design —
        // callers that must honour tokens look them up themselves.
        WireValue::FileHandleToken { .. } => Value::Null,
    })
}

// ── Actions, functions, bindings ─────────────────────────────────────────────

/// Rebuilds one [`Action`], validating every expression tree it carries.
pub fn rehydrate_action(action: &WireAction) -> Result<Action, MizuError> {
    Ok(match action {
        WireAction::Eval(tree) => Action::Eval(rehydrate_expr_tree(tree)?),
        WireAction::Assign { target, expr } => Action::Assign {
            target: target.clone(),
            expr: rehydrate_expr_tree(expr)?,
        },
        WireAction::Navigate { url } => Action::Navigate {
            url: rehydrate_expr_tree(url)?,
        },
        WireAction::NetworkCall {
            method,
            alias_sym,
            payload,
            path_param,
            target_var,
            format,
            header_names,
            header_exprs,
        } => {
            if header_names.len() != header_exprs.len() {
                return Err(err(format!(
                    "network call has {} header names but {} header expressions",
                    header_names.len(),
                    header_exprs.len()
                )));
            }
            let mut headers = Vec::with_capacity(header_names.len());
            for (name, expr) in header_names.iter().zip(header_exprs.iter()) {
                headers.push((name.clone(), rehydrate_expr_tree(expr)?));
            }
            Action::NetworkCall {
                method: rehydrate_method(method),
                alias_sym: Symbol(*alias_sym),
                payload: payload.as_ref().map(rehydrate_expr_tree).transpose()?,
                path_param: path_param.as_ref().map(rehydrate_expr_tree).transpose()?,
                target_var: target_var.clone(),
                format: rehydrate_format(format),
                headers,
            }
        }
    })
}

/// Rebuilds one [`MizuFunction`], checking the parameter vectors agree.
pub fn rehydrate_mizu_function(f: &WireMizuFunction) -> Result<MizuFunction, MizuError> {
    if f.param_syms.len() != f.param_types.len() {
        return Err(err(format!(
            "function has {} parameter symbols but {} parameter types",
            f.param_syms.len(),
            f.param_types.len()
        )));
    }
    let mut params = Vec::with_capacity(f.param_syms.len());
    for (sym, ty) in f.param_syms.iter().zip(f.param_types.iter()) {
        params.push((Symbol(*sym), rehydrate_value_type(ty, 0)?));
    }
    Ok(MizuFunction {
        params,
        body: rehydrate_expr_tree(&f.body)?,
    })
}

/// Rebuilds one [`ComputedBinding`].
pub fn rehydrate_computed_binding(
    cb: &WireComputedBinding,
) -> Result<ComputedBinding, MizuError> {
    Ok(ComputedBinding {
        name: Symbol(cb.name_sym),
        expr: rehydrate_expr_tree(&cb.expr)?,
        depends_on: cb.depends_on.iter().map(|s| Symbol(*s)).collect(),
        tainted: cb.tainted,
    })
}

fn rehydrate_endpoint(ep: &WireUrlEndpoint) -> UrlEndpoint {
    UrlEndpoint {
        kind: match ep.kind {
            WireUrlEndpointKind::Api => EndpointKind::Api,
            WireUrlEndpointKind::Media => EndpointKind::Media,
        },
        raw_target: ep.raw_target.clone(),
    }
}

// ── Top-level payload ────────────────────────────────────────────────────────

/// Rebuilds a [`ReloadPayload`] from its wire form.
///
/// Every parallel-vector pair is length-checked before it is zipped: Phase 1
/// encodes each `HashMap` as two `Vec`s, and nothing in the wire format
/// forces them to stay the same length, so a truncated `values` vector would
/// otherwise silently drop entries rather than being rejected.
///
/// Symbols are bounds-checked against the interner: a `Symbol` is an index
/// into `interner_strings`, and one past the end would resolve to `None`
/// deep inside the evaluator, turning a forged archive into a confusing
/// runtime failure far from its cause.
pub fn rehydrate_reload_payload(
    p: &WireReloadPayload,
) -> Result<mizu_core::messages::ReloadPayload, MizuError> {
    let symbol_count = p.interner_strings.len() as u32;
    let check_sym = |s: u32, what: &str| -> Result<Symbol, MizuError> {
        if s >= symbol_count {
            return Err(err(format!(
                "{what} references symbol {s} but the interner holds only \
                 {symbol_count} strings"
            )));
        }
        Ok(Symbol(s))
    };

    let zip_len = |a: usize, b: usize, what: &str| -> Result<(), MizuError> {
        if a != b {
            return Err(err(format!(
                "{what}: key/value vectors disagree ({a} keys, {b} values)"
            )));
        }
        Ok(())
    };

    // Interner: `vec` is the authority, `map` is its inverse.
    let interner = FrozenInterner {
        map: p
            .interner_strings
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), Symbol(i as u32)))
            .collect(),
        vec: p.interner_strings.clone(),
    };

    zip_len(
        p.logic_fn_keys.len(),
        p.logic_fn_values.len(),
        "logic functions",
    )?;
    let mut logic_fns = rustc_hash::FxHashMap::default();
    for (k, v) in p.logic_fn_keys.iter().zip(p.logic_fn_values.iter()) {
        logic_fns.insert(check_sym(*k, "logic function")?, rehydrate_mizu_function(v)?);
    }

    zip_len(
        p.click_action_ids.len(),
        p.click_actions.len(),
        "click actions",
    )?;
    let mut click_actions = std::collections::HashMap::new();
    for (id, a) in p.click_action_ids.iter().zip(p.click_actions.iter()) {
        click_actions.insert(*id, rehydrate_action(a)?);
    }

    zip_len(
        p.submit_action_ids.len(),
        p.submit_actions.len(),
        "submit actions",
    )?;
    let mut submit_actions = std::collections::HashMap::new();
    for (id, a) in p.submit_action_ids.iter().zip(p.submit_actions.iter()) {
        submit_actions.insert(*id, rehydrate_action(a)?);
    }

    let root_timer_actions = p
        .root_timer_actions
        .iter()
        .map(rehydrate_action)
        .collect::<Result<Vec<_>, _>>()?;

    zip_len(
        p.init_var_keys.len(),
        p.init_var_values.len(),
        "initial variables",
    )?;
    let mut initial_variables = Vec::with_capacity(p.init_var_keys.len());
    for (k, v) in p.init_var_keys.iter().zip(p.init_var_values.iter()) {
        let sym = check_sym(*k, "initial variable")?;
        let name = interner
            .resolve(sym)
            .ok_or_else(|| err("initial variable symbol not resolvable"))?
            .to_string();
        initial_variables.push((name, rehydrate_value(v, 0)?));
    }

    zip_len(
        p.url_registry_keys.len(),
        p.url_registry_values.len(),
        "url registry",
    )?;
    let mut url_registry = UrlRegistry::default();
    for (k, v) in p.url_registry_keys.iter().zip(p.url_registry_values.iter()) {
        url_registry.insert(check_sym(*k, "url alias")?, rehydrate_endpoint(v));
    }

    let computed_bindings = p
        .computed_bindings
        .iter()
        .map(rehydrate_computed_binding)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(mizu_core::messages::ReloadPayload {
        logic_fns,
        click_actions,
        submit_actions,
        root_timer_actions,
        interner,
        initial_variables,
        url_registry,
        document_domain: p.document_domain.clone(),
        computed_bindings,
    })
}

#[cfg(test)]
mod tests;

// ── Events (broker → worker) ─────────────────────────────────────────────────

/// Rebuilds a [`mizu_core::messages::UiEvent`] from its wire form.
///
/// Runs in the worker. The broker is the worker's parent and is not the
/// threat model here — but the checks stay anyway: a length mismatch between
/// `field_keys` and `field_values` is a bug on someone's side, and
/// discovering it as a rejected frame beats silently dropping half a form.
pub fn rehydrate_ui_event(
    e: &crate::wire::events::WireUiEvent,
) -> Result<mizu_core::messages::UiEvent, MizuError> {
    use crate::wire::events::WireUiEvent;
    use mizu_core::messages::UiEvent;

    Ok(match e {
        WireUiEvent::Click { node_id } => UiEvent::Click { node_id: *node_id },
        WireUiEvent::RootTimer { index } => UiEvent::RootTimer { index: *index },
        WireUiEvent::CloseTab => UiEvent::CloseTab,
        WireUiEvent::UpdateVariable { name, value } => UiEvent::UpdateVariable {
            name: name.clone(),
            value: rehydrate_value(value, 0)?,
        },
        WireUiEvent::SubmitForm {
            submitter_node_id,
            field_keys,
            field_values,
        } => {
            if field_keys.len() != field_values.len() {
                return Err(err(format!(
                    "submit form has {} field names but {} values",
                    field_keys.len(),
                    field_values.len()
                )));
            }
            let mut fields = rustc_hash::FxHashMap::default();
            for (k, v) in field_keys.iter().zip(field_values.iter()) {
                fields.insert(k.clone(), rehydrate_value(v, 0)?);
            }
            UiEvent::SubmitForm {
                submitter_node_id: *submitter_node_id,
                fields,
            }
        }
        WireUiEvent::Reload(payload) => {
            UiEvent::Reload(Box::new(rehydrate_reload_payload(payload)?))
        }
    })
}

// ── Responses (worker → broker) ──────────────────────────────────────────────

/// Rebuilds a [`mizu_core::messages::WorkerResponse`] from its wire form.
///
/// This one runs in the **broker**, on bytes an untrusted worker produced,
/// so it is the hostile direction. Note what it deliberately does *not* do:
/// it does not sanity-check `gesture`, and it does not resolve any URL. Both
/// are the capability broker's job
/// (`render::security::broker::authorize_action`), which re-derives them
/// from state the worker cannot reach. A partial version of that check here
/// would create a second, weaker notion of the same rule.
pub fn rehydrate_worker_response(
    r: &crate::wire::response::WireWorkerResponse,
) -> Result<mizu_core::messages::WorkerResponse, MizuError> {
    use mizu_core::messages::{StateUpdate, WorkerResponse};

    if r.mutated_syms.len() != r.mutated_values.len() {
        return Err(err(format!(
            "worker response has {} mutated symbols but {} values",
            r.mutated_syms.len(),
            r.mutated_values.len()
        )));
    }
    let mut mutated_variables = Vec::with_capacity(r.mutated_syms.len());
    for (sym, val) in r.mutated_syms.iter().zip(r.mutated_values.iter()) {
        mutated_variables.push((Symbol(*sym), rehydrate_value(val, 0)?));
    }

    let runtime_actions = r
        .runtime_actions
        .iter()
        .map(rehydrate_runtime_action)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(WorkerResponse {
        state_update: StateUpdate { mutated_variables },
        runtime_actions,
        gesture: r.gesture,
    })
}

/// Rebuilds one [`mizu_core::messages::RuntimeAction`] from its wire form.
///
/// Structural only — every policy question about the result (is this alias
/// declared? was there a gesture? does this URL belong to this origin?) is
/// answered later by the capability broker.
pub fn rehydrate_runtime_action(
    a: &crate::wire::actions::WireRuntimeAction,
) -> Result<mizu_core::messages::RuntimeAction, MizuError> {
    use crate::wire::actions::{WireNetworkMethod, WirePayloadFormat, WireRuntimeAction};
    use mizu_core::messages::RuntimeAction;
    use mizu_core::parser::logic::{NetworkMethod, PayloadFormat};

    fn method(m: &WireNetworkMethod) -> NetworkMethod {
        match m {
            WireNetworkMethod::Get => NetworkMethod::Get,
            WireNetworkMethod::Post => NetworkMethod::Post,
            WireNetworkMethod::Put => NetworkMethod::Put,
            WireNetworkMethod::Delete => NetworkMethod::Delete,
            WireNetworkMethod::Query => NetworkMethod::Query,
        }
    }
    fn format(f: &WirePayloadFormat) -> PayloadFormat {
        match f {
            WirePayloadFormat::Json => PayloadFormat::Json,
            WirePayloadFormat::Form => PayloadFormat::Form,
            WirePayloadFormat::Text => PayloadFormat::Text,
            WirePayloadFormat::Yaml => PayloadFormat::Yaml,
            WirePayloadFormat::Multipart => PayloadFormat::Multipart,
        }
    }
    fn headers(
        names: &[String],
        vals: &[WireValue],
    ) -> Result<Vec<(String, Value)>, MizuError> {
        if names.len() != vals.len() {
            return Err(err(format!(
                "action has {} header names but {} header values",
                names.len(),
                vals.len()
            )));
        }
        let mut out = Vec::with_capacity(names.len());
        for (n, v) in names.iter().zip(vals.iter()) {
            out.push((n.clone(), rehydrate_value(v, 0)?));
        }
        Ok(out)
    }

    Ok(match a {
        WireRuntimeAction::None => RuntimeAction::None,
        WireRuntimeAction::Navigate { url } => RuntimeAction::Navigate { url: url.clone() },
        WireRuntimeAction::DownloadMedia { url } => {
            RuntimeAction::DownloadMedia { url: url.clone() }
        }
        WireRuntimeAction::DownloadAlias { endpoint_symbol } => RuntimeAction::DownloadAlias {
            endpoint_symbol: *endpoint_symbol,
        },
        WireRuntimeAction::CopyToClipboard { node_id } => RuntimeAction::CopyToClipboard {
            node_id: node_id.clone(),
        },
        WireRuntimeAction::GetSystemTime {
            target_variable_sym,
        } => RuntimeAction::GetSystemTime {
            target_variable: Symbol(*target_variable_sym),
        },
        WireRuntimeAction::StoreLocal { key, value } => RuntimeAction::StoreLocal {
            key: key.clone(),
            value: rehydrate_value(value, 0)?,
        },
        WireRuntimeAction::NetworkCall {
            method: m,
            endpoint_symbol,
            payload,
            path_param,
            target_variable_sym,
            format: f,
            header_keys,
            header_values,
        } => RuntimeAction::NetworkCall {
            method: method(m),
            endpoint_symbol: *endpoint_symbol,
            payload: payload.as_ref().map(|p| rehydrate_value(p, 0)).transpose()?,
            path_param: path_param.clone(),
            target_variable: Symbol(*target_variable_sym),
            format: format(f),
            headers: headers(header_keys, header_values)?,
        },
        WireRuntimeAction::ResolvedCall {
            method: m,
            url,
            payload,
            target_variable_sym,
            format: f,
            header_keys,
            header_values,
        } => RuntimeAction::ResolvedCall {
            method: m.clone(),
            url: url.clone(),
            payload: payload.as_ref().map(|p| rehydrate_value(p, 0)).transpose()?,
            target_variable: Symbol(*target_variable_sym),
            format: format(f),
            headers: headers(header_keys, header_values)?,
        },
    })
}
