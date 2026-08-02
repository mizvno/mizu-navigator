//! AST and function/action type definitions for the Mizu logic block.

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value};

/// The type annotation on a Mizu function parameter or binding.
///
/// Parameters without an annotation use `None` at the call site
/// and accept any runtime value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    /// Corresponds to the `num` keyword — maps to [`Value::Int`] or [`Value::Decimal`].
    Num,
    /// Corresponds to the `string` keyword — maps to [`Value::String`].
    Str,
    /// Corresponds to the `bool` keyword — maps to [`Value::Bool`].
    Bool,
    /// Corresponds to the `list` keyword — matches any [`Value::List`] of the inner type.
    List(Box<ValueType>),
    /// Corresponds to the `record` keyword. Fields are canonicalized to sorted-by-name order at construction time.
    Record(Vec<(std::sync::Arc<str>, ValueType)>),
    /// Corresponds to the `?` suffix — explicitly nullable.
    Nullable(Box<ValueType>),
}

impl std::fmt::Display for ValueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueType::Num => write!(f, "num"),
            ValueType::Str => write!(f, "string"),
            ValueType::Bool => write!(f, "bool"),
            ValueType::List(inner) => write!(f, "list<{}>", inner),
            ValueType::Record(fields) => {
                write!(f, "record{{")?;
                for (i, (name, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", name, ty)?;
                }
                write!(f, "}}")
            }
            ValueType::Nullable(inner) => write!(f, "{}?", inner),
        }
    }
}

/// HTTP method for a compile-time–validated network call.
///
/// Used by [`Action::NetworkCall`] — the Mizu source verbs `GET`, `POST`,
/// `PUT`, `DELETE`, and `QUERY` each map to one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMethod {
    /// `GET` — retrieve a resource.
    Get,
    /// `POST` — create or submit.
    Post,
    /// `PUT` — replace a resource.
    Put,
    /// `DELETE` — remove a resource.
    Delete,
    /// `QUERY` — server-side filter / search (non-standard extension).
    Query,
}

impl NetworkMethod {
    /// Returns the uppercase HTTP method string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkMethod::Get => "GET",
            NetworkMethod::Post => "POST",
            NetworkMethod::Put => "PUT",
            NetworkMethod::Delete => "DELETE",
            NetworkMethod::Query => "QUERY",
        }
    }
}

/// Request payload wire format for [`Action::NetworkCall`], selected by an
/// optional trailing `as <keyword>` clause.
///
/// This is always fixed at parse time, never computed from a runtime
/// expression — see `SECURITY-INVARIANTS.md` §6 (the `alias_sym` /
/// `path_param` non-sinks entry): a format that could depend on a tainted
/// runtime value would be a header-injection channel with no existing gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadFormat {
    /// `Content-Type: application/json` — the default when no `as` clause is
    /// present. Any [`Value`] shape is accepted.
    #[default]
    Json,
    /// `Content-Type: application/x-www-form-urlencoded` — the payload must
    /// be a flat [`Value::Record`] of scalar (`Bool`/`Int`/`String`/`Null`)
    /// fields.
    Form,
    /// `Content-Type: text/plain; charset=utf-8` — the payload must be
    /// exactly a [`Value::String`].
    Text,
    /// `Content-Type: application/yaml` — the payload may be any [`Value`]
    /// shape, serialised the same way JSON is.
    Yaml,
    /// `Content-Type: multipart/form-data; boundary=<random>` — the payload
    /// must be a [`Value::Record`]; each field becomes a text, JSON, or file
    /// part depending on its value's shape (see
    /// `network::worker::multipart`). The only format that can carry a
    /// [`Value::FileHandle`] onto the wire.
    Multipart,
}

