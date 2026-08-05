//! # `wire::reload` — `WireReloadPayload` and supporting AST wire types
//!
//! `WireReloadPayload` is the rkyv-archivable mirror of
//! [`mizu_core::messages::ReloadPayload`].  It is placed in an anonymous
//! shared memory region (see [`crate::shm`]) and read by the worker
//! zero-copy via `rkyv::access`.
//!
//! ## HashMap → parallel Vec encoding
//!
//! rkyv cannot archive a `HashMap` in a zero-copy fashion.  Every map in
//! `ReloadPayload` is encoded as two parallel `Vec`s of equal length:
//!
//! | Original | Wire encoding |
//! |---|---|
//! | `FxHashMap<Symbol, MizuFunction>` | `logic_fn_keys: Vec<u32>` + `logic_fn_values: Vec<WireMizuFunction>` |
//! | `HashMap<u32, Action>` (clicks) | `click_action_ids: Vec<u32>` + `click_actions: Vec<WireAction>` |
//! | `HashMap<u32, Action>` (submits) | `submit_action_ids: Vec<u32>` + `submit_actions: Vec<WireAction>` |
//! | `FxHashMap<Symbol, UrlEndpoint>` | `url_registry_keys: Vec<u32>` + `url_registry_values: Vec<WireUrlEndpoint>` |
//! | `Vec<(String, Value)>` (initial vars) | `init_var_keys: Vec<u32>` + `init_var_values: Vec<WireValue>` |
//!
//! The worker reconstructs each map from its parallel vecs in O(n) after
//! validating the archive, paying the reconstruction cost once per document
//! load — not per event.
//!
//! ## AST serialization strategy
//!
//! The Mizu AST (`Expr`, `MizuFunction`, `Action`, `ComputedBinding`) is
//! arena-based: all nodes live in contiguous `Vec`s inside an `ExprArena`.
//! `WireMizuFunction` and `WireAction` capture the full arena + root index,
//! serialized as flat `Vec<WireExpr>` + `u32` root.  `WireExpr` variants
//! mirror `Expr` exactly, replacing `ExprId` with a raw `u32` index (which
//! is already the underlying representation).

#![forbid(unsafe_code)]

use rkyv::{Archive, Deserialize, Serialize};

use crate::wire::value::WireValue;

// ── Primitive AST wire types ─────────────────────────────────────────────────

/// Wire mirror of [`mizu_core::parser::logic::BinOp`].
#[derive(Debug, Clone, Copy, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireBinOp {
    Add, Sub, Mul, Div,
    Eq, Ne, Lt, Gt, Le, Ge,
    And, Or,
}

/// Wire mirror of [`mizu_core::parser::logic::ValueType`].
///
/// `ValueType::Record` carries `Vec<(Arc<str>, ValueType)>` which is not
/// directly archivable.  Encoded as two parallel vecs.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
#[rkyv(serialize_bounds(
    __S: rkyv::ser::Writer + rkyv::ser::Allocator,
    __S::Error: rkyv::rancor::Source,
))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(
    __C: rkyv::validation::ArchiveContext,
    __C::Error: rkyv::rancor::Source,
)))]
pub enum WireValueType {
    Num,
    Str,
    Bool,
    List(#[rkyv(omit_bounds)] Box<WireValueType>),
    /// Field names and types encoded as parallel vecs (sorted by name,
    /// matching the canonicalization in `ValueType::Record`).
    Record {
        field_names: Vec<String>,
        #[rkyv(omit_bounds)]
        field_types: Vec<WireValueType>,
    },
    Nullable(#[rkyv(omit_bounds)] Box<WireValueType>),
}

/// Wire mirror of an `Expr` node.  `ExprId` → raw `u32` index.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireExpr {
    Literal(WireValue),
    Variable(u32),
    BinaryOp { left: u32, op: WireBinOp, right: u32 },
    FunctionCall { name: u32, args_start: u32, args_len: u32 },
    Let { name: u32, value: u32, body: u32 },
    Not(u32),
    IfElse { condition: u32, then_expr: u32, else_expr: u32 },
    FieldAccess { base: u32, field: u32, field_hash: u32 },
}

/// Wire mirror of an `ExprArena` (nodes + argument pool) plus a root index.
///
/// Serialised alongside the root to keep the two inseparable, matching the
/// `ExprTree` invariant on the core side.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireExprTree {
    /// All `Expr` nodes in the arena, in allocation order.
    pub nodes: Vec<WireExpr>,
    /// Shared argument pool for `FunctionCall` nodes.
    pub args_pool: Vec<u32>,
    /// Index of the root node inside `nodes`.
    pub root: u32,
}

/// Wire mirror of [`mizu_core::parser::logic::NetworkMethod`].
#[derive(Debug, Clone, Copy, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireNetworkMethodReload {
    Get, Post, Put, Delete, Query,
}

