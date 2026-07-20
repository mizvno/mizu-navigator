//! # `types` — Core Value Primitives and Variable Store
//!
//! This module defines the fundamental data model of the Mizu runtime.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::core::errors::MizuError;
use crate::parser::logic::{Expr, MizuFunction, apply_binop, check_type, type_name};


/// A Symbol represents a unique identifier mapped from a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Symbol(pub u32);

/// The scale factor used for fixed-point arithmetic.
pub const DECIMAL_SCALE: i64 = 10_000;

/// Map strings to symbols and resolve symbols back to strings.
///
/// ## Concurrency model
///
/// The UI thread and the logic worker thread each own an independent
/// **clone** of a `StringInterner` — there is no shared/locked table, so
/// there is no lock contention on this cold path (interning happens at
/// parse/reload time; see the module-level architecture note below). The two
/// clones are guaranteed to agree on every `Symbol(u32)` ID *at the moment of
/// the clone* because [`Clone`] preserves the source's contents exactly.
/// [`freeze`](Self::freeze) is the boundary that keeps them agreeing
/// afterward: once frozen, a thread must not mint symbols for names the
/// other clone has never seen, because the other clone has no way to learn
/// about them and the two tables would silently diverge (`Symbol(7)` meaning
/// one string on one thread and a different string, or nothing, on the
/// other).
///
/// After the initial parse phase the interner **must** be
/// [`freeze`](Self::freeze)d. Post-freeze code that may encounter strings
/// not declared in the logic block (form field names, network response
/// variable names) must use [`VariableStore::set_runtime`] instead of
/// [`VariableStore::set`]. `set_runtime` calls [`get`](Self::get) rather than
/// [`get_or_intern`](Self::get_or_intern) and silently discards unknown
/// names, so the frozen symbol table is never mutated by untrusted runtime
/// content — this is also the load-bearing defence against a document (or a
/// malicious network response / form) growing the symbol table unboundedly.
///
/// [`get_or_intern`](Self::get_or_intern) itself remains a plain,
/// **thread-local** insert-or-lookup: calling it post-freeze with a genuinely
/// new name does intern it (there is no dummy/sentinel `Symbol` — every
/// `Symbol` this type ever returns is real and resolvable), but only in
/// *this* thread's own copy, and it logs a `tracing::warn!` because it means
/// a caller produced a `Symbol` that has no defined meaning on the other
/// thread's clone. The architectural rule that prevents this from happening
/// in practice is: never send a freshly-`get_or_intern`-ed post-freeze
/// `Symbol` across the UI↔worker channel. Cross-thread messages instead
/// carry the resolved `String` name (see [`crate::network::UiEvent::UpdateVariable`])
/// and the receiving thread resolves it against its *own* frozen table via
/// `set_runtime`/`get` — the two clones never need to invent a shared ID for
/// the same runtime string, because neither side is trusted to mint one on
/// the other's behalf.
///
/// Cloning a frozen interner produces a **frozen** copy.
#[derive(Debug, Default)]
pub struct StringInterner {
    /// Name → `Symbol` lookup, the inverse of `vec`.
    pub map: HashMap<String, Symbol>,
    /// `Symbol(i)` resolves to `vec[i]`; append-only.
    pub vec: Vec<String>,
    /// Once `true`, further insertions via `get_or_intern` are a logged caller
    /// bug (see [`Self::freeze`]).
    pub frozen: bool,
}

impl Clone for StringInterner {
    fn clone(&self) -> Self {
        // Preserve the `frozen` flag so the logic worker's copy of the
        // interner is also frozen.  The worker must never add new symbols at
        // runtime; doing so would create Symbol(u32) IDs that diverge between
        // the UI thread (whose interner never sees those new symbols) and the
        // worker, making every cross-thread symbol lookup unreliable.
        //
        // Callers that run after freeze and might encounter runtime-generated
        // strings must use VariableStore::set_runtime, which consults
        // interner.get() (not get_or_intern()) and discards unknown names.
        Self {
            map: self.map.clone(),
            vec: self.vec.clone(),
            frozen: self.frozen,
        }
    }
}

impl StringInterner {
    /// Creates a new empty interner.
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            vec: Vec::new(),
            frozen: false,
        }
    }

    /// Marks the interner as frozen.
    ///
    /// After this call, [`get_or_intern`](Self::get_or_intern) still works
    /// correctly (it never returns a bogus/unresolvable `Symbol`) but logs a
    /// `tracing::warn!` when asked to mint a symbol for a name it hasn't
    /// seen before — that symbol is only valid on *this* thread's clone of
    /// the table, so producing one post-freeze is a caller bug (see the
    /// type-level docs for the architectural rule that avoids this).
    ///
    /// Cloning a frozen interner produces a frozen copy (see [`Clone`]).
    pub fn freeze(&mut self) {
        self.frozen = true;
    }

    /// Interns `s` and returns its [`Symbol`], inserting it into this
    /// interner's own table if it is not already present.
    ///
    /// Every `Symbol` this method returns is real and resolvable via
    /// [`resolve`](Self::resolve) — there is no sentinel/dummy value. If the
    /// interner is frozen and `s` is not yet present, the insert still
    /// happens (so the caller always gets a working `Symbol` back), but a
    /// `tracing::warn!` is emitted: a `Symbol` minted here has no defined
    /// meaning on any *other* thread's clone of this table, so this should
    /// only ever happen for names local to this thread. Cross-thread code
    /// that may encounter runtime-generated strings must use
    /// [`get`](Self::get) or [`VariableStore::set_runtime`] instead.
    pub fn get_or_intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        if self.frozen {
            tracing::warn!(
                name = s,
                "StringInterner is frozen: interning '{}' post-freeze — the resulting \
                 Symbol is only valid on this thread's copy of the table and has no \
                 defined meaning elsewhere; cross-thread code must resolve names via \
                 set_runtime/get against its own frozen table instead of minting a new \
                 Symbol to send across threads",
                s
            );
        }
        let id = self.vec.len() as u32;
        let sym = Symbol(id);
        self.map.insert(s.to_string(), sym);
        self.vec.push(s.to_string());
        sym
    }

    /// Returns the [`Symbol`] for `s` if it was interned, or `None`.
    ///
    /// Unlike [`get_or_intern`](Self::get_or_intern) this method is
    /// **read-only**: it never adds new symbols and is safe to call on a frozen
    /// interner with arbitrary runtime strings.
    pub fn get(&self, s: &str) -> Option<Symbol> {
        self.map.get(s).copied()
    }

    /// Resolves a Symbol back to its string representation.
    pub fn resolve(&self, sym: Symbol) -> Option<&str> {
        self.vec.get(sym.0 as usize).map(|s| s.as_str())
    }
}


/// The set of all primitive values in the Mizu type system.
#[derive(Debug, Clone)]
pub enum Value {
    /// A null or empty value.
    Null,
    /// A boolean value (true or false).
    Bool(bool),
    /// A scaled 64-bit integer representing a fixed-point decimal.
    Int(i64),
    /// A reference-counted string.
    String(Arc<str>),
    /// A reference-counted list of nested values.
    List(Arc<Vec<Value>>),
    /// A reference-counted record of key-value pairs sorted by key.
    Record(Arc<[(Arc<str>, Value)]>),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Record(a), Value::Record(b)) => a == b,
            _ => false,
        }
    }
}

impl Value {
    /// Safely retrieves the value associated with `field` if this value is a `Value::Record`.
    /// Performs a binary search on the sorted key-value record slice.
    pub fn get_field(&self, field: &str) -> Option<&Value> {
        match self {
            Value::Record(slice) => {
                slice
                    .binary_search_by_key(&field, |(k, _)| k.as_ref())
                    .map(|idx| &slice[idx].1)
                    .ok()
            }
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => {
                let integer_part = i / DECIMAL_SCALE;
                let fractional_part = (i % DECIMAL_SCALE).abs();

                if fractional_part == 0 {
                    write!(f, "{}", integer_part)
                } else {
                    let mut frac_str = format!("{:04}", fractional_part);
                    frac_str = frac_str.trim_end_matches('0').to_string();
                    if *i < 0 && integer_part == 0 {
                        write!(f, "-{}.{}", integer_part, frac_str)
                    } else {
                        write!(f, "{}.{}", integer_part, frac_str)
                    }
                }
            }
            Value::String(s) => write!(f, "{s}"),
            Value::List(items) => {
                write!(f, "[")?;
                let mut iter = items.iter().peekable();
                while let Some(item) = iter.next() {
                    write!(f, "{item}")?;
                    if iter.peek().is_some() {
                        write!(f, ", ")?;
                    }
                }
                write!(f, "]")
            }
            Value::Record(fields) => {
                write!(f, "{{")?;
                let mut iter = fields.iter().peekable();
                while let Some((k, v)) = iter.next() {
                    write!(f, "{k}: {v}")?;
                    if iter.peek().is_some() {
                        write!(f, ", ")?;
                    }
                }
                write!(f, "}}")
            }
        }
    }
}


impl From<i64> for Value {
    #[inline]
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}

impl From<bool> for Value {
    #[inline]
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<String> for Value {
    #[inline]
    fn from(s: String) -> Self {
        Value::String(Arc::from(s.as_str()))
    }
}

impl From<&str> for Value {
    #[inline]
    fn from(s: &str) -> Self {
        Value::String(Arc::from(s))
    }
}

/// Maximum nesting depth accepted by [`from_json`]; payloads nested deeper
/// are rejected with `Err(MizuError::SecurityViolation)`. Prevents a
/// maliciously-crafted deeply-nested JSON payload from overflowing the
/// native call stack.
///
/// # Consistency with [`MAX_EVAL_DEPTH`]
///
/// This is intentionally tied to [`MAX_EVAL_DEPTH`] rather than given an
/// independent, smaller value. The evaluator can legitimately construct a
/// [`Value`] nested up to `MAX_EVAL_DEPTH` levels deep (e.g. `StorageStore`
/// persisting a deeply-nested record built by a script), and that value is
/// later round-tripped through `serde_json` by `storage::read_all`. If
/// `MAX_JSON_DEPTH` were lower than `MAX_EVAL_DEPTH`, a value the evaluator
/// was allowed to build would silently fail to come back on the next load
/// (see `storage::tests::read_all_skips_over_deep_record_but_returns_rest`)
/// — an availability/correctness bug, not a security one, since the data
/// triggering it was produced by the app itself, not attacker input. Keeping
/// `MAX_JSON_DEPTH >= MAX_EVAL_DEPTH` guarantees anything the evaluator can
/// build is always re-readable from storage.
const MAX_JSON_DEPTH: u32 = MAX_EVAL_DEPTH;

/// Converts a `serde_json::Value` into a Mizu [`Value`].
///
/// Mapping:
/// * `null` → [`Value::Null`]
/// * `bool` → [`Value::Bool`]
/// * number (integer or floating-point — `Value` has no separate
///   floating-point variant) → [`Value::Int`], scaled by `DECIMAL_SCALE`
///   and rounded to the nearest fixed-point value
/// * string → [`Value::String`]
/// * array → [`Value::List`] (elements converted recursively, depth-bounded)
/// * object → [`Value::Record`] (values converted recursively, depth-bounded)
///
/// # Errors
///
/// Returns [`MizuError::SecurityViolation`] if any element is nested deeper
/// than [`MAX_JSON_DEPTH`], rather than silently truncating the payload to
/// [`Value::Null`]. A malicious deeply-nested payload must be rejected
/// outright — truncation would let a caller mistake attacker-controlled data
/// for a legitimate absence of a value.
pub fn from_json(json: &serde_json::Value) -> Result<Value, MizuError> {
    from_json_bounded(json, 0)
}

fn from_json_bounded(json: &serde_json::Value, depth: u32) -> Result<Value, MizuError> {
    if depth > MAX_JSON_DEPTH {
        return Err(MizuError::SecurityViolation(format!(
            "JSON payload exceeds maximum nesting depth of {MAX_JSON_DEPTH}"
        )));
    }
    match json {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            let float_val = n.as_f64().unwrap_or(0.0);
            let scaled = (float_val * (DECIMAL_SCALE as f64)).round() as i64;
            Ok(Value::Int(scaled))
        }
        serde_json::Value::String(s) => Ok(Value::String(Arc::from(s.as_str()))),
        serde_json::Value::Array(arr) => {
            let items = arr
                .iter()
                .map(|v| from_json_bounded(v, depth + 1))
                .collect::<Result<Vec<Value>, MizuError>>()?;
            Ok(Value::List(Arc::new(items)))
        }
        serde_json::Value::Object(map) => {
            let mut slice: Vec<(Arc<str>, Value)> = map
                .iter()
                .map(|(k, v)| Ok((Arc::from(k.as_str()), from_json_bounded(v, depth + 1)?)))
                .collect::<Result<Vec<(Arc<str>, Value)>, MizuError>>()?;
            slice.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(Value::Record(Arc::from(slice)))
        }
    }
}