impl PayloadFormat {
    /// Parses the keyword following `as` in a `NetworkCall`'s trailing
    /// clause. Returns `None` for any string that isn't one of the five
    /// recognised keywords — the caller turns that into a hard parse error.
    #[must_use]
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s {
            "json" => Some(PayloadFormat::Json),
            "form" => Some(PayloadFormat::Form),
            "text" => Some(PayloadFormat::Text),
            "yaml" => Some(PayloadFormat::Yaml),
            "multipart" => Some(PayloadFormat::Multipart),
            _ => None,
        }
    }

    /// Returns the lowercase source keyword (`json`, `form`, `text`, `yaml`, `multipart`).
    #[must_use]
    pub fn as_keyword(&self) -> &'static str {
        match self {
            PayloadFormat::Json => "json",
            PayloadFormat::Form => "form",
            PayloadFormat::Text => "text",
            PayloadFormat::Yaml => "yaml",
            PayloadFormat::Multipart => "multipart",
        }
    }
}

/// A recurring timer declared at the root of the `logic` block.
///
/// Syntax: `timer <interval> -> <action>`
///
/// Example: `timer 500ms -> count = count + 1`
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct RootTimer {
    /// How often the action fires.
    pub interval: TimerInterval,
    /// The action to execute on each tick.
    pub action: Action,
}

/// A timer interval, either a literal millisecond count or a variable name.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum TimerInterval {
    /// A constant interval in milliseconds (e.g. `500ms` → `500`).
    Millis(u64),
    /// A variable identifier whose runtime value specifies milliseconds.
    Variable(String),
}

/// A binary operator (arithmetic, comparison, or logical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinOp {
    /// Addition (`+`).
    Add,
    /// Subtraction (`-`).
    Sub,
    /// Multiplication (`*`).
    Mul,
    /// Division (`/`).
    Div,
    /// Equality (`==`).
    Eq,
    /// Inequality (`!=`).
    Ne,
    /// Less-than (`<`).
    Lt,
    /// Greater-than (`>`).
    Gt,
    /// Less-than-or-equal (`<=`).
    Le,
    /// Greater-than-or-equal (`>=`).
    Ge,
    /// Logical AND (`&&`).
    And,
    /// Logical OR (`||`).
    Or,
}

/// A 0-based index of an [`Expr`] node within an [`ExprArena`].
///
/// Never constructed directly outside this module — the only way to obtain
/// one is [`ExprArena::alloc`], so every `ExprId` that exists is guaranteed
/// resolvable in the arena that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExprId(u32);

/// Owns every descendant node of one self-contained expression tree (a
/// [`MizuFunction`] body, an [`Action`]'s expression, a [`ComputedBinding`]'s
/// expression, ...) in one contiguous `Vec`, instead of one heap allocation
/// per recursive `Box<Expr>` node.
///
/// Append-only: [`alloc`](Self::alloc) is the only way to add a node, and it
/// always returns a fresh, valid [`ExprId`]. Indexing with an `ExprId` that
/// did not come from *this* arena (e.g. one from a different function's
/// arena) is a logic error — see [`ExprTree`], which pairs a root `ExprId`
/// with the arena it belongs to so the two are never separated.
#[derive(Debug, Clone, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ExprArena {
    nodes: Vec<Expr>,
    /// Shared backing storage for every `FunctionCall`'s argument list in
    /// this arena. A `FunctionCall` node stores only a `(start, len)` pair
    /// into this pool (see [`Expr::FunctionCall`]) instead of owning its
    /// own `Vec`/`Box<[ExprId]>` — one small heap allocation per call with
    /// arguments would otherwise round-trip the global allocator on every
    /// parse, defeating the point of arena-allocating `Expr` nodes in the
    /// first place. Appending here grows this one buffer geometrically
    /// (the same amortized-O(1) cost `nodes` already pays), rather than
    /// allocating a new buffer per call.
    args_pool: Vec<ExprId>,
}