/// Wire mirror of [`mizu_core::parser::logic::PayloadFormat`].
#[derive(Debug, Clone, Copy, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WirePayloadFormatReload {
    Json, Form, Text, Yaml, Multipart,
}

/// Wire mirror of [`mizu_core::parser::Action`].
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireAction {
    Eval(WireExprTree),
    Assign {
        target: String,
        expr: WireExprTree,
    },
    Navigate {
        url: WireExprTree,
    },
    NetworkCall {
        method: WireNetworkMethodReload,
        /// Raw u32 of the URL alias Symbol.
        alias_sym: u32,
        payload: Option<WireExprTree>,
        path_param: Option<WireExprTree>,
        target_var: String,
        format: WirePayloadFormatReload,
        /// Header names (parse-time literals).
        header_names: Vec<String>,
        /// Header value expressions, parallel to `header_names`.
        header_exprs: Vec<WireExprTree>,
    },
}

/// Wire mirror of [`mizu_core::parser::MizuFunction`].
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireMizuFunction {
    /// Parameter symbols (raw u32) and their type annotations, parallel vecs.
    pub param_syms: Vec<u32>,
    pub param_types: Vec<WireValueType>,
    /// Function body.
    pub body: WireExprTree,
}

/// Wire mirror of [`mizu_core::parser::logic::ComputedBinding`].
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireComputedBinding {
    /// Interned symbol raw u32 for the binding's name.
    pub name_sym: u32,
    /// The RHS expression.
    pub expr: WireExprTree,
    /// Symbols (raw u32s) of all dependencies.
    pub depends_on: Vec<u32>,
    /// Whether this binding may derive from tainted (attacker-controlled) data.
    pub tainted: bool,
}

/// Wire mirror of [`mizu_core::parser::EndpointKind`].
#[derive(Debug, Clone, Copy, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireUrlEndpointKind {
    Api,
    Media,
}

/// Wire mirror of [`mizu_core::parser::UrlEndpoint`].
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireUrlEndpoint {
    pub kind: WireUrlEndpointKind,
    pub raw_target: String,
}

// ── Top-level payload ────────────────────────────────────────────────────────

/// Wire-format mirror of [`mizu_core::messages::ReloadPayload`].
///
/// Placed in an anonymous shared memory region; the worker accesses it
/// zero-copy via `rkyv::access::<ArchivedWireReloadPayload, _>(mmap_bytes)`.
///
/// All `HashMap<K, V>` fields are split into parallel `keys: Vec<K>` and
/// `values: Vec<V>` of equal length.  The worker reconstructs each map in
/// O(n) after archive validation.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireReloadPayload {
    // ── Logic functions ──────────────────────────────────────────────────────
    /// Symbol raw u32s for each logic function, parallel to `logic_fn_values`.
    pub logic_fn_keys: Vec<u32>,
    /// Compiled function bodies, parallel to `logic_fn_keys`.
    pub logic_fn_values: Vec<WireMizuFunction>,

    // ── Click actions ────────────────────────────────────────────────────────
    /// Node IDs of nodes with `click -> …` actions.
    pub click_action_ids: Vec<u32>,
    /// Click actions, parallel to `click_action_ids`.
    pub click_actions: Vec<WireAction>,

    // ── Submit actions ───────────────────────────────────────────────────────
    /// Node IDs of nodes with `submit -> …` actions.
    pub submit_action_ids: Vec<u32>,
    /// Submit actions, parallel to `submit_action_ids`.
    pub submit_actions: Vec<WireAction>,

    // ── Root timer actions ───────────────────────────────────────────────────
    /// Timer actions in declaration order.
    pub root_timer_actions: Vec<WireAction>,

    // ── Frozen string interner ───────────────────────────────────────────────
    /// `interner_strings[i]` is the string for `Symbol(i)`.
    /// The worker reconstructs the HashMap from this vec in O(n).
    pub interner_strings: Vec<String>,

    // ── Initial variable bindings ────────────────────────────────────────────
    /// Symbol raw u32s for non-null initial variables.
    pub init_var_keys: Vec<u32>,
    /// Initial values, parallel to `init_var_keys`.
    pub init_var_values: Vec<WireValue>,

    // ── URL registry ─────────────────────────────────────────────────────────
    /// Symbol raw u32s for URL alias names.
    pub url_registry_keys: Vec<u32>,
    /// URL endpoint data, parallel to `url_registry_keys`.
    pub url_registry_values: Vec<WireUrlEndpoint>,

    // ── Document metadata ────────────────────────────────────────────────────
    /// ASCII domain of the current document (e.g. `"example.com"`).
    pub document_domain: String,

    // ── Computed (derived) bindings ──────────────────────────────────────────
    /// In topological order: dependencies before dependents.
    pub computed_bindings: Vec<WireComputedBinding>,
}