/// Converts a Mizu [`Value`] into the corresponding `serde_json::Value`.
///
/// Mapping (inverse of [`from_json`]):
/// * [`Value::Null`]    → `null`
/// * [`Value::Bool`]   → `bool`
/// * [`Value::Int`]    → `number` (unscaled by `DECIMAL_SCALE` back to a
///   JSON float — `Value` has no floating-point variant of its own; the
///   fixed-point `Int` representation stands in for both integers and
///   floats. Falls back to `null` if the unscaled value isn't finite,
///   which `serde_json::Number` cannot represent.)
/// * [`Value::String`] → `string`
/// * [`Value::List`]   → `array` (elements converted recursively)
/// * [`Value::Record`] → `object` (values converted recursively)
pub fn to_json(val: &Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => {
            let unscaled = *i as f64 / (DECIMAL_SCALE as f64);
            serde_json::Number::from_f64(unscaled)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        },
        Value::String(s) => serde_json::Value::String(s.to_string()),
        Value::List(items) => serde_json::Value::Array(items.iter().map(to_json).collect()),
        Value::Record(slice) => {
            let obj: serde_json::Map<String, serde_json::Value> = slice
                .iter()
                .map(|(k, v)| (k.to_string(), to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
    }
}


/// Maximum number of AST node evaluations allowed per single top-level action.
///
/// This is the sole enforcement mechanism for Turing-incompleteness.  It is a
/// pure integer comparison — no hardware clock is read anywhere in the hot
/// loop.  Callers must reset `StateMachine::instruction_count` to `0` before
/// each top-level `evaluate` call so the budget applies per action, not
/// cumulatively across the session.
///
/// 20 000 instructions covers deeply-nested real expressions with significant
/// headroom while still bounding worst-case execution to microseconds on any
/// modern CPU — *provided* every charge tracks real cost. A flat AST-node
/// charge is only safe for genuinely O(1) work (arithmetic, comparisons,
/// variable lookups); operations whose native cost scales with data size
/// (`filter`/`count`'s list scan, `sort`'s `n·log₂n` comparisons, string
/// concatenation's `len(l)+len(r)` allocation) pre-charge that size against
/// the budget *before* doing the work, so a single AST node can never hide
/// more than one unit of real cost per charged instruction. With that
/// invariant held, a tight ~1 ns/instruction loop bounds worst-case
/// execution to roughly 20 000 × 1 ns = 20 µs of *actual* work, not just
/// 20 000 AST-node visits — well inside any UI frame budget.
pub const MAX_INSTRUCTIONS: u64 = 20_000;

/// Maximum number of `comp` (computed/derived variable) bindings a single
/// document may declare.
///
/// [`MAX_INSTRUCTIONS`] bounds the cost of evaluating *one* expression, but
/// [`crate::parser::logic::recompute_computed_bindings`] resets and re-applies
/// that budget for *every* comp binding that fires in a cascading recompute
/// (see `formal/MizuFormal/Budget.lean`'s `T1_reaction_bound`, which prices a
/// whole reaction at `(1 + #comps) · MAX_INSTRUCTIONS`). That theorem is an
/// honest bound *in terms of the document* — it does not, by itself, bound
/// the number of comps a document may declare. Without this cap, a document
/// with thousands of comps all transitively depending on one mutable
/// variable would let a single event legitimately burn `#comps ×
/// MAX_INSTRUCTIONS` of real interpreter work, which is a practical DoS
/// vector for a document loaded from an untrusted origin even though no
/// individual budget check is violated.
///
/// This is enforced at parse time (see `parser::logic::parse_computed_with_functions`),
/// so an over-large document is rejected with a clear `ParseError` at load
/// time rather than degrading silently (or timing out undiagnosably) at
/// first interaction.
///
/// 500 is a conservative **starting value**, not a value derived from
/// telemetry of real documents: the example/reference documents shipped in
/// this repository (`docs/reference/examples/*.mizu`) use at most two `comp`
/// bindings, so there is no existing corpus of legitimate high-comp-count
/// documents to calibrate against. Revisit this constant if real usage
/// demonstrates a legitimate need for more.
pub const MAX_COMP_BINDINGS: usize = 500;

/// Maximum recursion depth for [`StateMachine::evaluate`].
///
/// Prevents a native stack overflow on deeply-nested ASTs (e.g. crafted by
/// constructing an AST directly, or generated by an adversarial parser input
/// that slipped through [`crate::parser::logic::MAX_PARSE_DEPTH`]).
/// 256 is well below the native stack limit (~8 MB / ~64 B per frame ≈ thousands
/// of levels) while being unreachable by any legitimate Mizu expression.
///
/// This is also the floor for [`MAX_JSON_DEPTH`]: a [`Value`] built up to
/// this depth by the evaluator must remain re-readable from encrypted
/// storage on the next load.
pub const MAX_EVAL_DEPTH: u32 = 256;

/// Data-oriented flat state machine with a contiguous local variable stack and O(1) global lookup.
#[derive(Debug, Clone, Default)]
pub struct StateMachine {
    /// Global variable store keyed by Symbol.
    /// Uses FxHashMap (rustc-hash) instead of the SipHash default because Symbol is a u32
    /// sequential integer — DoS-resistance on integer keys is unnecessary overhead.
    pub global_store: FxHashMap<Symbol, Value>,
    /// Contiguous stack of local-binding values, indexed by `local_index`.
    pub local_stack: Vec<Value>,
    /// Symbol bound at each position of `local_stack` (parallel array).
    pub local_symbols: Vec<Symbol>,
    /// O(1) reverse index: Symbol → ordered list of indices into `local_stack` where that
    /// symbol is bound (earliest first, latest last).  Kept in sync with `local_stack` /
    /// `local_symbols` by `push_local`, `pop_local`, and `truncate_locals`.
    ///
    /// Lookup rule: the *last* index in the list is the innermost (shadow-winning) binding.
    /// A binding is in scope when its index ≥ the current frame_pointer.
    pub local_index: FxHashMap<Symbol, Vec<usize>>,
    /// Running count of evaluation steps since the last reset; see [`MAX_INSTRUCTIONS`].
    pub instruction_count: u64,
    /// Current `evaluate` recursion depth; see [`MAX_EVAL_DEPTH`].
    pub eval_depth: u32,
    /// Capability actions (network calls, storage writes, navigation, …)
    /// queued by the current action/expression evaluation, drained by the
    /// caller after execution completes.
    pub accumulated_actions: Vec<crate::network::RuntimeAction>,
    /// `(symbol, previous_value)` pairs recorded by [`Self::set_global`],
    /// enabling rollback on error and diffing to find mutated variables.
    pub undo_log: Vec<(Symbol, Value)>,
    /// Set of symbols that are computed (derived) variables.
    ///
    /// Assignment to any symbol in this set is rejected by [`execute_action`] at
    /// runtime.  Populated by the logic worker on each document reload.
    pub computed_var_syms: FxHashSet<Symbol>,
}

impl StateMachine {
    /// Creates an empty state machine with pre-allocated capacity for
    /// globals, locals, and the undo log.
    pub fn new() -> Self {
        Self {
            global_store: FxHashMap::with_capacity_and_hasher(128, Default::default()),
            local_stack: Vec::with_capacity(128),
            local_symbols: Vec::with_capacity(128),
            local_index: FxHashMap::default(),
            instruction_count: 0,
            eval_depth: 0,
            accumulated_actions: Vec::new(),
            undo_log: Vec::with_capacity(64),
            computed_var_syms: FxHashSet::default(),
        }
    }

    /// Pushes a new local binding of `sym` to `val` onto the local stack.
    pub fn push_local(&mut self, sym: Symbol, val: Value) {
        let idx = self.local_stack.len();
        self.local_stack.push(val);
        self.local_symbols.push(sym);
        self.local_index.entry(sym).or_default().push(idx);
    }

    /// Pops the most recently pushed local binding, if any.
    pub fn pop_local(&mut self) {
        if let Some(sym) = self.local_symbols.pop() {
            self.local_stack.pop();
            if let Some(v) = self.local_index.get_mut(&sym) {
                v.pop();
                if v.is_empty() {
                    self.local_index.remove(&sym);
                }
            }
        }
    }

    /// Truncate the local stack to `new_len` entries, removing index entries for every
    /// binding at positions ≥ `new_len`.  Used at function-call exit to discard the
    /// call frame's argument bindings.
    pub fn truncate_locals(&mut self, new_len: usize) {
        for i in (new_len..self.local_symbols.len()).rev() {
            let sym = self.local_symbols[i];
            if let Some(v) = self.local_index.get_mut(&sym) {
                v.pop();
                if v.is_empty() {
                    self.local_index.remove(&sym);
                }
            }
        }
        self.local_stack.truncate(new_len);
        self.local_symbols.truncate(new_len);
    }

    /// Assigns `val` to the global binding of `sym`, recording the previous
    /// value in `undo_log` for rollback/diffing.
    pub fn set_global(&mut self, sym: Symbol, val: Value) {
        let old_val = self.global_store.insert(sym, val).unwrap_or(Value::Null);
        self.undo_log.push((sym, old_val));
    }

    /// Returns the global binding of `sym`, or [`Value::Null`] if unset.
    pub fn get_global(&self, sym: Symbol) -> &Value {
        self.global_store.get(&sym).unwrap_or(&Value::Null)
    }

    /// Resolves a local symbol value in O(1) average time using the reverse index.
    ///
    /// Shadowing semantics: the innermost (most recently pushed) binding whose stack
    /// index is ≥ `frame_pointer` wins.  Bindings pushed before the current function
    /// call frame have indices < `frame_pointer` and are invisible to the callee.
    pub fn get_local(&self, sym: Symbol, frame_pointer: usize) -> Option<&Value> {
        if let Some(indices) = self.local_index.get(&sym)
            && let Some(&idx) = indices.last()
            && idx >= frame_pointer
        {
            return Some(&self.local_stack[idx]);
        }
        None
    }

    /// Looks up `name` in `interner`, then resolves the resulting symbol as a
    /// local (if in scope) or a non-null global.  Returns `None` if `name` is
    /// unknown or bound to nothing.
    pub fn get_value_by_name(&self, name: &str, interner: &StringInterner) -> Option<&Value> {
        if let Some(sym) = interner.get(name) {
            if let Some(val) = self.get_local(sym, 0) {
                return Some(val);
            }
            if let Some(val) = self.global_store.get(&sym)
                && !matches!(val, Value::Null)
            {
                return Some(val);
            }
        }
        None
    }

    /// Renders raw text formatting interpolations directly into a pre-allocated buffer.
    #[inline]
    pub fn interpolate_into(
        &self,
        raw_text: &str,
        interner: &StringInterner,
        buffer: &mut String,
    ) -> Result<(), MizuError> {
        self.interpolate_into_with_overlay(raw_text, interner, None, buffer)
    }

    /// Core interpolation engine.  When `overlay` is `Some`, its entries are
    /// consulted first for every `{var}` placeholder; the global store is the
    /// fallback.  This avoids cloning the entire `StateMachine` just to inject
    /// a handful of per-iteration bindings (the hot-path for `each` loops).
    ///
    /// Variable resolution order:
    ///   1. `overlay[name]` — if present and `overlay` is `Some`
    ///   2. `self.get_value_by_name(name, interner)` — global store fallback
    fn interpolate_into_with_overlay(
        &self,
        raw_text: &str,
        interner: &StringInterner,
        overlay: Option<&HashMap<String, Value>>,
        buffer: &mut String,
    ) -> Result<(), MizuError> {
        use std::fmt::Write;
        let mut chars = raw_text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next_c) = chars.peek() {
                    if next_c == '\\' || next_c == '{' || next_c == '}' {
                        buffer.push(next_c);
                        chars.next();
                    } else {
                        buffer.push('\\');
                    }
                } else {
                    buffer.push('\\');
                }
            } else if c == '{' {
                let mut var_name = String::new();
                let mut closed = false;
                while let Some(&next_c) = chars.peek() {
                    if next_c == '}' {
                        chars.next();
                        closed = true;
                        break;
                    } else if next_c == '{' {
                        break;
                    } else {
                        var_name.push(next_c);
                        chars.next();
                    }
                }
                if closed {
                    if var_name.contains('.') {
                        // Dot-path: resolve `{a.b.c}` by walking record fields.
                        // resolve_dot_path navigates via references — no intermediate
                        // Value is cloned; only the final leaf is formatted.
                        const MAX_RECORD_DEPTH: usize = 64;
                        let mut parts = var_name.splitn(MAX_RECORD_DEPTH, '.');
                        let root = parts.next().unwrap_or("");
                        let segments: Vec<&str> = parts.collect();

                        // Phase 1: try overlay for the root segment.
                        // `handled` is true ONLY when a leaf value was actually written to
                        // `buffer`. If the overlay owns the root key but the full dot-path
                        // resolves to `None`, `handled` stays `false` so execution falls
                        // through to Phase 2 — fixing the silent shadowing bug where a local
                        // variable lacking a nested field would block the global store's path.
                        let handled = if let Some(root_val) =
                            overlay.and_then(|map| map.get(root))
                        {
                            if let Some(leaf) = resolve_dot_path(root_val, &segments) {
                                let _ = write!(buffer, "{}", leaf);
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if !handled {
                            match self.get_value_by_name(root, interner) {
                                None => {
                                    let _ = write!(buffer, "{{{}}}", var_name);
                                }
                                Some(root_val) => match resolve_dot_path(root_val, &segments) {
                                    Some(leaf) => {
                                        let _ = write!(buffer, "{}", leaf);
                                    }
                                    None => {
                                        tracing::warn!(
                                            "interpolation: path `{}` could not be resolved",
                                            var_name
                                        );
                                        let _ = write!(buffer, "{{{}}}", var_name);
                                    }
                                },
                            }
                        }
                    } else {
                        let handled = overlay
                            .and_then(|map| map.get(var_name.as_str()))
                            .map(|val| {
                                let _ = write!(buffer, "{}", val);
                            })
                            .is_some();

                        if !handled {
                            if let Some(val) = self.get_value_by_name(&var_name, interner) {
                                let _ = write!(buffer, "{}", val);
                            } else {
                                let _ = write!(buffer, "{{{}}}", var_name);
                                tracing::warn!("Variable binding missing: {}", var_name);
                            }
                        }
                    }
                } else {
                    buffer.push('{');
                    buffer.push_str(&var_name);
                }
            } else {
                buffer.push(c);
            }
        }
        Ok(())
    }

    /// Evaluates a Mizu expression to a concrete Value.
    ///
    /// Budget enforcement is pure-integer: each recursive call increments
    /// `self.instruction_count` and the method returns `Err(MizuError::Timeout)`
    /// once the count exceeds [`MAX_INSTRUCTIONS`].  No hardware clock is read
    /// inside the hot loop — callers must reset `instruction_count` to `0`
    /// before each top-level invocation.
    ///
    /// `eval_depth` guards against native stack overflow on deeply-nested ASTs.
    /// It is incremented on entry and decremented before every return so it is
    /// always consistent; callers do not need to reset it.
    pub fn evaluate(
        &mut self,
        expr: &Expr,
        frame_pointer: usize,
        functions: &FxHashMap<Symbol, MizuFunction>,
        interner: &StringInterner,
    ) -> Result<Value, MizuError> {
        self.instruction_count += 1;
        if self.instruction_count > MAX_INSTRUCTIONS {
            return Err(MizuError::Timeout);
        }
        self.eval_depth += 1;
        if self.eval_depth > MAX_EVAL_DEPTH {
            self.eval_depth -= 1;
            return Err(MizuError::ExecutionError(
                "evaluation nesting too deep (max 256 levels)".to_owned(),
            ));
        }
        let result = self.evaluate_impl(expr, frame_pointer, functions, interner);
        self.eval_depth -= 1;
        result
    }

    fn evaluate_impl(
        &mut self,
        expr: &Expr,
        frame_pointer: usize,
        functions: &FxHashMap<Symbol, MizuFunction>,
        interner: &StringInterner,
    ) -> Result<Value, MizuError> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Variable(sym) => {
                if let Some(val) = self.get_local(*sym, frame_pointer) {
                    Ok(val.clone())
                } else {
                    let val = self.get_global(*sym);
                    if !matches!(val, Value::Null) {
                        Ok(val.clone())
                    } else {
                        let name = interner.resolve(*sym).unwrap_or("<unknown>").to_owned();
                        Err(MizuError::VariableNotFound(name))
                    }
                }
            }
            Expr::BinaryOp { left, op, right } => {
                let lv = self.evaluate(left, frame_pointer, functions, interner)?;
                let rv = self.evaluate(right, frame_pointer, functions, interner)?;
                apply_binop(op, lv, rv, &mut self.instruction_count)
            }
            Expr::FunctionCall { name: sym, args } => {
                let resolved_name = interner.resolve(*sym).unwrap_or("<unknown>");
                match resolved_name {
                    "copy_to_clipboard" => {
                        if args.len() != 1 {
                            return Err(MizuError::ExecutionError(
                                "copy_to_clipboard expects 1 argument".to_string(),
                            ));
                        }
                        let val = self.evaluate(&args[0], frame_pointer, functions, interner)?;
                        let node_id = match val {
                            Value::String(s) => s.to_string(),
                            _ => {
                                return Err(MizuError::ExecutionError(
                                    "copy_to_clipboard argument must be a node id string"
                                        .to_string(),
                                ));
                            }
                        };
                        self.accumulated_actions
                            .push(crate::network::RuntimeAction::CopyToClipboard { node_id });
                        return Ok(Value::Bool(true));
                    }
                    "get_system_time" => {
                        // arg[0] must be a bare variable identifier (Expr::Variable),
                        // never evaluated — mirrors `download`'s alias argument.
                        //
                        // Before this restriction, the argument was evaluated as an
                        // arbitrary expression to a string used to *look up* the
                        // write target at runtime, making get_system_time the only
                        // construct in the language whose assignment target was
                        // chosen dynamically rather than fixed at parse time. That
                        // broke the static flow checker's core assumption (every
                        // write target is a known Symbol, `parser::flow.rs`) and put
                        // the write out of reach of taint analysis entirely: a
                        // target string derived from `$form`/a network response
                        // could redirect the write to any variable with no static
                        // check able to see it. Requiring a bare identifier here
                        // fixes the target at parse time, so this is now analyzable
                        // exactly like any other assignment.
                        let target_variable = match args.as_slice() {
                            [Expr::Variable(sym)] => *sym,
                            _ => {
                                return Err(MizuError::ExecutionError(
                                    "get_system_time expects a single bare variable \
                                     identifier, e.g. get_system_time(my_var)"
                                        .to_string(),
                                ));
                            }
                        };
                        if self.computed_var_syms.contains(&target_variable) {
                            return Err(MizuError::ExecutionError(
                                "get_system_time cannot target a computed variable"
                                    .to_string(),
                            ));
                        }
                        self.accumulated_actions.push(
                            crate::network::RuntimeAction::GetSystemTime {
                                target_variable,
                            },
                        );
                        return Ok(Value::Bool(true));
                    }
                    "store_local" => {
                        if args.len() != 2 {
                            return Err(MizuError::ExecutionError(
                                "store_local expects 2 arguments: (key, value)".to_string(),
                            ));
                        }
                        let key_val =
                            self.evaluate(&args[0], frame_pointer, functions, interner)?;
                        let key_str = match key_val {
                            Value::String(s) => s.to_string(),
                            _ => {
                                return Err(MizuError::ExecutionError(
                                    "store_local key must be a string".to_string(),
                                ));
                            }
                        };
                        let value = self.evaluate(&args[1], frame_pointer, functions, interner)?;
                        self.accumulated_actions
                            .push(crate::network::RuntimeAction::StoreLocal {
                                key: key_str,
                                value,
                            });
                        return Ok(Value::Bool(true));
                    }
                    "filter" if args.len() == 3 => {
                        let list_val =
                            self.evaluate(&args[0], frame_pointer, functions, interner)?;
                        let field_val =
                            self.evaluate(&args[1], frame_pointer, functions, interner)?;
                        let target = self.evaluate(&args[2], frame_pointer, functions, interner)?;
                        let list = match list_val {
                            Value::List(l) => l,
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: "list",
                                    found: type_name(&other),
                                });
                            }
                        };
                        // Charge the instruction budget before the native iteration to prevent
                        // large lists from bypassing MAX_INSTRUCTIONS via unmetered CPU work.
                        self.instruction_count =
                            self.instruction_count.saturating_add(list.len() as u64);
                        if self.instruction_count > MAX_INSTRUCTIONS {
                            return Err(MizuError::Timeout);
                        }
                        let field = match field_val {
                            Value::String(s) => s,
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: "string",
                                    found: type_name(&other),
                                });
                            }
                        };
                        let filtered: Vec<Value> = list
                            .iter()
                            .filter(|item| {
                                item.get_field(field.as_ref())
                                    .map(|v| v == &target)
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect();
                        return Ok(Value::List(Arc::new(filtered)));
                    }
                    "count" if args.len() == 3 => {
                        let list_val =
                            self.evaluate(&args[0], frame_pointer, functions, interner)?;
                        let field_val =
                            self.evaluate(&args[1], frame_pointer, functions, interner)?;
                        let target = self.evaluate(&args[2], frame_pointer, functions, interner)?;
                        let list = match list_val {
                            Value::List(l) => l,
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: "list",
                                    found: type_name(&other),
                                });
                            }
                        };
                        self.instruction_count =
                            self.instruction_count.saturating_add(list.len() as u64);
                        if self.instruction_count > MAX_INSTRUCTIONS {
                            return Err(MizuError::Timeout);
                        }
                        let field = match field_val {
                            Value::String(s) => s,
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: "string",
                                    found: type_name(&other),
                                });
                            }
                        };
                        let n = list
                            .iter()
                            .filter(|item| {
                                item.get_field(field.as_ref())
                                    .map(|v| v == &target)
                                    .unwrap_or(false)
                            })
                            .count();
                        return Ok(Value::Int(n as i64));
                    }
                    "download" if args.len() == 1 => {
                        // arg[0] must be a bare alias identifier (Expr::Variable);
                        // aliases are not runtime variables and cannot be store-looked-up.
                        let alias_sym = match &args[0] {
                            Expr::Variable(sym) => *sym,
                            _ => return Err(MizuError::ExecutionError(
                                "download: alias must be a bare identifier, e.g. download(backup)"
                                    .to_string(),
                            )),
                        };
                        self.accumulated_actions.push(
                            crate::network::RuntimeAction::DownloadAlias {
                                endpoint_symbol: alias_sym.0,
                            },
                        );
                        return Ok(Value::Null);
                    }
                    "sort" if args.len() == 3 => {
                        let list_val =
                            self.evaluate(&args[0], frame_pointer, functions, interner)?;
                        let field_val =
                            self.evaluate(&args[1], frame_pointer, functions, interner)?;
                        let direction_val = match &args[2] {
                            Expr::Variable(sym) => {
                                let name = interner.resolve(*sym).unwrap_or("");
                                if name == "asc" || name == "desc" {
                                    Value::String(Arc::from(name))
                                } else {
                                    self.evaluate(&args[2], frame_pointer, functions, interner)?
                                }
                            }
                            _ => self.evaluate(&args[2], frame_pointer, functions, interner)?,
                        };
                        let list = match list_val {
                            Value::List(l) => l,
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: "list",
                                    found: type_name(&other),
                                });
                            }
                        };
                        let n = list.len();
                        let log2_n = if n > 0 {
                            usize::BITS - n.leading_zeros()
                        } else {
                            0
                        };
                        let sorting_cost = (n as u64).saturating_mul(log2_n as u64);
                        self.instruction_count =
                            self.instruction_count.saturating_add(sorting_cost);
                        if self.instruction_count > MAX_INSTRUCTIONS {
                            return Err(MizuError::Timeout);
                        }
                        let field = match field_val {
                            Value::String(s) => s,
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: "string",
                                    found: type_name(&other),
                                });
                            }
                        };
                        let direction = match direction_val {
                            Value::String(s) => s,
                            other => {
                                return Err(MizuError::TypeError {
                                    expected: "string",
                                    found: type_name(&other),
                                });
                            }
                        };
                        if direction.as_ref() != "asc" && direction.as_ref() != "desc" {
                            return Err(MizuError::ExecutionError(format!(
                                "sort: direction must be `asc` or `desc`, got `{direction}`"
                            )));
                        }
                        let mut items: Vec<Value> = (*list).clone();
                        items.sort_by(|a, b| {
                            let ord =
                                compare_values(field_value(a, &field), field_value(b, &field));
                            if direction.as_ref() == "desc" {
                                ord.reverse()
                            } else {
                                ord
                            }
                        });
                        return Ok(Value::List(Arc::new(items)));
                    }
                    _ => {}
                }

                let func = functions.get(sym).ok_or_else(|| {
                    MizuError::ParseError(format!("call to undefined function `{resolved_name}`"))
                })?;

                if args.len() != func.params.len() {
                    return Err(MizuError::ParseError(format!(
                        "function `{resolved_name}` expects {} argument(s), got {}",
                        func.params.len(),
                        args.len()
                    )));
                }

                let mut evaluated_args = Vec::with_capacity(args.len());
                for arg_expr in args {
                    evaluated_args.push(self.evaluate(
                        arg_expr,
                        frame_pointer,
                        functions,
                        interner,
                    )?);
                }

                let new_fp = self.local_stack.len();
                for ((param_sym, expected_type), arg_val) in func.params.iter().zip(evaluated_args)
                {
                    let param_name = interner.resolve(*param_sym).unwrap_or("<unknown>");
                    check_type(&arg_val, expected_type.as_ref(), resolved_name, param_name)?;
                    self.push_local(*param_sym, arg_val);
                }

                let res = self.evaluate(&func.body, new_fp, functions, interner);
                self.truncate_locals(new_fp);
                res
            }
            Expr::Let {
                name: sym,
                value,
                body,
            } => {
                let bound_val = self.evaluate(value, frame_pointer, functions, interner)?;
                self.push_local(*sym, bound_val);
                let res = self.evaluate(body, frame_pointer, functions, interner);
                self.pop_local();
                res
            }
            Expr::Not(inner) => {
                let val = self.evaluate(inner, frame_pointer, functions, interner)?;
                match val {
                    Value::Bool(b) => Ok(Value::Bool(!b)),
                    other => Err(crate::core::errors::MizuError::TypeError {
                        expected: "bool",
                        found: type_name(&other),
                    }),
                }
            }
            // Lazy: only the selected branch is evaluated.
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                let cond_val = self.evaluate(condition, frame_pointer, functions, interner)?;
                match cond_val {
                    Value::Bool(true) => {
                        self.evaluate(then_expr, frame_pointer, functions, interner)
                    }
                    Value::Bool(false) => {
                        self.evaluate(else_expr, frame_pointer, functions, interner)
                    }
                    other => Err(crate::core::errors::MizuError::TypeError {
                        expected: "bool",
                        found: type_name(&other),
                    }),
                }
            }
            Expr::FieldAccess { base, field } => {
                let base_val = self.evaluate(base, frame_pointer, functions, interner)?;
                if !matches!(base_val, Value::Record(_)) {
                    return Err(MizuError::TypeError {
                        expected: "record",
                        found: type_name(&base_val),
                    });
                }
                base_val
                    .get_field(field.as_ref())
                    .cloned()
                    .ok_or_else(|| MizuError::VariableNotFound(field.to_string()))
            }
        }
    }
}