impl ExprArena {
    /// Creates a new, empty arena.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            args_pool: Vec::new(),
        }
    }

    /// Appends `expr` to the arena and returns its new [`ExprId`].
    pub fn alloc(&mut self, expr: Expr) -> ExprId {
        let id = ExprId(self.nodes.len() as u32);
        self.nodes.push(expr);
        id
    }

    /// Resolves `id` to its node.
    #[must_use]
    pub fn get(&self, id: ExprId) -> &Expr {
        &self.nodes[id.0 as usize]
    }

    /// Appends `args` to the shared argument pool and returns the
    /// `(start, len)` pair a [`Expr::FunctionCall`] node needs to recover
    /// its argument slice via [`Self::args`].
    ///
    /// # Errors
    ///
    /// Returns [`MizuError::ParseError`] if `args.len()` doesn't fit in a
    /// `u32` (or if the pool itself would grow past `u32::MAX` entries).
    /// This is generously far beyond any legitimate call's argument count,
    /// but a document (or a directly-constructed `Expr`) is untrusted
    /// input — rejecting cleanly here instead of truncating or panicking
    /// keeps this fail-secure the same way every other arena/parser limit
    /// in this crate is.
    pub fn push_args(&mut self, args: &[ExprId]) -> Result<(u32, u32), MizuError> {
        let start = u32::try_from(self.args_pool.len()).map_err(|_| {
            MizuError::ParseError(
                "expression arena argument pool exceeds u32::MAX entries".to_string(),
            )
        })?;
        let len = u32::try_from(args.len()).map_err(|_| {
            MizuError::ParseError(
                "function call has more arguments than can be represented".to_string(),
            )
        })?;
        self.args_pool.extend_from_slice(args);
        Ok((start, len))
    }

    /// Resolves a `FunctionCall`'s `(start, len)` pair back to its argument
    /// slice. `start`/`len` must have come from [`Self::push_args`] on
    /// *this* arena — see [`ExprTree`]'s note on why a root and its arena
    /// always travel together.
    #[must_use]
    pub fn args(&self, start: u32, len: u32) -> &[ExprId] {
        &self.args_pool[start as usize..(start as usize + len as usize)]
    }
}

impl std::ops::Index<ExprId> for ExprArena {
    type Output = Expr;
    fn index(&self, id: ExprId) -> &Expr {
        self.get(id)
    }
}

/// A complete, self-contained expression tree: a root node plus the arena
/// holding every node it (transitively) references.
///
/// This is what a `Box<Expr>`-based field held directly before the arena
/// migration — every former top-level `Expr` field (`MizuFunction::body`,
/// `Action::Eval`'s payload, `ComputedBinding::expr`, ...) now holds one of
/// these instead, so the root and its arena travel together and can never
/// be paired with the wrong arena.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ExprTree {
    /// Every node in this tree.
    pub arena: ExprArena,
    /// The tree's root node.
    pub root: ExprId,
}

impl ExprTree {
    /// Returns the root [`Expr`] node.
    #[must_use]
    pub fn root(&self) -> &Expr {
        self.arena.get(self.root)
    }
}