// ── Conversions from core AST types ─────────────────────────────────────────

impl From<&mizu_core::parser::logic::BinOp> for WireBinOp {
    fn from(b: &mizu_core::parser::logic::BinOp) -> Self {
        use mizu_core::parser::logic::BinOp;
        match b {
            BinOp::Add => WireBinOp::Add,
            BinOp::Sub => WireBinOp::Sub,
            BinOp::Mul => WireBinOp::Mul,
            BinOp::Div => WireBinOp::Div,
            BinOp::Eq  => WireBinOp::Eq,
            BinOp::Ne  => WireBinOp::Ne,
            BinOp::Lt  => WireBinOp::Lt,
            BinOp::Gt  => WireBinOp::Gt,
            BinOp::Le  => WireBinOp::Le,
            BinOp::Ge  => WireBinOp::Ge,
            BinOp::And => WireBinOp::And,
            BinOp::Or  => WireBinOp::Or,
        }
    }
}

impl From<&mizu_core::parser::logic::ValueType> for WireValueType {
    fn from(vt: &mizu_core::parser::logic::ValueType) -> Self {
        use mizu_core::parser::logic::ValueType;
        match vt {
            ValueType::Num      => WireValueType::Num,
            ValueType::Str      => WireValueType::Str,
            ValueType::Bool     => WireValueType::Bool,
            ValueType::List(inner) => {
                WireValueType::List(Box::new(WireValueType::from(inner.as_ref())))
            }
            ValueType::Record(fields) => {
                let field_names = fields.iter().map(|(n, _)| n.to_string()).collect();
                let field_types = fields.iter().map(|(_, t)| WireValueType::from(t)).collect();
                WireValueType::Record { field_names, field_types }
            }
            ValueType::Nullable(inner) => {
                WireValueType::Nullable(Box::new(WireValueType::from(inner.as_ref())))
            }
        }
    }
}

impl From<&mizu_core::parser::logic::NetworkMethod> for WireNetworkMethodReload {
    fn from(m: &mizu_core::parser::logic::NetworkMethod) -> Self {
        use mizu_core::parser::logic::NetworkMethod;
        match m {
            NetworkMethod::Get    => WireNetworkMethodReload::Get,
            NetworkMethod::Post   => WireNetworkMethodReload::Post,
            NetworkMethod::Put    => WireNetworkMethodReload::Put,
            NetworkMethod::Delete => WireNetworkMethodReload::Delete,
            NetworkMethod::Query  => WireNetworkMethodReload::Query,
        }
    }
}

impl From<&mizu_core::parser::logic::PayloadFormat> for WirePayloadFormatReload {
    fn from(f: &mizu_core::parser::logic::PayloadFormat) -> Self {
        use mizu_core::parser::logic::PayloadFormat;
        match f {
            PayloadFormat::Json      => WirePayloadFormatReload::Json,
            PayloadFormat::Form      => WirePayloadFormatReload::Form,
            PayloadFormat::Text      => WirePayloadFormatReload::Text,
            PayloadFormat::Yaml      => WirePayloadFormatReload::Yaml,
            PayloadFormat::Multipart => WirePayloadFormatReload::Multipart,
        }
    }
}

/// Convert a core `ExprArena` + root `ExprId` into a `WireExprTree`.
pub fn wire_expr_tree(tree: &mizu_core::parser::logic::ExprTree) -> WireExprTree {
    // Walk every node in the arena by raw index and convert.
    let arena = &tree.arena;
    let n = arena.node_count();
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n as u32 {
        nodes.push(wire_expr(arena.get_by_index(i), arena));
    }
    // Serialize the shared argument pool as a flat Vec<u32> of raw indices.
    let args_pool: Vec<u32> = (0..arena.args_pool_len())
        .map(|i| arena.args_pool_index(i))
        .collect();
    WireExprTree {
        nodes,
        args_pool,
        root: tree.root.index(),
    }
}

fn wire_expr(
    expr: &mizu_core::parser::logic::Expr,
    _arena: &mizu_core::parser::logic::ExprArena,
) -> WireExpr {
    use mizu_core::parser::logic::Expr;
    match expr {
        Expr::Literal(v) => WireExpr::Literal(WireValue::from(v)),
        Expr::Variable(sym) => WireExpr::Variable(sym.0),
        Expr::BinaryOp { left, op, right } => WireExpr::BinaryOp {
            left:  left.index(),
            op:    WireBinOp::from(op),
            right: right.index(),
        },
        Expr::FunctionCall { name, args_start, args_len } => WireExpr::FunctionCall {
            name:       name.0,
            args_start: *args_start,
            args_len:   *args_len,
        },
        Expr::Let { name, value, body } => WireExpr::Let {
            name:  name.0,
            value: value.index(),
            body:  body.index(),
        },
        Expr::Not(inner) => WireExpr::Not(inner.index()),
        Expr::IfElse { condition, then_expr, else_expr } => WireExpr::IfElse {
            condition: condition.index(),
            then_expr: then_expr.index(),
            else_expr: else_expr.index(),
        },
        Expr::FieldAccess { base, field, field_hash } => WireExpr::FieldAccess {
            base:       base.index(),
            field:      field.0,
            field_hash: *field_hash,
        },
    }
}