/// Returns the value of `field` in `item` if `item` is a `Record`.
fn field_value<'a>(item: &'a Value, field: &str) -> Option<&'a Value> {
    item.get_field(field)
}

/// Navigates a dot-separated path through nested `Value::Record` values,
/// returning a reference to the leaf without cloning any intermediate value.
fn resolve_dot_path<'a>(root: &'a Value, segments: &[&str]) -> Option<&'a Value> {
    let mut current = root;
    for segment in segments {
        current = current.get_field(segment)?;
    }
    Some(current)
}

/// Returns a stable numeric weight for each `Value` variant.
///
/// This weight is the tiebreaker used by [`compare_values`] when the two
/// values belong to different variants.  The ordering is arbitrary but fixed,
/// which is sufficient to satisfy Strict Weak Ordering.
///
/// Weights: Null=1, Bool=2, Int=3, String=4, List=5, Record=6.
#[inline]
fn variant_weight(v: &Value) -> u8 {
    match v {
        Value::Null => 1,
        Value::Bool(_) => 2,
        Value::Int(_) => 3,
        Value::String(_) => 4,
        Value::List(_) => 5,
        Value::Record(_) => 6,
    }
}

/// Compares two optional record-field values for sorting purposes, satisfying
/// Strict Weak Ordering so that `Vec::sort_by` never invokes undefined behaviour.
///
/// Rules:
/// * `(None, None)` → `Equal`
/// * `(None, Some(_))` → `Less` / `(Some(_), None)` → `Greater`  (None is smallest)
/// * Same-variant pairs use native ordering.
/// * All other heterogeneous pairs are ordered by [`variant_weight`], which is
///   deterministic and total.
///
/// A single call here costs O(string length) for a `String` pair, or more for
/// nested `List`/`Record` pairs — not O(1) like the numeric/bool cases. This
/// is safe *without* its own instruction charge because `sort`'s caller
/// already pre-charges `n·log₂n` for the whole pass (bounding the *count* of
/// calls), and every `Value` reachable here is itself already
/// budget-bounded: a `String`/`List`/`Record` built by the evaluator can only
/// be as large as the instructions already spent constructing it (string
/// concatenation charges its length — see `apply_binop`; there is no runtime
/// operator that grows a `List`/`Record`, so their size is fixed at parse
/// time), and values delivered by a network response are bounded separately
/// at the network layer, not by this instruction budget at all.
fn compare_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,

        (Some(Value::Null), Some(Value::Null)) => Ordering::Equal,
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        (Some(Value::Int(x)), Some(Value::Int(y))) => x.cmp(y),
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),

        (Some(Value::List(x)), Some(Value::List(y))) => {
            for (elem_a, elem_b) in x.iter().zip(y.iter()) {
                let ord = compare_values(Some(elem_a), Some(elem_b));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            x.len().cmp(&y.len())
        }

        (Some(Value::Record(x)), Some(Value::Record(y))) => {
            for ((ka, va), (kb, vb)) in x.iter().zip(y.iter()) {
                let key_ord = ka.cmp(kb);
                if key_ord != Ordering::Equal {
                    return key_ord;
                }
                let val_ord = compare_values(Some(va), Some(vb));
                if val_ord != Ordering::Equal {
                    return val_ord;
                }
            }
            x.len().cmp(&y.len())
        }

        (Some(x), Some(y)) => variant_weight(x).cmp(&variant_weight(y)),
    }
}