/// An expression node in the Mizu AST.
///
/// `Expr` is a read-only AST tree — there are no mutation nodes,
/// no assignment nodes, and no loop nodes.  Every evaluation is a
/// deterministic fold over this tree.
///
/// ## Arena-indexed, not `Box`-recursive
///
/// Recursive positions here are [`ExprId`] indices into an [`ExprArena`]
/// rather than `Box<Expr>`, keeping a whole tree in one contiguous
/// allocation instead of one heap allocation per node. See [`ExprTree`] for
/// how a root node and its arena travel together. `FunctionCall`'s argument
/// list is a `(start, len)` pair into the same arena's shared argument pool
/// ([`ExprArena::push_args`]/[`ExprArena::args`]) rather than an owned
/// `Vec`/`Box<[ExprId]>` — a per-call collection would otherwise hit the
/// global allocator once per call with arguments, on top of whatever the
/// arena itself already amortizes.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum Expr {
    /// A compile-time constant literal.
    Literal(Value),

    /// A variable reference resolved at evaluation time via [`VariableStore`].
    /// The identifier is pre-interned at parse time — no HashMap lookup at runtime.
    Variable(Symbol),

    /// A binary arithmetic operation.
    BinaryOp {
        /// Left-hand operand.
        left: ExprId,
        /// The operator.
        op: BinOp,
        /// Right-hand operand.
        right: ExprId,
    },

    /// A call to a named Mizu function.
    FunctionCall {
        /// The function name, pre-interned at parse time.
        name: Symbol,
        /// Start offset of this call's arguments in the owning
        /// [`ExprArena`]'s shared argument pool. Resolve via
        /// [`ExprArena::args`] — never meaningful without the arena this
        /// node came from.
        args_start: u32,
        /// Number of argument expressions, evaluated left-to-right.
        args_len: u32,
    },

    /// A local binding used in multi-line function bodies.
    ///
    /// `let name = value_expr in body_expr`
    ///
    /// This node is not written by users; the parser synthesises it from
    /// indented `name = expr` lines within a multi-line function body.
    Let {
        /// The bound name, pre-interned at parse time.
        name: Symbol,
        /// The expression whose result is bound to `name`.
        value: ExprId,
        /// The expression that may reference `name`.
        body: ExprId,
    },

    /// Logical NOT unary operator (`!expr`).
    Not(ExprId),

    /// A conditional expression — produced by both syntactic forms:
    ///
    /// * `if <cond> then <then> else <else>`
    /// * `<cond> ? <then> : <else>`
    ///
    /// Evaluation is **lazy**: only the selected branch is evaluated.
    /// The condition must evaluate to `bool`; a non-bool condition is a
    /// `TypeError`.
    IfElse {
        /// The boolean guard expression.
        condition: ExprId,
        /// Expression evaluated when condition is true.
        then_expr: ExprId,
        /// Expression evaluated when condition is false.
        else_expr: ExprId,
    },

    /// Field access on a [`Value::Record`]: `base.field`.
    ///
    /// `base` must evaluate to a `Record`; accessing a missing field or a
    /// non-record base is a runtime error.  Chains (`a.b.c`) are represented
    /// as left-nested nodes: `FieldAccess { base: FieldAccess { base: a, field: b }, field: c }`.
    FieldAccess {
        /// The base expression, which must evaluate to a `Record`.
        base: ExprId,
        /// The field name to look up in the record.
        field: Symbol,
        /// Precomputed FNV-1a hash of the field name to accelerate runtime lookups.
        field_hash: u32,
    },
}

/// An interactive action triggered by an event.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum Action {
    /// An expression evaluated for its effects (e.g., calling a procedure).
    Eval(ExprTree),
    /// An assignment that mutates a variable in the store.
    Assign {
        /// The target variable name.
        target: String,
        /// The expression to evaluate and assign.
        expr: ExprTree,
    },
    /// A declarative navigation request to completely replace the document.
    Navigate {
        /// The URI expression to navigate to.
        url: ExprTree,
    },
    /// A compile-time–validated HTTP call via a named URL alias.
    ///
    /// The alias is resolved at parse time against the [`UrlRegistry`]; a
    /// missing or wrong-kind alias is a hard compile error.
    NetworkCall {
        /// HTTP verb.
        method: NetworkMethod,
        /// The interned Symbol for the URL alias (e.g. `login` → `Symbol(N)`).
        alias_sym: Symbol,
        /// Optional JSON payload expression (used by POST, PUT, QUERY).
        payload: Option<ExprTree>,
        /// Optional path parameter expression (used by DELETE for `/item/{id}`).
        path_param: Option<ExprTree>,
        /// The variable name that receives the response.
        target_var: String,
        /// Request payload wire format, fixed at parse time by an optional
        /// trailing `as <keyword>` clause (defaults to `json`).
        format: PayloadFormat,
        /// Custom request headers: `(name, value_expr)` pairs from zero or
        /// more trailing `header "<name>" <expr>` clauses.
        ///
        /// The name is always a parse-time string literal (validated and
        /// denylist-checked at parse time — see `SECURITY-INVARIANTS.md`'s
        /// Non-sinks entry); the value is a runtime expression, evaluated and
        /// stringified at request time.
        headers: Vec<(String, ExprTree)>,
    },
}