/// Convert a core `Action` into a `WireAction`.
pub fn wire_action(action: &mizu_core::parser::Action) -> WireAction {
    use mizu_core::parser::Action;
    match action {
        Action::Eval(tree) => WireAction::Eval(wire_expr_tree(tree)),
        Action::Assign { target, expr } => WireAction::Assign {
            target: target.clone(),
            expr:   wire_expr_tree(expr),
        },
        Action::Navigate { url } => WireAction::Navigate {
            url: wire_expr_tree(url),
        },
        Action::NetworkCall {
            method, alias_sym, payload, path_param, target_var, format, headers,
        } => WireAction::NetworkCall {
            method:      WireNetworkMethodReload::from(method),
            alias_sym:   alias_sym.0,
            payload:     payload.as_ref().map(wire_expr_tree),
            path_param:  path_param.as_ref().map(wire_expr_tree),
            target_var:  target_var.clone(),
            format:      WirePayloadFormatReload::from(format),
            header_names: headers.iter().map(|(n, _)| n.clone()).collect(),
            header_exprs: headers.iter().map(|(_, e)| wire_expr_tree(e)).collect(),
        },
    }
}

/// Convert a core `MizuFunction` into a `WireMizuFunction`.
pub fn wire_mizu_function(f: &mizu_core::parser::MizuFunction) -> WireMizuFunction {
    WireMizuFunction {
        param_syms:  f.params.iter().map(|(s, _)| s.0).collect(),
        param_types: f.params.iter().map(|(_, t)| WireValueType::from(t)).collect(),
        body:        wire_expr_tree(&f.body),
    }
}

/// Convert a core `ComputedBinding` into a `WireComputedBinding`.
pub fn wire_computed_binding(cb: &mizu_core::parser::logic::ComputedBinding) -> WireComputedBinding {
    WireComputedBinding {
        name_sym:   cb.name.0,
        expr:       wire_expr_tree(&cb.expr),
        depends_on: cb.depends_on.iter().map(|s| s.0).collect(),
        tainted:    cb.tainted,
    }
}

impl From<&mizu_core::messages::ReloadPayload> for WireReloadPayload {
    fn from(p: &mizu_core::messages::ReloadPayload) -> Self {
        use crate::wire::value::WireValue;

        // Logic functions
        let (logic_fn_keys, logic_fn_values) = p
            .logic_fns
            .iter()
            .map(|(sym, f)| (sym.0, wire_mizu_function(f)))
            .unzip();

        // Click actions
        let (click_action_ids, click_actions) = p
            .click_actions
            .iter()
            .map(|(id, a)| (*id, wire_action(a)))
            .unzip();

        // Submit actions
        let (submit_action_ids, submit_actions) = p
            .submit_actions
            .iter()
            .map(|(id, a)| (*id, wire_action(a)))
            .unzip();

        // Root timer actions
        let root_timer_actions = p.root_timer_actions.iter().map(wire_action).collect();

        // Interner — just the vec (index → string)
        let interner_strings = p.interner.vec.clone();

        // Initial variables: (String name → Symbol first, then raw u32)
        let (init_var_keys, init_var_values) = p
            .initial_variables
            .iter()
            .map(|(name, val)| {
                let sym = p.interner.get(name).map(|s| s.0).unwrap_or(u32::MAX);
                (sym, WireValue::from(val))
            })
            .unzip();

        // URL registry
        let (url_registry_keys, url_registry_values) = p
            .url_registry
            .iter()
            .map(|(sym, ep)| {
                let kind = match ep.kind {
                    mizu_core::parser::EndpointKind::Api   => WireUrlEndpointKind::Api,
                    mizu_core::parser::EndpointKind::Media => WireUrlEndpointKind::Media,
                };
                (sym.0, WireUrlEndpoint { kind, raw_target: ep.raw_target.clone() })
            })
            .unzip();

        // Computed bindings
        let computed_bindings = p.computed_bindings.iter().map(wire_computed_binding).collect();

        WireReloadPayload {
            logic_fn_keys,
            logic_fn_values,
            click_action_ids,
            click_actions,
            submit_action_ids,
            submit_actions,
            root_timer_actions,
            interner_strings,
            init_var_keys,
            init_var_values,
            url_registry_keys,
            url_registry_values,
            document_domain: p.document_domain.clone(),
            computed_bindings,
        }
    }
}