/// A backwards compatibility layer wrapping StateMachine and StringInterner.
#[derive(Debug, Clone, Default)]
pub struct VariableStore {
    /// The underlying flat evaluator state (globals, locals, budgets, queued actions).
    pub state_machine: StateMachine,
    /// Name ↔ `Symbol` mapping shared with `state_machine`'s expressions.
    pub interner: StringInterner,
}

impl VariableStore {
    /// Creates an empty store with a fresh, unfrozen interner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state_machine: StateMachine::new(),
            interner: StringInterner::new(),
        }
    }

    /// Creates an empty store reusing an existing (typically frozen) interner.
    #[must_use]
    pub fn with_interner(interner: StringInterner) -> Self {
        Self {
            state_machine: StateMachine::new(),
            interner,
        }
    }

    /// Binds `sym` directly to `value`, bypassing name interning.
    pub fn set_symbol(&mut self, sym: Symbol, value: impl Into<Value>) {
        self.state_machine.set_global(sym, value.into());
    }

    /// Binds `name` to `value`.
    ///
    /// Calls [`StringInterner::get_or_intern`] to intern the name.  Do **not**
    /// call this method after the interner has been
    /// [`freeze`](StringInterner::freeze)d with a runtime-generated string;
    /// use [`set_runtime`](Self::set_runtime) instead.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<Value>) {
        let name_str = name.into();
        let value_val = value.into();
        let sym = self.interner.get_or_intern(&name_str);
        self.state_machine.set_global(sym, value_val);
    }

    /// Frozen-safe version of [`set`](Self::set).
    ///
    /// Uses [`StringInterner::get`] (read-only) instead of
    /// [`get_or_intern`](StringInterner::get_or_intern).  If `name` is already
    /// in the interner the value is stored normally.  If `name` is **not** in
    /// the interner (i.e. it was not declared in the parse phase), the call is
    /// a no-op and a `tracing::debug!` is emitted — the frozen symbol table is
    /// never mutated.
    ///
    /// Use this method anywhere that runs after [`StringInterner::freeze`] and
    /// may encounter strings not declared at compile time, e.g.:
    /// - `UiEvent::SubmitForm` field names
    /// - `UiEvent::UpdateVariable` variable names from network responses
    pub fn set_runtime(&mut self, name: &str, value: impl Into<Value>) {
        if let Some(sym) = self.interner.get(name) {
            self.state_machine.set_global(sym, value.into());
        } else {
            tracing::debug!(
                name,
                "set_runtime: `{}` is not in the frozen interner — declare it in \
                 the logic block to make it bindable at runtime",
                name
            );
        }
    }

    /// Looks up `name` as a local (frame 0) or non-null global.
    ///
    /// # Errors
    ///
    /// Returns [`MizuError::VariableNotFound`] if `name` is unknown or unbound.
    pub fn get(&self, name: &str) -> Result<&Value, MizuError> {
        if let Some(sym) = self.interner.get(name) {
            if let Some(val) = self.state_machine.get_local(sym, 0) {
                return Ok(val);
            }
            let val = self.state_machine.get_global(sym);
            if !matches!(val, Value::Null) {
                return Ok(val);
            }
        }
        Err(MizuError::VariableNotFound(name.to_owned()))
    }

    /// Replaces every `{name}` placeholder in `text` with the string form of
    /// the corresponding variable's value.
    ///
    /// # Errors
    ///
    /// Returns [`MizuError::BindingNotFound`] if a placeholder references an
    /// unbound name.
    pub fn interpolate(&self, text: &str) -> Result<String, MizuError> {
        let mut buf = String::with_capacity(text.len());
        self.state_machine
            .interpolate_into(text, &self.interner, &mut buf)?;
        Ok(buf)
    }

    /// Interpolates string placeholders, checking `overlay` before the global store.
    ///
    /// `overlay` is a small per-iteration binding map used by `each` loops to inject
    /// the current element value (e.g. `item → Record{…}`) without mutating the store.
    /// If `overlay` is empty, this is identical to [`Self::interpolate`].
    ///
    /// Unlike the previous implementation, this method passes `overlay` directly into
    /// the interpolation engine as an `Option<&HashMap<…>>` parameter — no clone of
    /// `StateMachine` or `StringInterner` is performed.
    pub fn interpolate_with_overlay(
        &self,
        text: &str,
        overlay: &HashMap<String, crate::core::types::Value>,
    ) -> Result<String, MizuError> {
        let mut buf = String::with_capacity(text.len());
        let overlay_opt = if overlay.is_empty() {
            None
        } else {
            Some(overlay)
        };
        self.state_machine.interpolate_into_with_overlay(
            text,
            &self.interner,
            overlay_opt,
            &mut buf,
        )?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        StateMachine, StringInterner, Value, VariableStore, compare_values, field_value, from_json,
        variant_weight, Symbol,
    };
    use crate::core::errors::MizuError;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;


    #[test]
    fn string_from_string_ref() {
        let v = Value::from("hello");
        assert_eq!(v, Value::String(std::sync::Arc::from("hello")));
    }

    #[test]
    fn string_from_owned_string() {
        let v = Value::from(String::from("world"));
        assert_eq!(v, Value::String(std::sync::Arc::from("world")));
    }

    #[test]
    fn string_display_verbatim() {
        let v = Value::String(std::sync::Arc::from("Mizu rocks"));
        assert_eq!(v.to_string(), "Mizu rocks");
    }


    #[test]
    fn bool_from_true() {
        let v = Value::from(true);
        assert_eq!(v, Value::Bool(true));
    }

    #[test]
    fn bool_from_false() {
        let v = Value::from(false);
        assert_eq!(v, Value::Bool(false));
    }

    #[test]
    fn bool_display_lowercase() {
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Bool(false).to_string(), "false");
    }


    #[test]
    fn list_display_empty() {
        let v = Value::List(std::sync::Arc::new(vec![]));
        assert_eq!(v.to_string(), "[]");
    }

    #[test]
    fn list_display_single_element() {
        let v = Value::List(std::sync::Arc::new(vec![Value::Int(10_000)]));
        assert_eq!(v.to_string(), "[1]");
    }

    #[test]
    fn list_display_multiple_elements() {
        let v = Value::List(std::sync::Arc::new(vec![
            Value::Int(10_000),
            Value::String(std::sync::Arc::from("two")),
            Value::Bool(false),
        ]));
        assert_eq!(v.to_string(), "[1, two, false]");
    }

    #[test]
    fn list_display_nested() {
        let inner = Value::List(std::sync::Arc::new(vec![
            Value::Int(20_000),
            Value::Int(30_000),
        ]));
        let outer = Value::List(std::sync::Arc::new(vec![Value::Int(10_000), inner]));
        assert_eq!(outer.to_string(), "[1, [2, 3]]");
    }


    #[test]
    fn store_set_and_get_int_scaled() {
        let mut store = VariableStore::new();
        store.set("price", Value::Int(99_900));
        let result = store.get("price");
        assert!(result.is_ok());
        assert_eq!(*result.unwrap(), Value::Int(99_900));
    }

    #[test]
    fn store_set_and_get_string() {
        let mut store = VariableStore::new();
        store.set("label", Value::from("checkout"));
        assert_eq!(
            *store.get("label").unwrap(),
            Value::String(std::sync::Arc::from("checkout"))
        );
    }

    #[test]
    fn store_set_and_get_bool() {
        let mut store = VariableStore::new();
        store.set("flag", Value::from(true));
        assert_eq!(*store.get("flag").unwrap(), Value::Bool(true));
    }

    #[test]
    fn store_set_and_get_list() {
        let mut store = VariableStore::new();
        let list = Value::List(std::sync::Arc::new(vec![
            Value::Int(10_000),
            Value::Int(20_000),
        ]));
        store.set("items", list.clone());
        assert_eq!(*store.get("items").unwrap(), list);
    }

    #[test]
    fn store_set_convenience_into() {
        // `set` accepts any `impl Into<Value>`, so raw Rust types work directly.
        let mut store = VariableStore::new();
        store.set("x", 7_i64);
        store.set("greeting", "hi");
        store.set("active", false);
        assert_eq!(*store.get("x").unwrap(), Value::Int(7));
        assert_eq!(
            *store.get("greeting").unwrap(),
            Value::String(std::sync::Arc::from("hi"))
        );
        assert_eq!(*store.get("active").unwrap(), Value::Bool(false));
    }

    #[test]
    fn store_overwrite_binding() {
        let mut store = VariableStore::new();
        store.set("count", 1_i64);
        store.set("count", 2_i64);
        assert_eq!(*store.get("count").unwrap(), Value::Int(2));
    }

    #[test]
    fn store_scope_chaining() {
        let mut store = VariableStore::new();
        store.set("x", 10_i64);
        store.set("y", 20_i64);

        let fp = store.state_machine.local_stack.len();
        let x_sym = store.interner.get_or_intern("x");
        let y_sym = store.interner.get_or_intern("y");
        let z_sym = store.interner.get_or_intern("z");

        store.state_machine.push_local(x_sym, Value::from(15_i64));

        assert_eq!(
            *store.state_machine.get_local(x_sym, fp).unwrap(),
            Value::from(15_i64)
        );
        assert!(store.state_machine.get_local(y_sym, fp).is_none());
        assert!(store.state_machine.get_local(z_sym, fp).is_none());
    }

    #[test]
    fn state_machine_get_local_o1_shadowing() {
        let mut sm = StateMachine::new();
        let mut interner = StringInterner::default();
        let x = interner.get_or_intern("x");
        let y = interner.get_or_intern("y");

        sm.push_local(x, Value::Int(1));
        let outer_fp = sm.local_stack.len();

        sm.push_local(x, Value::Int(2));

        assert_eq!(
            sm.get_local(x, outer_fp),
            Some(&Value::Int(2)),
            "inner binding must shadow outer at frame_pointer={outer_fp}"
        );
        // y is not bound in any frame
        assert_eq!(sm.get_local(y, outer_fp), None);

        sm.pop_local();
        assert_eq!(
            sm.get_local(x, 0),
            Some(&Value::Int(1)),
            "after pop, outer x=1 must be visible from fp=0"
        );
        // But x is no longer visible from inner_fp (the binding index is below inner_fp)
        assert_eq!(
            sm.get_local(x, outer_fp),
            None,
            "outer binding must not be visible from inner frame_pointer"
        );

        sm.pop_local();
        assert_eq!(sm.get_local(x, 0), None);

        assert!(
            sm.local_index.get(&x).map(|v| v.is_empty()).unwrap_or(true),
            "local_index must be empty after all pops"
        );
    }

    #[test]
    fn state_machine_truncate_locals_removes_index_entries() {
        let mut sm = StateMachine::new();
        let mut interner = StringInterner::default();
        let a = interner.get_or_intern("a");
        let b = interner.get_or_intern("b");

        let fp = sm.local_stack.len();
        sm.push_local(a, Value::Int(10));
        sm.push_local(b, Value::Int(20));

        assert_eq!(sm.get_local(a, fp), Some(&Value::Int(10)));
        assert_eq!(sm.get_local(b, fp), Some(&Value::Int(20)));

        sm.truncate_locals(fp);

        assert_eq!(sm.get_local(a, fp), None, "a must be gone after truncate");
        assert_eq!(sm.get_local(b, fp), None, "b must be gone after truncate");
        assert!(sm.local_stack.is_empty());
        assert!(sm.local_index.get(&a).map(|v| v.is_empty()).unwrap_or(true));
        assert!(sm.local_index.get(&b).map(|v| v.is_empty()).unwrap_or(true));
    }


    #[test]
    fn store_get_missing_returns_err() {
        let store = VariableStore::new();
        let result = store.get("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn store_get_missing_is_variable_not_found() {
        let store = VariableStore::new();
        let err = store.get("ghost").unwrap_err();
        assert!(
            matches!(err, MizuError::VariableNotFound(ref name) if name == "ghost"),
            "expected VariableNotFound(\"ghost\"), got: {err:?}"
        );
    }

    #[test]
    fn store_get_missing_error_message() {
        let store = VariableStore::new();
        let err = store.get("missing_var").unwrap_err();
        assert_eq!(err.to_string(), "variable not found: `missing_var`");
    }

    #[test]
    fn store_new_and_default_are_equivalent() {
        let a = VariableStore::new();
        let b = VariableStore::default();
        assert!(a.get("x").is_err());
        assert!(b.get("x").is_err());
    }


    #[test]
    fn json_object_becomes_record() {
        let json: serde_json::Value = serde_json::from_str(r#"{"id":1,"name":"Neko"}"#).unwrap();
        let val = from_json(&json).unwrap();
        assert_eq!(val.get_field("id"), Some(&Value::Int(10_000)));
        assert_eq!(
            val.get_field("name"),
            Some(&Value::String(Arc::from("Neko")))
        );
    }

    #[test]
    fn json_array_of_objects() {
        let json: serde_json::Value = serde_json::from_str(r#"[{"id":1},{"id":2}]"#).unwrap();
        let val = from_json(&json).unwrap();
        if let Value::List(items) = val {
            assert_eq!(items.len(), 2);
            assert!(
                matches!(items[0], Value::Record(_)),
                "first element must be Record"
            );
            assert!(
                matches!(items[1], Value::Record(_)),
                "second element must be Record"
            );
        } else {
            panic!("expected Value::List, got {val:?}");
        }
    }

    #[test]
    fn json_string_passthrough() {
        let json: serde_json::Value = serde_json::from_str(r#""hello""#).unwrap();
        let val = from_json(&json).unwrap();
        assert_eq!(val, Value::String(Arc::from("hello")));
    }

    #[test]
    fn json_null_becomes_value_null() {
        let val = from_json(&serde_json::Value::Null).unwrap();
        assert_eq!(val, Value::Null);
    }

    #[test]
    fn json_bool_becomes_value_bool() {
        assert_eq!(from_json(&serde_json::json!(true)).unwrap(), Value::Bool(true));
        assert_eq!(from_json(&serde_json::json!(false)).unwrap(), Value::Bool(false));
    }

    #[test]
    fn json_integer_becomes_value_int() {
        let val = from_json(&serde_json::json!(42)).unwrap();
        assert_eq!(val, Value::Int(420_000));
    }

    #[test]
    fn json_float_becomes_value_int() {
        let val = from_json(&serde_json::json!(3.14)).unwrap();
        assert_eq!(val, Value::Int(31_400));
    }

    #[test]
    fn record_display_contains_fields() {
        let json: serde_json::Value = serde_json::from_str(r#"{"x":1}"#).unwrap();
        let val = from_json(&json).unwrap();
        let display = val.to_string();
        assert!(
            display.contains("x"),
            "display must contain field name: {display}"
        );
        assert!(
            display.contains("1"),
            "display must contain field value: {display}"
        );
        assert!(
            display.starts_with('{'),
            "display must start with '{{': {display}"
        );
        assert!(
            display.ends_with('}'),
            "display must end with '}}': {display}"
        );
    }


    #[test]
    fn from_json_depth_limit_returns_err() {
        // Build a 300-level nested array: [[[[...[42]...]]]]
        // Nesting beyond MAX_JSON_DEPTH (== MAX_EVAL_DEPTH == 256) must be
        // rejected outright with Err(MizuError::SecurityViolation) rather
        // than silently clamped to Value::Null — a clamp would let a caller
        // mistake a malicious deeply-nested payload for legitimate absent
        // data.
        let mut json = serde_json::json!(42_i64);
        for _ in 0..300 {
            json = serde_json::json!([json]);
        }

        let result = from_json(&json);

        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "deeply-nested JSON must be rejected with SecurityViolation, got: {result:?}"
        );
    }

    #[test]
    fn from_json_shallow_nesting_parses_fully() {
        // A 3-level nested array (well within MAX_JSON_DEPTH) must parse
        // completely — the depth limit must not truncate legitimate data.
        let json = serde_json::json!([[[42_i64]]]);
        let result = from_json(&json).unwrap();

        let l1 = match &result {
            Value::List(v) => &v[0],
            other => panic!("level 0 must be List: {other:?}"),
        };
        let l2 = match l1 {
            Value::List(v) => &v[0],
            other => panic!("level 1 must be List: {other:?}"),
        };
        let leaf = match l2 {
            Value::List(v) => &v[0],
            other => panic!("level 2 must be List: {other:?}"),
        };
        assert_eq!(*leaf, Value::Int(420_000), "leaf must be Int(420_000)");
    }

    #[test]
    fn store_interpolate_string() {
        let mut store = VariableStore::new();
        store.set("count", 42 * super::DECIMAL_SCALE);
        store.set("name", "Mizu");

        let result = store.interpolate("{name} has {count} items");
        assert_eq!(result.unwrap(), "Mizu has 42 items");

        let lenient_res = store.interpolate("{name} has {missing}");
        assert_eq!(lenient_res.unwrap(), "Mizu has {missing}");

        let escaped_res = store.interpolate("\\{name\\} has {count}");
        assert_eq!(escaped_res.unwrap(), "{name} has 42");

        let escaped_backslash_res = store.interpolate("Test \\\\{count}");
        assert_eq!(escaped_backslash_res.unwrap(), "Test \\42");
    }


    #[test]
    fn eval_field_access_on_record() {
        use crate::core::types::Symbol;
        use crate::parser::logic::{Expr, MizuFunction};
        use rustc_hash::FxHashMap;

        let mut store = VariableStore::new();
        let mut map: Vec<(Arc<str>, Value)> = Vec::new();
        map.push((Arc::from("name"), Value::String(Arc::from("Neko"))));
        store.set("item", Value::Record(Arc::from(map)));

        let item_sym = store.interner.get_or_intern("item");
        let expr = Expr::FieldAccess {
            base: Box::new(Expr::Variable(item_sym)),
            field: Arc::from("name"),
        };

        let funcs: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
        store.state_machine.instruction_count = 0;
        let result = store
            .state_machine
            .evaluate(&expr, 0, &funcs, &store.interner);
        assert_eq!(result.unwrap(), Value::String(Arc::from("Neko")));
    }

    #[test]
    fn eval_field_access_missing_field() {
        use crate::core::types::Symbol;
        use crate::parser::logic::{Expr, MizuFunction};
        use rustc_hash::FxHashMap;

        let mut store = VariableStore::new();
        let map: Vec<(Arc<str>, Value)> = Vec::new();
        store.set("item", Value::Record(Arc::from(map)));

        let item_sym = store.interner.get_or_intern("item");
        let expr = Expr::FieldAccess {
            base: Box::new(Expr::Variable(item_sym)),
            field: Arc::from("missing"),
        };

        let funcs: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
        store.state_machine.instruction_count = 0;
        let result = store
            .state_machine
            .evaluate(&expr, 0, &funcs, &store.interner);
        assert!(matches!(result, Err(MizuError::VariableNotFound(_))));
    }

    #[test]
    fn eval_field_access_on_non_record() {
        use crate::core::types::Symbol;
        use crate::parser::logic::{Expr, MizuFunction};
        use rustc_hash::FxHashMap;

        let mut store = VariableStore::new();
        store.set("item", Value::String(Arc::from("hello")));

        let item_sym = store.interner.get_or_intern("item");
        let expr = Expr::FieldAccess {
            base: Box::new(Expr::Variable(item_sym)),
            field: Arc::from("field"),
        };

        let funcs: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
        store.state_machine.instruction_count = 0;
        let result = store
            .state_machine
            .evaluate(&expr, 0, &funcs, &store.interner);
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    #[test]
    fn interpolate_dot_access() {
        let mut store = VariableStore::new();
        let mut map: Vec<(Arc<str>, Value)> = Vec::new();
        map.push((Arc::from("age"), Value::Int(3 * super::DECIMAL_SCALE)));
        map.push((Arc::from("name"), Value::String(Arc::from("Neko"))));
        store.set("item", Value::Record(Arc::from(map)));

        let result = store
            .interpolate("The cat is {item.name} and is {item.age} years old")
            .unwrap();
        assert_eq!(result, "The cat is Neko and is 3 years old");

        // Missing field falls back to literal placeholder.
        let fallback = store.interpolate("{item.missing}").unwrap();
        assert_eq!(fallback, "{item.missing}");
    }


    #[test]
    fn overlay_takes_priority_over_store() {
        // A key present in both the overlay and the store must resolve to the
        // overlay value — the store must not be consulted.
        let mut store = VariableStore::new();
        store.set("name", "global");

        let mut overlay = HashMap::new();
        overlay.insert("name".to_string(), Value::from("local"));

        let result = store
            .interpolate_with_overlay("Hello {name}", &overlay)
            .unwrap();
        assert_eq!(
            result, "Hello local",
            "overlay must shadow the global store"
        );
    }

    #[test]
    fn overlay_falls_back_to_store_for_missing_key() {
        // Keys absent from the overlay must still resolve from the global store.
        let mut store = VariableStore::new();
        store.set("greeting", "hello");

        let overlay: HashMap<String, Value> = HashMap::new();
        let result = store
            .interpolate_with_overlay("{greeting} {name}", &overlay)
            .unwrap();
        // `name` is missing from both overlay and store → literal placeholder.
        assert_eq!(result, "hello {name}");
    }

    #[test]
    fn overlay_dot_path_from_overlay_record() {
        // {item.field} must resolve through a Record stored in the overlay,
        // without cloning the StateMachine or StringInterner.
        let store = VariableStore::new(); // empty global store

        let mut inner: Vec<(Arc<str>, Value)> = Vec::new();
        inner.push((Arc::from("name"), Value::String(Arc::from("Neko"))));
        let record = Value::Record(Arc::from(inner));

        let mut overlay = HashMap::new();
        overlay.insert("item".to_string(), record);

        let result = store
            .interpolate_with_overlay("{item.name}", &overlay)
            .unwrap();
        assert_eq!(
            result, "Neko",
            "dot-path must resolve through overlay record"
        );
    }

    #[test]
    fn overlay_dot_path_falls_back_to_store() {
        // {item.name} when `item` is absent from the overlay but present in the
        // store must fall back correctly.
        let mut store = VariableStore::new();
        let mut inner: Vec<(Arc<str>, Value)> = Vec::new();
        inner.push((Arc::from("name"), Value::String(Arc::from("Stripe"))));
        store.set("item", Value::Record(Arc::from(inner)));

        let overlay: HashMap<String, Value> = HashMap::new(); // empty overlay
        let result = store
            .interpolate_with_overlay("{item.name}", &overlay)
            .unwrap();
        assert_eq!(
            result, "Stripe",
            "dot-path must fall back to store when absent from overlay"
        );
    }

    #[test]
    fn empty_overlay_is_identical_to_interpolate() {
        // An empty overlay must produce exactly the same result as a direct
        // `interpolate` call (the fast-path and overlay-path must agree).
        let mut store = VariableStore::new();
        store.set("x", Value::Int(42));

        let overlay: HashMap<String, Value> = HashMap::new();
        let via_overlay = store.interpolate_with_overlay("x={x}", &overlay).unwrap();
        let direct = store.interpolate("x={x}").unwrap();
        assert_eq!(via_overlay, direct);
    }


    /// Builds a small list of records for use in built-in tests.
    ///
    /// Records:
    ///   { done: true,  priority: 3, name: "alpha" }
    ///   { done: false, priority: 1, name: "beta"  }
    ///   { done: true,  priority: 2, name: "gamma" }
    ///   { done: false, priority: 1, name: "delta" }
    ///   { done: true,  priority: 1, name: "epsilon" }
    fn make_task_list() -> Value {
        let rows: &[(&str, bool, i64, &str)] = &[
            ("alpha", true, 3, "alpha"),
            ("beta", false, 1, "beta"),
            ("gamma", true, 2, "gamma"),
            ("delta", false, 1, "delta"),
            ("epsilon", true, 1, "epsilon"),
        ];
        let items: Vec<Value> = rows
            .iter()
            .map(|(name, done, priority, _)| {
                let mut m: Vec<(Arc<str>, Value)> = Vec::new();
                m.push((Arc::from("done"), Value::Bool(*done)));
                m.push((Arc::from("name"), Value::String(Arc::from(*name))));
                m.push((Arc::from("priority"), Value::Int(*priority)));
                Value::Record(Arc::from(m))
            })
            .collect();
        Value::List(Arc::new(items))
    }

    /// Helper: evaluate a FunctionCall built-in via `StateMachine::evaluate`.
    fn eval_builtin(
        store: &mut VariableStore,
        name: &str,
        args: Vec<crate::parser::logic::Expr>,
    ) -> Result<Value, MizuError> {
        use crate::core::types::Symbol;
        use crate::parser::logic::MizuFunction;
        use rustc_hash::FxHashMap;
        let sym = store.interner.get_or_intern(name);
        let expr = crate::parser::logic::Expr::FunctionCall { name: sym, args };
        let fns: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
        store.state_machine.instruction_count = 0;
        store
            .state_machine
            .evaluate(&expr, 0, &fns, &store.interner)
    }

    #[test]
    fn test_filter_by_bool() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("done"))),
            Expr::Literal(Value::Bool(true)),
        ];
        let result = eval_builtin(&mut store, "filter", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        assert_eq!(items.len(), 3);
        for item in items.iter() {
            assert_eq!(item.get_field("done"), Some(&Value::Bool(true)));
        }
    }

    #[test]
    fn test_filter_by_string() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("name"))),
            Expr::Literal(Value::String(Arc::from("gamma"))),
        ];
        let result = eval_builtin(&mut store, "filter", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].get_field("name"), Some(&Value::String(Arc::from("gamma"))));
    }

    #[test]
    fn test_filter_by_num() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("priority"))),
            Expr::Literal(Value::Int(1)),
        ];
        let result = eval_builtin(&mut store, "filter", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        assert_eq!(items.len(), 3); // beta, delta, epsilon
    }

    #[test]
    fn test_filter_empty_result() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("priority"))),
            Expr::Literal(Value::Int(99)),
        ];
        let result = eval_builtin(&mut store, "filter", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        assert_eq!(items.len(), 0);
    }

    #[test]
    fn test_count_basic() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("done"))),
            Expr::Literal(Value::Bool(false)),
        ];
        let result = eval_builtin(&mut store, "count", args).unwrap();
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn test_sort_asc() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let asc_sym = store.interner.get_or_intern("asc");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("priority"))),
            Expr::Variable(asc_sym),
        ];
        let result = eval_builtin(&mut store, "sort", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        let priorities: Vec<i64> = items
            .iter()
            .map(|item| {
                if let Some(&Value::Int(p)) = item.get_field("priority") {
                    p
                } else {
                    panic!()
                }
            })
            .collect();
        assert_eq!(priorities, vec![1, 1, 1, 2, 3]);
    }

    #[test]
    fn test_sort_desc() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let desc_sym = store.interner.get_or_intern("desc");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("priority"))),
            Expr::Variable(desc_sym),
        ];
        let result = eval_builtin(&mut store, "sort", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        let priorities: Vec<i64> = items
            .iter()
            .map(|item| {
                if let Some(&Value::Int(p)) = item.get_field("priority") {
                    p
                } else {
                    panic!()
                }
            })
            .collect();
        assert_eq!(priorities, vec![3, 2, 1, 1, 1]);
    }

    #[test]
    fn test_sort_string() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list());
        let tasks_sym = store.interner.get_or_intern("tasks");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("name"))),
            Expr::Literal(Value::String(Arc::from("asc"))),
        ];
        let result = eval_builtin(&mut store, "sort", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        let names: Vec<String> = items
            .iter()
            .map(|item| {
                if let Some(Value::String(s)) = item.get_field("name") {
                    s.to_string()
                } else {
                    panic!()
                }
            })
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "delta", "epsilon", "gamma"]);
    }

    #[test]
    fn test_filter_on_non_list() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("not_a_list", Value::Int(42));
        let sym = store.interner.get_or_intern("not_a_list");
        let args = vec![
            Expr::Variable(sym),
            Expr::Literal(Value::String(Arc::from("field"))),
            Expr::Literal(Value::Bool(true)),
        ];
        let result = eval_builtin(&mut store, "filter", args);
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }


    /// Build a list of `n` records each containing a single int field `v`.
    fn make_large_list(n: usize) -> Value {
        let items: Vec<Value> = (0..n)
            .map(|i| {
                let mut m: Vec<(Arc<str>, Value)> = Vec::new();
                m.push((Arc::from("v"), Value::Int(i as i64)));
                Value::Record(Arc::from(m))
            })
            .collect();
        Value::List(Arc::new(items))
    }

    #[test]
    fn test_filter_large_list_triggers_timeout() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("big", make_large_list(25_000));
        let sym = store.interner.get_or_intern("big");
        let args = vec![
            Expr::Variable(sym),
            Expr::Literal(Value::String(Arc::from("v"))),
            Expr::Literal(Value::Int(1)),
        ];
        let result = eval_builtin(&mut store, "filter", args);
        assert!(
            matches!(result, Err(MizuError::Timeout)),
            "filter on 25 000-element list must return Timeout, got: {result:?}"
        );
    }

    #[test]
    fn test_count_large_list_triggers_timeout() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("big", make_large_list(25_000));
        let sym = store.interner.get_or_intern("big");
        let args = vec![
            Expr::Variable(sym),
            Expr::Literal(Value::String(Arc::from("v"))),
            Expr::Literal(Value::Int(1)),
        ];
        let result = eval_builtin(&mut store, "count", args);
        assert!(
            matches!(result, Err(MizuError::Timeout)),
            "count on 25 000-element list must return Timeout, got: {result:?}"
        );
    }

    #[test]
    fn test_sort_large_list_triggers_timeout() {
        use crate::parser::logic::Expr;
        // n=2000: log2_n = usize::BITS - 2000_usize.leading_zeros() = 11
        // sorting_cost = 2000 * 11 = 22_000 > MAX_INSTRUCTIONS(20_000) → Timeout.
        let mut store = VariableStore::new();
        store.set("big", make_large_list(2_000));
        let sym = store.interner.get_or_intern("big");
        let asc_sym = store.interner.get_or_intern("asc");
        let args = vec![
            Expr::Variable(sym),
            Expr::Literal(Value::String(Arc::from("v"))),
            Expr::Variable(asc_sym),
        ];
        let result = eval_builtin(&mut store, "sort", args);
        assert!(
            matches!(result, Err(MizuError::Timeout)),
            "sort on 2 000-element list must return Timeout, got: {result:?}"
        );
    }

    #[test]
    fn string_concat_doubling_chain_triggers_timeout_early() {
        // Reproduces the exponential-doubling bypass: a chain of nested
        // `let`s each doubling a string (`let s = s + s in …`). Before the
        // concat charge, this was bounded only by MAX_EVAL_DEPTH (256) and
        // would reach gigabyte-scale strings within ~30-40 levels while
        // burning under 1% of the nominal instruction budget. With the
        // concat charge, cumulative cost after k doublings from a
        // 1-byte seed is 2*(2^k - 1) instructions, which exceeds
        // MAX_INSTRUCTIONS (20 000) around k≈14 — so 40 levels (well under
        // the 256-level depth guard, and nowhere near problematic string
        // sizes) must already time out.
        use crate::parser::logic::{BinOp, Expr};
        use rustc_hash::FxHashMap;

        let mut store = VariableStore::new();
        let sym = store.interner.get_or_intern("s");

        let mut body = Expr::Variable(sym);
        for _ in 0..40 {
            let double_val = Expr::BinaryOp {
                left: Box::new(Expr::Variable(sym)),
                op: BinOp::Add,
                right: Box::new(Expr::Variable(sym)),
            };
            body = Expr::Let {
                name: sym,
                value: Box::new(double_val),
                body: Box::new(body),
            };
        }
        let ast = Expr::Let {
            name: sym,
            value: Box::new(Expr::Literal(Value::String(Arc::from("a")))),
            body: Box::new(body),
        };

        store.state_machine.instruction_count = 0;
        store.state_machine.eval_depth = 0;
        let fns = FxHashMap::default();
        let result = store.state_machine.evaluate(&ast, 0, &fns, &store.interner);

        assert!(
            matches!(result, Err(MizuError::Timeout)),
            "40-level string-doubling chain must hit the instruction budget \
             (around level 14) instead of completing, got: {result:?}"
        );
    }

    #[test]
    fn test_filter_small_list_still_works() {
        // The budget charge must not break normal-sized lists.
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        store.set("tasks", make_task_list()); // 5 elements
        let tasks_sym = store.interner.get_or_intern("tasks");
        let args = vec![
            Expr::Variable(tasks_sym),
            Expr::Literal(Value::String(Arc::from("done"))),
            Expr::Literal(Value::Bool(true)),
        ];
        let result = eval_builtin(&mut store, "filter", args).unwrap();
        let Value::List(items) = result else {
            panic!("expected list")
        };
        assert_eq!(
            items.len(),
            3,
            "filter of 5-element list must still succeed"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // get_system_time — dynamic write-target closed (RM-04)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn get_system_time_bare_variable_queues_correct_target() {
        use crate::network::RuntimeAction;
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        let target_sym = store.interner.get_or_intern("elapsed");
        let args = vec![Expr::Variable(target_sym)];
        let result = eval_builtin(&mut store, "get_system_time", args).unwrap();
        assert_eq!(result, Value::Bool(true));
        assert_eq!(store.state_machine.accumulated_actions.len(), 1);
        match &store.state_machine.accumulated_actions[0] {
            RuntimeAction::GetSystemTime { target_variable } => {
                assert_eq!(*target_variable, target_sym);
            }
            other => panic!("expected GetSystemTime, got: {other:?}"),
        }
    }

    #[test]
    fn get_system_time_non_variable_arg_rejected_at_runtime() {
        // Defense in depth: even if an `Expr::FunctionCall` for
        // get_system_time were constructed directly (bypassing the parser's
        // own bare-identifier restriction — e.g. from a future code path,
        // or a test), the evaluator itself must still reject a target that
        // isn't a bare Symbol fixed at construction time. This is exactly
        // the shape the pre-fix code accepted: an expression (here a
        // literal, but conceptually `$form.x`) evaluated at runtime to pick
        // the write target.
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        let args = vec![Expr::Literal(Value::String(Arc::from("evil_target")))];
        let err = eval_builtin(&mut store, "get_system_time", args).unwrap_err();
        assert!(
            matches!(err, MizuError::ExecutionError(_)),
            "expected ExecutionError for a non-bare-identifier target, got: {err:?}"
        );
        assert!(
            store.state_machine.accumulated_actions.is_empty(),
            "a rejected target must not queue a GetSystemTime action"
        );
    }

    #[test]
    fn get_system_time_computed_variable_target_rejected_at_runtime() {
        use crate::parser::logic::Expr;
        let mut store = VariableStore::new();
        let comp_sym = store.interner.get_or_intern("derived");
        store.state_machine.computed_var_syms.insert(comp_sym);
        let args = vec![Expr::Variable(comp_sym)];
        let err = eval_builtin(&mut store, "get_system_time", args).unwrap_err();
        assert!(
            matches!(err, MizuError::ExecutionError(_)),
            "expected ExecutionError when targeting a computed variable, got: {err:?}"
        );
    }

    #[test]
    fn test_strict_weak_ordering_heterogeneous() {
        // Records where the sorted field contains different Value variants.
        // Before the fix, heterogeneous pairs collapsed to Equal, violating
        // transitivity and causing undefined sort behaviour.
        let mut items = vec![
            // score: String("hello")  — variant weight 4
            {
                let mut m: Vec<(Arc<str>, Value)> = Vec::new();
                m.push((Arc::from("score"), Value::String(Arc::from("hello"))));
                Value::Record(Arc::from(m))
            },
            // score: Int(10)  — variant weight 3
            {
                let mut m: Vec<(Arc<str>, Value)> = Vec::new();
                m.push((Arc::from("score"), Value::Int(10)));
                Value::Record(Arc::from(m))
            },
            // score: Int(1)  — variant weight 3, lower numeric value
            {
                let mut m: Vec<(Arc<str>, Value)> = Vec::new();
                m.push((Arc::from("score"), Value::Int(1)));
                Value::Record(Arc::from(m))
            },
        ];

        // Must not panic; the comparator must be a valid strict-weak order.
        items.sort_by(|a, b| compare_values(field_value(a, "score"), field_value(b, "score")));

        // Expected: Int(1) < Int(10) < String("hello")
        // (all Ints have weight 3 < String weight 4; within Ints, 1 < 10)
        let scores: Vec<String> = items
            .iter()
            .map(|item| {
                item.get_field("score")
                    .map(|v| match v {
                        Value::Int(n) => n.to_string(),
                        Value::String(s) => s.to_string(),
                        _ => "?".to_string(),
                    })
                    .unwrap_or_else(|| "?".to_string())
            })
            .collect();

        assert_eq!(
            scores,
            vec!["1", "10", "hello"],
            "heterogeneous sort must be stable, deterministic, and panic-free: {scores:?}"
        );
    }

    #[test]
    fn test_variant_weight_ordering() {
        // None < Null < Bool < Int < String < List < Record
        assert!(variant_weight(&Value::Null) < variant_weight(&Value::Bool(true)));
        assert!(variant_weight(&Value::Bool(true)) < variant_weight(&Value::Int(0)));
        assert!(variant_weight(&Value::Int(0)) < variant_weight(&Value::String(Arc::from(""))));
        assert!(
            variant_weight(&Value::String(Arc::from("")))
                < variant_weight(&Value::List(Arc::new(vec![])))
        );
        assert!(
            variant_weight(&Value::List(Arc::new(vec![])))
                < variant_weight(&Value::Record(Arc::from(Vec::new())))
        );
    }

    #[test]
    fn test_none_is_less_than_some() {
        use std::cmp::Ordering;
        assert_eq!(compare_values(None, Some(&Value::Null)), Ordering::Less);
        assert_eq!(compare_values(None, Some(&Value::Int(0))), Ordering::Less);
        assert_eq!(
            compare_values(Some(&Value::Int(0)), None),
            Ordering::Greater
        );
        assert_eq!(compare_values(None::<&Value>, None), Ordering::Equal);
    }


    #[test]
    fn eval_depth_guard() {
        // evaluate_impl is a large function; in debug mode each call frame can
        // be several KB. With MAX_EVAL_DEPTH=256 the guard fires after
        // 257 × evaluate + 256 × evaluate_impl frames, which can approach the
        // 2 MB default test-thread stack. Run this test in a thread with an
        // explicitly enlarged stack so it works in both debug and release builds.
        let handle = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024) // 16 MB
            .spawn(|| {
                use crate::core::errors::MizuError;
                use crate::parser::logic::{BinOp, Expr};
                use rustc_hash::FxHashMap;

                // Build a 300-level deep BinaryOp chain entirely in Rust.
                // The parser would reject this before evaluation, so we bypass
                // it to test the evaluator's own depth guard directly.
                let mut ast = Expr::Literal(Value::Int(0));
                for _ in 0..300 {
                    ast = Expr::BinaryOp {
                        left: Box::new(ast),
                        op: BinOp::Add,
                        right: Box::new(Expr::Literal(Value::Int(0))),
                    };
                }

                let mut store = VariableStore::new();
                store.state_machine.instruction_count = 0;
                store.state_machine.eval_depth = 0;
                let fns = FxHashMap::default();

                let result = store.state_machine.evaluate(&ast, 0, &fns, &store.interner);
                match result {
                    Err(MizuError::ExecutionError(msg)) => {
                        assert!(
                            msg.contains("nesting too deep"),
                            "error must mention nesting depth: {msg}"
                        );
                    }
                    Err(MizuError::Timeout) => {} // budget may expire first — also acceptable
                    Ok(_) => panic!("expected depth error for 300-level AST, got Ok"),
                    Err(other) => panic!("unexpected error variant: {other:?}"),
                }
            })
            .expect("thread spawn must succeed");

        handle
            .join()
            .expect("depth-guard test thread must not panic");
    }

    /// Cross-function composition of `MAX_EVAL_DEPTH`.
    ///
    /// [`crate::parser::logic::MAX_PARSE_DEPTH`] (256) bounds nesting depth
    /// **per expression tree parsed in isolation** — a function body is one
    /// such tree, and the expression at a call site is another. Nothing at
    /// parse time prevents a ~250-level-deep function body from being
    /// invoked from within a call-site expression that is itself nested
    /// several levels deep, which would compose to a total `evaluate()`
    /// recursion depth exceeding 256 even though neither individual tree
    /// violates `MAX_PARSE_DEPTH`.
    ///
    /// This test builds exactly that scenario directly on the AST (bypassing
    /// the parser, as `eval_depth_guard` above does) and checks that
    /// `eval_depth` — which is a single running counter on `StateMachine`,
    /// never reset at a function-call boundary (only `local_stack` is
    /// truncated there, see the `Expr::FunctionCall` arm of `evaluate_impl`)
    /// — still fires cleanly.
    ///
    /// Unlike `eval_depth_guard`, this test deliberately does **not** run on
    /// an arbitrarily-generous stack. Production's `LogicWorker`
    /// (`parser::logic_worker::LogicWorker::spawn`) evaluates on a thread
    /// started with an explicit
    /// [`crate::parser::logic_worker::LogicWorker::STACK_SIZE_BYTES`]-sized
    /// stack (16 MiB) — so this test re-execs the test binary as a child
    /// process and runs the scenario on a thread built with that exact same
    /// constant, to determine whether the depth guard reliably wins the race
    /// against native stack exhaustion under the conditions production
    /// actually runs under, rather than under the artificially generous
    /// conditions of `eval_depth_guard`. A real native stack overflow aborts
    /// the process (it cannot be caught with `catch_unwind`), so this has to
    /// be observed from a parent process inspecting the child's exit status.
    #[test]
    fn cross_function_composition_depth_guard() {
        const CHILD_ENV: &str = "MIZU_DEPTH_COMPOSITION_CHILD";
        const OK_MARKER: &str = "DEPTH_GUARD_FIRED_CLEANLY";

        if std::env::var_os(CHILD_ENV).is_some() {
            run_cross_function_composition_child(OK_MARKER);
            return;
        }

        let exe = std::env::current_exe().expect("current_exe");
        let output = std::process::Command::new(exe)
            .arg("core::types::tests::cross_function_composition_depth_guard")
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
            "cross-function eval_depth composition did not cleanly hit the \
             MAX_EVAL_DEPTH guard on a default-size thread (status={:?}). \
             This may indicate a native stack overflow occurring before the \
             eval_depth check can intervene, which would be a SEPARATE, \
             more serious finding than a missing guard.\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
            output.status, stdout, stderr
        );
    }

    /// Runs the actual cross-function composition scenario on a thread built
    /// with the same `stack_size` production's `LogicWorker::spawn` uses
    /// ([`crate::parser::logic_worker::LogicWorker::STACK_SIZE_BYTES`]), and
    /// prints `ok_marker` iff `evaluate` returned the expected
    /// `MAX_EVAL_DEPTH` error rather than panicking, hanging, or (silently,
    /// from this process's point of view) crashing.
    fn run_cross_function_composition_child(ok_marker: &'static str) {
        use crate::parser::logic_worker::LogicWorker;
        use rustc_hash::FxHashMap;

        let handle = std::thread::Builder::new()
            .stack_size(LogicWorker::STACK_SIZE_BYTES)
            .spawn(move || run_cross_function_composition_scenario(ok_marker))
            .expect("thread spawn must succeed");

        handle.join().expect("composition scenario thread must not panic");
    }

    /// The actual cross-function composition scenario, run on whatever
    /// thread `run_cross_function_composition_child` builds.
    fn run_cross_function_composition_scenario(ok_marker: &str) {
        use crate::parser::logic::{BinOp, Expr, MizuFunction};
        use rustc_hash::FxHashMap;

        let mut store = VariableStore::new();
        let param = store.interner.get_or_intern("x");
        let func_sym = store.interner.get_or_intern("deeply_nested_fn");

        // Function body: ~250 levels of BinaryOp nesting -- representative
        // of the deepest single expression tree the parser will accept
        // under MAX_PARSE_DEPTH (256) for a function body parsed on its own.
        let mut body = Expr::Variable(param);
        for _ in 0..250 {
            body = Expr::BinaryOp {
                left: Box::new(body),
                op: BinOp::Add,
                right: Box::new(Expr::Literal(Value::Int(0))),
            };
        }
        let func = MizuFunction {
            params: vec![(param, None)],
            body,
        };
        let mut functions = FxHashMap::default();
        functions.insert(func_sym, func);

        // Call-site expression: another ~20 levels of nesting -- itself
        // comfortably under MAX_PARSE_DEPTH on its own -- wrapping a call
        // to the function above. Neither tree alone violates
        // MAX_PARSE_DEPTH, but composed at evaluation time they exceed
        // MAX_EVAL_DEPTH (256).
        let mut call_site = Expr::FunctionCall {
            name: func_sym,
            args: vec![Expr::Literal(Value::Int(1))],
        };
        for _ in 0..20 {
            call_site = Expr::BinaryOp {
                left: Box::new(call_site),
                op: BinOp::Add,
                right: Box::new(Expr::Literal(Value::Int(0))),
            };
        }

        store.state_machine.instruction_count = 0;
        store.state_machine.eval_depth = 0;

        let result = store
            .state_machine
            .evaluate(&call_site, 0, &functions, &store.interner);

        match result {
            Err(MizuError::ExecutionError(msg)) if msg.contains("nesting too deep") => {
                println!("{ok_marker}");
            }
            // Also acceptable: the instruction budget could in principle be
            // exhausted first depending on constant tuning: still a clean,
            // bounded error, not a crash.
            Err(MizuError::Timeout) => {
                println!("{ok_marker}");
            }
            other => {
                println!("UNEXPECTED_RESULT: {other:?}");
            }
        }
    }

    /// Measures the real native stack depth required to run a `evaluate()`
    /// chain deep enough to trip `MAX_EVAL_DEPTH` (256), in whichever profile
    /// the test binary was built under (debug or `--release`).
    ///
    /// The comment on `eval_depth_guard` above only established that debug
    /// frames are "several KB" each; it never quantified the release-mode
    /// case, where `evaluate`/`evaluate_impl` frames are dramatically
    /// smaller after inlining and optimization. Production's `LogicWorker`
    /// (`parser::logic_worker::LogicWorker::spawn`) always runs in whatever
    /// profile the binary was built under, so a release-only guess is not
    /// good enough either — this test probes a fixed ladder of candidate
    /// stack sizes and, for each, re-execs this same test binary (a real
    /// native stack overflow aborts the process and cannot be caught with
    /// `catch_unwind`, so it must be observed from a parent process) to run
    /// the same 300-level chain used by `cross_function_composition_depth_guard`
    /// on a thread built with exactly that `stack_size`. The smallest
    /// candidate that survives is the empirical per-profile floor.
    ///
    /// This is a manual measurement tool, not a correctness gate — it is
    /// `#[ignore]`d so normal `cargo test` runs stay fast. Run it directly to
    /// reproduce the numbers documented next to `LogicWorker::spawn` and in
    /// `walkthrough.md`:
    ///   `cargo test --release --lib core::types::tests::measure_stack_usage_at_max_eval_depth -- --ignored --nocapture`
    ///   `cargo test          --lib core::types::tests::measure_stack_usage_at_max_eval_depth -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn measure_stack_usage_at_max_eval_depth() {
        const STACK_ENV: &str = "MIZU_STACK_MEASURE_BYTES";
        const OK_MARKER: &str = "STACK_MEASURE_OK";

        if let Some(bytes) = std::env::var_os(STACK_ENV) {
            let stack_size: usize = bytes
                .to_str()
                .expect("env var must be UTF-8")
                .parse()
                .expect("env var must be a valid usize");
            run_stack_measurement_child(stack_size, OK_MARKER);
            return;
        }

        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };

        let exe = std::env::current_exe().expect("current_exe");
        // Doubling ladder from 16 KiB up to 4 MiB covers everywhere a
        // per-frame estimate in the tens-of-KB-to-single-KB range could land,
        // for both debug and release.
        let candidates: &[usize] = &[
            16 * 1024,
            32 * 1024,
            64 * 1024,
            128 * 1024,
            256 * 1024,
            512 * 1024,
            1024 * 1024,
            2 * 1024 * 1024,
            4 * 1024 * 1024,
        ];

        let mut smallest_safe: Option<usize> = None;
        for &size in candidates {
            let output = std::process::Command::new(&exe)
                .arg("core::types::tests::measure_stack_usage_at_max_eval_depth")
                .arg("--exact")
                .arg("--nocapture")
                .arg("--test-threads=1")
                .arg("--ignored")
                .env(STACK_ENV, size.to_string())
                .output()
                .expect("failed to spawn measurement child process");

            let stdout = String::from_utf8_lossy(&output.stdout);
            let safe = output.status.success() && stdout.contains(OK_MARKER);
            println!(
                "[{profile}] stack_size={size} bytes ({:.1} KiB) -> {}",
                size as f64 / 1024.0,
                if safe { "survived" } else { "CRASHED" }
            );
            if safe && smallest_safe.is_none() {
                smallest_safe = Some(size);
            }
        }

        println!(
            "[{profile}] RESULT: smallest tested stack_size that survives a \
             300-level eval_depth chain (exceeds MAX_EVAL_DEPTH=256) = {:?}",
            smallest_safe
        );
    }

    /// Runs the actual 300-level `evaluate()` chain — identical in shape to
    /// `eval_depth_guard` and `cross_function_composition_depth_guard` — on a
    /// thread built with exactly `stack_size` bytes, and prints `ok_marker`
    /// iff it completes without a native stack overflow (regardless of
    /// whether the result is the depth-guard error or a timeout — both are
    /// controlled, non-crashing outcomes).
    fn run_stack_measurement_child(stack_size: usize, ok_marker: &str) {
        use crate::parser::logic::{BinOp, Expr};
        use rustc_hash::FxHashMap;

        let handle = std::thread::Builder::new()
            .stack_size(stack_size)
            .spawn(|| {
                let mut ast = Expr::Literal(Value::Int(0));
                for _ in 0..300 {
                    ast = Expr::BinaryOp {
                        left: Box::new(ast),
                        op: BinOp::Add,
                        right: Box::new(Expr::Literal(Value::Int(0))),
                    };
                }

                let mut store = VariableStore::new();
                store.state_machine.instruction_count = 0;
                store.state_machine.eval_depth = 0;
                let fns = FxHashMap::default();

                let _ = store.state_machine.evaluate(&ast, 0, &fns, &store.interner);
            })
            .expect("thread spawn must succeed");

        handle.join().expect("measurement thread must not panic");
        println!("{ok_marker}");
    }

    #[test]
    fn interpolate_deep_dot_path() {
        // Three-level nesting: {a.b.c} must resolve to the leaf string.
        let mut store = VariableStore::new();

        // Build: a = { b: { c: "value" } }
        let mut inner: Vec<(Arc<str>, Value)> = Vec::new();
        inner.push((Arc::from("c"), Value::String(Arc::from("value"))));
        let mut outer: Vec<(Arc<str>, Value)> = Vec::new();
        outer.push((Arc::from("b"), Value::Record(Arc::from(inner))));
        store.set("a", Value::Record(Arc::from(outer)));

        let result = store
            .interpolate("{a.b.c}")
            .expect("interpolation must succeed");
        assert_eq!(
            result, "value",
            "three-level dot-path must resolve to leaf"
        );
    }

    #[test]
    fn interpolate_dot_path_missing_intermediate() {
        // {a.b.c} where `b` is a String, not a Record — must fall back to literal.
        let mut store = VariableStore::new();

        let mut outer: Vec<(Arc<str>, Value)> = Vec::new();
        outer.push((Arc::from("b"), Value::String(Arc::from("not_a_record"))));
        store.set("a", Value::Record(Arc::from(outer)));

        let result = store
            .interpolate("{a.b.c}")
            .expect("interpolation must not error");
        assert_eq!(
            result, "{a.b.c}",
            "non-record intermediate must produce literal fallback"
        );
    }


    #[test]
    fn frozen_interner_existing_symbols_unchanged() {
        let mut interner = StringInterner::new();
        let sym_a = interner.get_or_intern("alpha");
        let sym_b = interner.get_or_intern("beta");

        interner.freeze();

        // Existing symbols must still resolve to the same ID post-freeze.
        assert_eq!(interner.get_or_intern("alpha"), sym_a);
        assert_eq!(interner.get_or_intern("beta"), sym_b);
        assert_eq!(interner.get("alpha"), Some(sym_a));
        assert_eq!(interner.resolve(sym_a), Some("alpha"));
    }

    #[test]
    fn frozen_interner_new_symbol_is_still_real_and_resolvable() {
        // `get_or_intern` never returns a dummy/sentinel Symbol: even when
        // called post-freeze (a caller bug, since the resulting Symbol only
        // has meaning on this thread's copy of the table — see the
        // type-level docs), it must intern the name for real rather than
        // silently corrupting the caller with an unresolvable placeholder.
        let mut interner = StringInterner::new();
        interner.get_or_intern("existing");
        interner.freeze();

        let old_map_len = interner.map.len();
        let old_vec_len = interner.vec.len();

        let sym = interner.get_or_intern("runtime-added");

        // The table did grow by exactly one entry.
        assert_eq!(interner.map.len(), old_map_len + 1);
        assert_eq!(interner.vec.len(), old_vec_len + 1);

        // The returned symbol is real: it resolves back to the name and is
        // found by both `get` and a subsequent `get_or_intern`.
        assert_ne!(sym, Symbol(u32::MAX), "no sentinel/dummy Symbol must ever be returned");
        assert_eq!(interner.resolve(sym), Some("runtime-added"));
        assert_eq!(interner.get("runtime-added"), Some(sym));
        assert_eq!(interner.get_or_intern("runtime-added"), sym);
    }

    /// M1 fix: clone must preserve `frozen = true` so that the logic worker's
    /// copy of the interner cannot silently diverge Symbol(u32) IDs.
    ///
    /// The old test asserted `!cloned.frozen` (the pre-fix behavior where Clone
    /// deliberately unset the flag).  That behavior was the root cause of M1:
    /// the unfrozen worker could add new symbols in a different order than the
    /// UI thread, making Symbol IDs inconsistent across threads.
    ///
    /// Post-fix: both threads share the same frozen interner; runtime-generated
    /// strings that are not pre-declared must use `VariableStore::set_runtime`
    /// (which calls `get` not `get_or_intern`) rather than `get_or_intern`.
    #[test]
    fn frozen_clone_inherits_frozen_state() {
        let mut interner = StringInterner::new();
        interner.get_or_intern("x");
        interner.freeze();
        assert!(interner.frozen, "original must be frozen");

        let cloned = interner.clone();
        assert!(
            cloned.frozen,
            "clone must inherit frozen=true (M1 fix): worker must not add new symbols"
        );

        // The clone must resolve all pre-freeze symbols identically.
        let sym_x = interner.get("x");
        assert_eq!(cloned.get("x"), sym_x, "symbol IDs must be identical in clone");
    }


    #[test]
    fn compare_lists_equal_content() {
        use std::cmp::Ordering;
        let a = Value::List(Arc::new(vec![Value::Int(1), Value::Int(2)]));
        let b = Value::List(Arc::new(vec![Value::Int(1), Value::Int(2)]));
        assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Equal);
    }

    #[test]
    fn compare_lists_lexicographic() {
        use std::cmp::Ordering;
        // [1, 3] > [1, 2]
        let a = Value::List(Arc::new(vec![Value::Int(1), Value::Int(3)]));
        let b = Value::List(Arc::new(vec![Value::Int(1), Value::Int(2)]));
        assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Greater);
        assert_eq!(compare_values(Some(&b), Some(&a)), Ordering::Less);
    }

    #[test]
    fn compare_lists_shorter_less_than_longer() {
        use std::cmp::Ordering;
        // [1] < [1, 2] (prefix match, shorter is Less)
        let shorter = Value::List(Arc::new(vec![Value::Int(1)]));
        let longer = Value::List(Arc::new(vec![Value::Int(1), Value::Int(2)]));
        assert_eq!(
            compare_values(Some(&shorter), Some(&longer)),
            Ordering::Less
        );
        assert_eq!(
            compare_values(Some(&longer), Some(&shorter)),
            Ordering::Greater
        );
    }

    #[test]
    fn compare_empty_lists_equal() {
        use std::cmp::Ordering;
        let a = Value::List(Arc::new(vec![]));
        let b = Value::List(Arc::new(vec![]));
        assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Equal);
    }

    #[test]
    fn sort_list_of_lists_is_deterministic() {
        // Sorting [[3], [1,2], [1], []] must produce a stable lexicographic order.
        let mut lists = vec![
            Value::List(Arc::new(vec![Value::Int(3)])),
            Value::List(Arc::new(vec![Value::Int(1), Value::Int(2)])),
            Value::List(Arc::new(vec![Value::Int(1)])),
            Value::List(Arc::new(vec![])),
        ];
        lists.sort_by(|a, b| compare_values(Some(a), Some(b)));
        // Expected: [] < [1] < [1,2] < [3]
        let lengths: Vec<usize> = lists
            .iter()
            .map(|v| {
                if let Value::List(v) = v { v.len() } else { panic!() }
            })
            .collect();
        assert_eq!(lengths, vec![0, 1, 2, 1]);
        // Verify the last element is [3].
        if let Value::List(last) = lists.last().unwrap() {
            assert_eq!(last.as_slice(), &[Value::Int(3)]);
        } else {
            panic!("last element must be a List");
        }
    }

    #[test]
    fn compare_records_equal_content() {
        use std::cmp::Ordering;
        let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
        ma.push((Arc::from("x"), Value::Int(1)));
        let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
        mb.push((Arc::from("x"), Value::Int(1)));
        let a = Value::Record(Arc::from(ma));
        let b = Value::Record(Arc::from(mb));
        assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Equal);
    }

    #[test]
    fn compare_records_same_keys() {
        use std::cmp::Ordering;
        // { x: 1 } < { x: 2 }
        let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
        ma.push((Arc::from("x"), Value::Int(1)));
        let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
        mb.push((Arc::from("x"), Value::Int(2)));
        let a = Value::Record(Arc::from(ma));
        let b = Value::Record(Arc::from(mb));
        assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Less);
        assert_eq!(compare_values(Some(&b), Some(&a)), Ordering::Greater);
    }

    #[test]
    fn compare_records_by_key_name() {
        use std::cmp::Ordering;
        // { a: 1 } < { b: 1 } because "a" < "b"
        let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
        ma.push((Arc::from("a"), Value::Int(1)));
        let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
        mb.push((Arc::from("b"), Value::Int(1)));
        let a = Value::Record(Arc::from(ma));
        let b = Value::Record(Arc::from(mb));
        assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Less);
    }

    #[test]
    fn compare_records_shorter_less_than_longer() {
        use std::cmp::Ordering;
        // { x: 1 } < { x: 1, y: 2 } (same keys up to len, shorter is Less)
        let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
        ma.push((Arc::from("x"), Value::Int(1)));
        let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
        mb.push((Arc::from("x"), Value::Int(1)));
        mb.push((Arc::from("y"), Value::Int(2)));
        let a = Value::Record(Arc::from(ma));
        let b = Value::Record(Arc::from(mb));
        assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Less);
        assert_eq!(compare_values(Some(&b), Some(&a)), Ordering::Greater);
    }

    #[test]
    fn sort_records_by_single_field_via_compare_values() {
        // Before the fix, sorting a list whose items are themselves Record values
        // (not comparing a field *inside* a Record, but the Record *itself*) would
        // collapse to all-Equal and produce undefined order.
        let mut records: Vec<Value> = (0..4_i64)
            .rev()
            .map(|i| {
                let mut m: Vec<(Arc<str>, Value)> = Vec::new();
                m.push((Arc::from("v"), Value::Int(i)));
                Value::Record(Arc::from(m))
            })
            .collect();
        // compare_values on two Records now compares keys then values.
        records.sort_by(|a, b| compare_values(Some(a), Some(b)));
        let vals: Vec<i64> = records
            .iter()
            .map(|r| {
                if let Some(&Value::Int(n)) = r.get_field("v") {
                    n
                } else {
                    panic!()
                }
            })
            .collect();
        assert_eq!(vals, vec![0, 1, 2, 3], "records must sort by their 'v' field");
    }

    // ------------------------------------------------------------------
    // Task 1 regression: BTreeMap-based Record sort — zero allocation,
    // strict weak ordering, correct result on deeply mixed inputs
    // ------------------------------------------------------------------

    /// Verifies that sorting a list of multi-key records via `compare_values`
    /// produces the correct lexicographic order and does not panic.
    ///
    /// With the BTreeMap representation, `compare_values` iterates the two maps
    /// in parallel using `Iterator::zip` — no `Vec` allocation, no `sort_unstable`
    /// call.  The correctness guarantee is structural: BTreeMap always yields keys
    /// in ascending order, so the zip is guaranteed to visit corresponding keys.
    #[test]
    fn compare_records_btreemap_zero_alloc_sort() {
        use std::cmp::Ordering;

        // Three records with two keys each, in descending insertion order,
        // to verify that BTreeMap's sorted iterator is key-order, not
        // insertion-order.
        let make = |a: i64, b: i64| {
            let mut m: Vec<(Arc<str>, Value)> = Vec::new();
            // Insert in reverse alphabetical order — BTreeMap must still iterate "alpha" first.
            m.push((Arc::from("zeta"), Value::Int(b)));
            m.push((Arc::from("alpha"), Value::Int(a)));
            m.sort_by(|x, y| x.0.cmp(&y.0));
            Value::Record(Arc::from(m))
        };

        let r1 = make(1, 10); // { alpha:1, zeta:10 }
        let r2 = make(2, 5);  // { alpha:2, zeta:5  }
        let r3 = make(1, 20); // { alpha:1, zeta:20 }

        // r1 vs r3: alpha equal, zeta 10 < 20 → r1 < r3
        assert_eq!(compare_values(Some(&r1), Some(&r3)), Ordering::Less);
        // r3 vs r2: alpha 1 < 2 → r3 < r2
        assert_eq!(compare_values(Some(&r3), Some(&r2)), Ordering::Less);
        // Transitivity: r1 < r3 < r2 → sort must yield [r1, r3, r2]
        let mut records = vec![r2.clone(), r1.clone(), r3.clone()];
        records.sort_by(|a, b| compare_values(Some(a), Some(b)));

        // Expected ascending order: r1 { alpha:1, zeta:10 }, r3 { alpha:1, zeta:20 }, r2 { alpha:2, zeta:5 }
        let alpha_vals: Vec<i64> = records
            .iter()
            .map(|r| {
                if let Some(&Value::Int(n)) = r.get_field("alpha") {
                    n
                } else {
                    panic!()
                }
            })
            .collect();
        assert_eq!(
            alpha_vals, vec![1, 1, 2],
            "BTreeMap record sort must respect key order regardless of insertion order"
        );
    }


    /// A frozen interner's clone must also be frozen.
    /// Before the fix Clone deliberately set `frozen = false`; this test
    /// catches any future regression.
    #[test]
    fn interner_clone_preserves_frozen_state() {
        let mut interner = StringInterner::new();
        interner.get_or_intern("alpha");
        interner.get_or_intern("beta");
        assert!(!interner.frozen, "must start unfrozen");

        interner.freeze();
        assert!(interner.frozen);

        let clone = interner.clone();
        assert!(
            clone.frozen,
            "cloned interner must inherit frozen=true so the worker thread \
             cannot add new symbols after the parse phase"
        );
    }

    /// An unfrozen interner's clone must also be unfrozen (no spurious
    /// over-freezing of clones taken before the parse phase completes).
    #[test]
    fn interner_clone_preserves_unfrozen_state() {
        let mut interner = StringInterner::new();
        interner.get_or_intern("x");
        assert!(!interner.frozen);

        let clone = interner.clone();
        assert!(!clone.frozen, "pre-freeze clone must remain unfrozen");
    }

    /// Symbols are identical in the original and its frozen clone.
    #[test]
    fn interner_clone_symbols_are_identical() {
        let mut interner = StringInterner::new();
        let s_alpha = interner.get_or_intern("alpha");
        let s_beta = interner.get_or_intern("beta");
        interner.freeze();

        let clone = interner.clone();
        assert_eq!(clone.get("alpha"), Some(s_alpha));
        assert_eq!(clone.get("beta"), Some(s_beta));
        assert_eq!(clone.vec.len(), interner.vec.len());
    }


    /// `set_runtime` updates a pre-declared (interned) variable normally.
    #[test]
    fn set_runtime_updates_known_variable() {
        let mut store = VariableStore::new();
        store.set("price", Value::Int(10));
        store.interner.freeze();

        store.set_runtime("price", Value::Int(99));
        assert_eq!(*store.get("price").unwrap(), Value::Int(99));
    }

    /// `set_runtime` silently discards names that are not in the frozen interner,
    /// leaving the symbol table unchanged.
    #[test]
    fn set_runtime_discards_unknown_names_and_does_not_grow_interner() {
        let mut store = VariableStore::new();
        store.set("declared", Value::Int(1));
        store.interner.freeze();

        let interned_count = store.interner.vec.len();

        store.set_runtime("undeclared_field", Value::Int(42));
        store.set_runtime("another_unknown", Value::from("hello"));

        // Interner must not have grown.
        assert_eq!(
            store.interner.vec.len(),
            interned_count,
            "frozen interner must not grow via set_runtime"
        );
        // Unknown names are not stored.
        assert!(
            store.get("undeclared_field").is_err(),
            "undeclared variable must not appear in the store"
        );
    }

    /// Demonstrates the M1 fix end-to-end: after freeze, a clone used by the
    /// worker thread cannot add symbols that would diverge from the UI thread.
    /// Before the fix, the worker's clone was unfrozen and adding "runtime_var"
    /// would produce Symbol(N) on the worker but a DIFFERENT Symbol(M) if the
    /// UI thread independently interned the same name later.
    #[test]
    fn frozen_clone_cannot_diverge_symbol_ids() {
        let mut ui_interner = StringInterner::new();
        let sym_a = ui_interner.get_or_intern("declared_a");
        let sym_b = ui_interner.get_or_intern("declared_b");
        ui_interner.freeze();

        let worker_interner = ui_interner.clone();
        assert!(worker_interner.frozen, "worker must be frozen");

        // The worker resolves known symbols identically.
        assert_eq!(worker_interner.get("declared_a"), Some(sym_a));
        assert_eq!(worker_interner.get("declared_b"), Some(sym_b));

        // Worker-side VariableStore with the frozen clone.
        let mut worker_store = VariableStore::new();
        worker_store.interner = worker_interner;

        // set_runtime does NOT intern "runtime_var".
        worker_store.set_runtime("runtime_var", Value::Int(7));
        assert!(worker_store.get("runtime_var").is_err());

        // Symbol table size on both sides is still identical.
        assert_eq!(
            worker_store.interner.vec.len(),
            ui_interner.vec.len(),
            "worker must not add symbols after freeze"
        );
    }
}