/// A compiled Mizu function definition.
///
/// After passing the DAG validation step, instances of this struct can be
/// used freely by [`evaluate`] without risk of infinite recursion.
#[derive(Debug, Clone)]
pub struct MizuFunction {
    /// Ordered list of `(parameter_symbol, type_annotation)` pairs.
    /// The symbol is pre-interned — no string allocation at call time.
    pub params: Vec<(Symbol, ValueType)>,
    /// The function body expression (may be a chain of [`Expr::Let`] nodes
    /// for multi-line functions, with the return value at the innermost body).
    pub body: ExprTree,
}

/// A computed (derived) variable that auto-recomputes when dependencies change.
///
/// Syntax: `comp name = expr`
///
/// The `depends_on` list is derived statically by walking the right-hand-side
/// AST with [`collect_vars`].  Bindings are stored in topological order
/// (dependencies before dependents) after [`parse_computed`] validates the
/// absence of cycles.
#[derive(Debug, Clone)]
pub struct ComputedBinding {
    /// Interned symbol for the variable name.
    pub name: Symbol,
    /// The expression that defines this variable's value.
    pub expr: ExprTree,
    /// Symbols of all variables this binding may read: those referenced
    /// directly by `expr` plus — when parsed via
    /// [`parse_computed_with_functions`] — the globals read transitively inside
    /// any called logic function.  May include other comp vars.
    pub depends_on: Vec<Symbol>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_args_then_args_round_trips_exactly() {
        let mut arena = ExprArena::new();
        let a = arena.alloc(Expr::Literal(Value::Decimal(1)));
        let b = arena.alloc(Expr::Literal(Value::Decimal(2)));
        let c = arena.alloc(Expr::Literal(Value::Decimal(3)));
        let (start, len) = arena.push_args(&[a, b, c]).unwrap();
        assert_eq!(arena.args(start, len), &[a, b, c]);
    }

    #[test]
    fn push_args_empty_slice_round_trips_to_empty() {
        let mut arena = ExprArena::new();
        let (start, len) = arena.push_args(&[]).unwrap();
        assert_eq!(arena.args(start, len), &[] as &[ExprId]);
    }

    #[test]
    fn multiple_push_args_calls_do_not_overlap() {
        // Two FunctionCall nodes' argument ranges, pushed in sequence, must
        // each resolve back to exactly their own arguments -- proving the
        // shared pool doesn't let one call's args bleed into another's.
        let mut arena = ExprArena::new();
        let x = arena.alloc(Expr::Literal(Value::Decimal(10)));
        let y = arena.alloc(Expr::Literal(Value::Decimal(20)));
        let (start1, len1) = arena.push_args(&[x]).unwrap();
        let (start2, len2) = arena.push_args(&[y, x]).unwrap();
        assert_eq!(arena.args(start1, len1), &[x]);
        assert_eq!(arena.args(start2, len2), &[y, x]);
    }

    #[test]
    fn function_call_args_resolve_through_shared_pool() {
        // End-to-end: a FunctionCall node stores (args_start, args_len),
        // not its own collection -- resolving it through the arena must
        // recover the exact argument ExprIds that were pushed.
        let mut arena = ExprArena::new();
        let arg0 = arena.alloc(Expr::Literal(Value::Decimal(42)));
        let (args_start, args_len) = arena.push_args(&[arg0]).unwrap();
        let call = Expr::FunctionCall {
            name: Symbol(0),
            args_start,
            args_len,
        };
        let Expr::FunctionCall {
            args_start,
            args_len,
            ..
        } = call
        else {
            unreachable!()
        };
        assert_eq!(arena.args(args_start, args_len), &[arg0]);
    }
}
