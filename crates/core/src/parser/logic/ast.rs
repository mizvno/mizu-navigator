//! AST and function/action type definitions for the Mizu logic block.

use std::sync::Arc;

use crate::core::types::{Symbol, Value};

/// The type annotation on a Mizu function parameter or binding.
///
/// Parameters without an annotation use `None` at the call site
/// and accept any runtime value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    /// Corresponds to the `num` keyword — maps to [`Value::Int`] or [`Value::Float`].
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


/// A recurring timer declared at the root of the `logic` block.
///
/// Syntax: `timer <interval> -> <action>`
///
/// Example: `timer 500ms -> count = count + 1`
#[derive(Debug, Clone, PartialEq)]
pub struct RootTimer {
    /// How often the action fires.
    pub interval: TimerInterval,
    /// The action to execute on each tick.
    pub action: Action,
}

/// A timer interval, either a literal millisecond count or a variable name.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExprArena {
    nodes: Vec<Expr>,
}

impl ExprArena {
    /// Creates a new, empty arena.
    #[must_use]
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
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
#[derive(Debug, Clone, PartialEq)]
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
/// how a root node and its arena travel together. `FunctionCall::args` is a
/// `Vec<ExprId>` for the same reason — each argument expression's own
/// descendants live in the *caller's* arena, not a separate allocation per
/// argument.
#[derive(Debug, Clone, PartialEq)]
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
        /// Positional argument expressions, evaluated left-to-right.
        args: Vec<ExprId>,
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
        field: Arc<str>,
    },
}

/// An interactive action triggered by an event.
#[derive(Debug, Clone, PartialEq)]
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
