//! The `Value` enum and JSON (de)serialization.

use std::fmt;
use std::sync::Arc;

use crate::core::errors::MizuError;

use super::eval::MAX_EVAL_DEPTH;

/// The scale factor used for fixed-point arithmetic: 8 decimal digits of
/// precision. An `i64` holding a value scaled by this factor can represent
/// magnitudes up to `i64::MAX / DECIMAL_SCALE`, i.e. roughly
/// `±92,233,720,368.54775807`.
pub const DECIMAL_SCALE: i64 = 100_000_000;

/// Opaque handle to a user-selected local file, produced only by a
/// `type "file"` input's native picker dialog.
///
/// Deliberately holds only a path and a display name — never the file's
/// bytes. A `Value` is meant to be cheap to clone/compare/store (the same
/// spirit as the fixed-point `Value::Decimal` scale and the evaluator's bounded
/// recursion); loading a whole file into a `Value` the evaluator freely
/// passes through `filter`/`store_local`/comparisons would blow past that,
/// and would make raw file content reachable from ordinary logic in ways
/// nothing here is designed to gate. The file's bytes are read only by the
/// network worker, in bounded chunks, at the moment a `Multipart` request is
/// actually sent (see `network::worker::multipart`).
pub struct FileHandleData {
    /// Absolute filesystem path to the selected file.
    pub path: std::path::PathBuf,
    /// User-visible original filename (the last path component at selection
    /// time), used for the `Content-Disposition: filename=` part of a
    /// multipart upload. Never a path — see `network::worker::multipart`'s
    /// filename sanitisation for why even this is not trusted verbatim.
    pub filename: String,
}

impl std::fmt::Debug for FileHandleData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileHandleData")
            .field("filename", &self.filename)
            .field("path", &"<REDACTED FOR SECURITY>")
            .finish()
    }
}

/// One `key: value` pair of a [`Value::Record`], carrying a precomputed hash
/// of `key` so lookups compare a `u32` before ever touching the string bytes.
#[derive(Debug, Clone)]
pub struct RecordField {
    pub key: Arc<str>,
    /// [`hash_field`] of `key`, precomputed at construction.
    pub hash: u32,
    pub value: Value,
}

/// FNV-1a, 32-bit.
///
/// Deliberately *not* `FxHasher` (or any `DefaultHasher`): those mix at the
/// native word size, so the same key hashes differently on a 32-bit and a
/// 64-bit target. These hashes are baked into parsed field-access nodes
/// (`Expr::FieldAccess::field_hash`), so a word-size-dependent hash would make
/// the generated program representation architecture-dependent. Every
/// operation here is `u32` wrapping arithmetic — no `usize` anywhere — so the
/// result is identical on all targets.
#[inline]
pub fn hash_field(s: &str) -> u32 {
    const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;

    let mut hash = FNV_OFFSET_BASIS;
    for b in s.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
impl PartialEq for RecordField {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.key == other.key && self.value == other.value
    }
}

/// The set of all primitive values in the Mizu type system.
///
/// # Numerics
///
/// Two numeric variants, one surface type. [`Value::Int`] holds an exact
/// unscaled `i64`; [`Value::Decimal`] holds an `i64` scaled by
/// [`DECIMAL_SCALE`]. Both report `num` from `type_name`, and every operator
/// promotes `Int` to `Decimal` when the two meet, so the split is invisible
/// to a Mizu program except in the one way that matters: an integer keeps its
/// full `i64` range instead of being capped at `i64::MAX / DECIMAL_SCALE`.
///
/// The variant is chosen by the literal's *spelling* — `5` is `Int`, `5.0` is
/// `Decimal` — and JSON round-trips preserve that distinction exactly (see
/// [`to_json`] and `parse_json_number_exact`).
///
/// # Equality
///
/// `Value` has no production `PartialEq`. Structural comparison goes through
/// [`Value::budget_eq`], which is iterative and charged against a caller-owned
/// budget: two values can share `Arc` subtrees, so a naive recursive `==` on a
/// crafted DAG is both unbounded in stack depth and exponential in time
/// ("billion laughs"). The `cfg(test)` `PartialEq` below exists only so tests
/// can use `assert_eq!`, and it delegates to `budget_eq` so the test suite
/// exercises the same code production does.
///
/// # Depth is an invariant of the type, not of any one function
///
/// Dropping a `Value` is recursive and cannot be made otherwise from here:
/// freeing a `List` frees its `Vec<Value>`, whose elements free their own
/// children, all on the native stack. Nothing in this file can bound that —
/// only *not constructing* such a value can.
///
/// So every entry point that builds a `Value` from outside data bounds its
/// depth at [`MAX_EVAL_DEPTH`] (256): the JSON parser via `MAX_JSON_DEPTH`,
/// the evaluator via its own recursion limit, and the logic parser via
/// `MAX_PARSE_DEPTH`. A future entry point that skips that check does not
/// merely risk a slow comparison — it makes the value unfreeable. Measured in
/// a debug build, recursive drop costs roughly 400 bytes of stack per level,
/// so 256 levels is ~100 KB against a 2 MiB thread stack: comfortable, but
/// three orders of magnitude, not the "thousands of levels" that
/// [`MAX_EVAL_DEPTH`]'s own comment estimates for `evaluate`'s frames.
#[derive(Debug, Clone)]
pub enum Value {
    /// A null or empty value.
    Null,
    /// A boolean value (true or false).
    Bool(bool),
    /// An unscaled 64-bit integer.
    Int(i64),
    /// A scaled 64-bit integer representing a fixed-point decimal.
    Decimal(i64),
    /// A reference-counted string.
    String(Arc<str>),
    /// A reference-counted list of nested values.
    List(Arc<Vec<Value>>),
    /// A reference-counted record of key-value pairs, kept in strict
    /// lexicographic order of `key`.
    ///
    /// The ordering is load-bearing, not cosmetic: structural equality zips
    /// the two field slices pairwise, and [`to_json`] / [`fmt::Display`] emit
    /// fields in slice order. Sorting by anything else (a hash, say) makes
    /// equality depend on construction order and makes serialized output
    /// look pseudo-random. Build records through
    /// [`Value::record_from_unsorted`] rather than assembling the slice by
    /// hand, so the invariant is established in exactly one place.
    Record(Arc<[RecordField]>),
    /// An opaque handle to a locally-selected file (from a `type "file"`
    /// input). See [`FileHandleData`] for why this never carries file bytes.
    FileHandle(Arc<FileHandleData>),
}

/// Test-only `==`, delegating to the production [`Value::budget_eq`].
///
/// The budget is deliberately finite rather than `u64::MAX`: a test that
/// builds a structure large enough to exhaust it has built something no
/// production budget would ever admit, and should say so loudly instead of
/// quietly comparing for a second.
#[cfg(test)]
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        const TEST_EQ_BUDGET: u64 = 1_000_000;
        let mut budget = 0;
        match self.budget_eq(other, &mut budget, TEST_EQ_BUDGET) {
            Ok(verdict) => verdict,
            Err(_) => panic!(
                "Value `==` exceeded the {TEST_EQ_BUDGET}-node test budget; \
                 call `budget_eq` directly with an explicit budget"
            ),
        }
    }
}

impl Value {
    /// Builds a [`Value::Record`] from unordered key-value pairs, establishing
    /// the lexicographic ordering the variant requires and deduplicating keys.
    ///
    /// This is the only place records are ordered; every construction site
    /// goes through it so the invariant cannot drift between them.
    ///
    /// Duplicate keys are collapsed to the *first* occurrence in input order
    /// (`sort_by` is stable, and `dedup_by` drops the later element of an
    /// equal pair). A record with two `"a"` fields would otherwise have a
    /// layout — and therefore a serialization, and therefore a stored
    /// ciphertext — that depends on which duplicate `get_field` happened to
    /// reach first.
    pub fn record_from_unsorted<I, K>(pairs: I) -> Value
    where
        I: IntoIterator<Item = (K, Value)>,
        K: AsRef<str>,
    {
        let mut fields: Vec<RecordField> = pairs
            .into_iter()
            .map(|(key, value)| RecordField {
                hash: hash_field(key.as_ref()),
                key: Arc::from(key.as_ref()),
                value,
            })
            .collect();
        fields.sort_by(|a, b| a.key.cmp(&b.key));
        fields.dedup_by(|a, b| a.key == b.key);
        Value::Record(Arc::from(fields))
    }

    /// Safely retrieves the value associated with `field_name` if this value
    /// is a [`Value::Record`].
    ///
    /// A linear scan, not a binary search: the slice is ordered by key, not by
    /// hash, and records in practice hold a handful of fields, so a scan over
    /// a contiguous run of `u32`s beats the mispredicted branch chain of a
    /// binary search. The `hash` compare is the filter and the `key` compare
    /// is the decision, so a hash collision resolves correctly instead of
    /// needing a fallback pass — and an attacker who floods a record with
    /// colliding keys still only pays for the string compares their own
    /// collisions caused.
    pub fn get_field(&self, field_hash: u32, field_name: &str) -> Option<&Value> {
        match self {
            Value::Record(slice) => slice
                .iter()
                .find(|f| f.hash == field_hash && f.key.as_ref() == field_name)
                .map(|f| &f.value),
            _ => None,
        }
    }

    /// Structural equality, iterative and charged against a caller-owned
    /// budget.
    ///
    /// Two properties this has to provide that a derived `PartialEq` cannot:
    ///
    /// * **Bounded stack.** Comparison walks an explicit work stack rather
    ///   than the native one, so nesting depth costs heap, not frames. A
    ///   `Value` deep enough to overflow the stack is reachable from storage
    ///   rehydration and from the evaluator, both of which cap depth at
    ///   [`MAX_EVAL_DEPTH`] — but that cap is an input validation, and the
    ///   comparison routine should not be the thing that depends on it.
    /// * **Bounded time.** Values share `Arc` subtrees freely, so a DAG built
    ///   by repeatedly pairing a value with itself has a node count
    ///   exponential in its construction cost. `budget` counts every visited
    ///   pair against `max_budget` and returns [`MizuError::Timeout`] when it
    ///   runs out; `Arc::ptr_eq` short-circuits the shared case that makes
    ///   such a DAG cheap to build in the first place.
    ///
    /// `budget` is a running total the caller owns, so a loop of comparisons
    /// shares one ceiling rather than granting each iteration a fresh one.
    ///
    /// `Int` and `Decimal` compare across the scale factor in `i128`, so a
    /// magnitude that cannot be represented at `Decimal` scale answers
    /// "not equal" instead of failing.
    pub fn budget_eq(
        &self,
        other: &Self,
        budget: &mut u64,
        max_budget: u64,
    ) -> Result<bool, MizuError> {
        let mut stack: Vec<(&Value, &Value)> = vec![(self, other)];

        while let Some((a, b)) = stack.pop() {
            *budget = budget.saturating_add(1);
            if *budget > max_budget {
                return Err(MizuError::Timeout);
            }

            match (a, b) {
                (Value::Null, Value::Null) => {}
                (Value::Bool(x), Value::Bool(y)) => {
                    if x != y {
                        return Ok(false);
                    }
                }
                (Value::Int(x), Value::Int(y)) => {
                    if x != y {
                        return Ok(false);
                    }
                }
                (Value::Decimal(x), Value::Decimal(y)) => {
                    if x != y {
                        return Ok(false);
                    }
                }
                (Value::Int(x), Value::Decimal(y)) | (Value::Decimal(y), Value::Int(x)) => {
                    if (*x as i128) * (DECIMAL_SCALE as i128) != (*y as i128) {
                        return Ok(false);
                    }
                }
                (Value::String(x), Value::String(y)) => {
                    if Arc::ptr_eq(x, y) {
                        continue;
                    }
                    // A string compare is O(len), not the flat unit of work
                    // every other arm here costs; charge it before running it
                    // so a list of long strings cannot outrun the budget.
                    *budget = budget.saturating_add(x.len().min(y.len()) as u64);
                    if *budget > max_budget {
                        return Err(MizuError::Timeout);
                    }
                    if x != y {
                        return Ok(false);
                    }
                }
                (Value::FileHandle(x), Value::FileHandle(y)) => {
                    if !Arc::ptr_eq(x, y) {
                        return Ok(false);
                    }
                }
                (Value::List(x), Value::List(y)) => {
                    if Arc::ptr_eq(x, y) {
                        continue;
                    }
                    if x.len() != y.len() {
                        return Ok(false);
                    }
                    // Charge the whole run up front: the pairs are pushed now
                    // but charged as they pop, so without this the work stack
                    // could grow past `max_budget` entries before the budget
                    // noticed.
                    *budget = budget.saturating_add(x.len() as u64);
                    if *budget > max_budget {
                        return Err(MizuError::Timeout);
                    }
                    stack.extend(x.iter().zip(y.iter()));
                }
                (Value::Record(x), Value::Record(y)) => {
                    if Arc::ptr_eq(x, y) {
                        continue;
                    }
                    if x.len() != y.len() {
                        return Ok(false);
                    }
                    *budget = budget.saturating_add(x.len() as u64);
                    if *budget > max_budget {
                        return Err(MizuError::Timeout);
                    }
                    for (fx, fy) in x.iter().zip(y.iter()) {
                        // Both slices are key-sorted, so equal records must
                        // agree position by position. The `hash` compare makes
                        // the common mismatch a `u32` compare.
                        if fx.hash != fy.hash || fx.key != fy.key {
                            return Ok(false);
                        }
                        stack.push((&fx.value, &fy.value));
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}

/// Renders a [`DECIMAL_SCALE`]-scaled `i64` in decimal notation, exactly.
///
/// Trailing zeros in the fractional part are trimmed, so `1.50000000` prints
/// as `1.5`. `always_fractional` forces at least one fractional digit for a
/// whole value (`5` → `5.0`), which is what distinguishes a serialized
/// [`Value::Decimal`] from a serialized [`Value::Int`] on the way back in.
///
/// No step here goes through `f64`: the two halves are integer-divided out of
/// the scaled value and formatted as digits.
fn format_decimal(scaled: i64, always_fractional: bool) -> String {
    let integer_part = scaled / DECIMAL_SCALE;
    let fractional_part = (scaled % DECIMAL_SCALE).abs();
    // `-0.5` truncates to an integer part of `0`, which carries no sign of its
    // own; recover it from the scaled value.
    let sign = if scaled < 0 && integer_part == 0 {
        "-"
    } else {
        ""
    };

    if fractional_part == 0 {
        if always_fractional {
            format!("{sign}{integer_part}.0")
        } else {
            format!("{sign}{integer_part}")
        }
    } else {
        let mut frac = fractional_part;
        let mut num_digits = 8;
        // `fractional_part != 0` here, so this terminates on the last
        // significant digit rather than running the counter to zero.
        while frac % 10 == 0 {
            frac /= 10;
            num_digits -= 1;
        }
        format!("{sign}{integer_part}.{frac:0num_digits$}")
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            // A whole-valued decimal displays as `5`, not `5.0`: this is the
            // user-facing rendering, where the `Int`/`Decimal` split is not
            // supposed to be visible. `to_json` deliberately does the
            // opposite — see its doc comment.
            Value::Decimal(i) => f.write_str(&format_decimal(*i, false)),
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
                while let Some(field) = iter.next() {
                    write!(f, "{}: {}", field.key, field.value)?;
                    if iter.peek().is_some() {
                        write!(f, ", ")?;
                    }
                }
                write!(f, "}}")
            }
            // Redact to the display filename only — never the full path — and
            // strip control characters, which would otherwise let a crafted
            // filename inject escape sequences into a terminal log line or
            // break out of the `<file: …>` token in the inspector.
            //
            // `Value::String` above is deliberately *not* filtered the same
            // way: that is document text on its way to the renderer, where a
            // newline or tab is content, not an injection.
            Value::FileHandle(handle) => {
                let safe_name: String = handle
                    .filename
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                write!(f, "<file: {safe_name}>")
            }
        }
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

/// Maximum nesting depth accepted by [`from_json_str`] / [`from_json_slice`];
/// payloads nested deeper
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

/// Maximum total number of nodes accepted from an *untrusted* JSON payload.
///
/// `MAX_JSON_DEPTH` alone bounds only one dimension: `[[[…]]]` is caught,
/// but a flat `[0,0,0,…]` of a hundred million elements is depth 2 and would
/// be allocated in full before anything noticed. This caps the total.
///
/// It is deliberately *not* applied to storage rehydration — see
/// [`from_json_slice`] — because a record the app itself was allowed to write
/// must always be readable back.
/// Raised from 32_768 once the node cost was measured rather than guessed:
/// ~66 ns and 24 bytes per node for a flat list, ~117 ns for records, so a
/// full payload parses in ~17-29 ms and holds ~6-25 MB. The old ceiling
/// rejected ordinary API responses — a 5000-row table with six fields is
/// already over it — while the memory it saved was never the binding
/// constraint. What *is* worth watching is the product with
/// `max_concurrent_fetches` (16): record-heavy payloads at this cap put the
/// worst-case resident set near 400 MB, so the two constants should move
/// together.
const MAX_JSON_NODES: usize = 250_000;

/// Prefix marking a [`ValueSeed`] error as a limit violation rather than a
/// syntax error, so the boundary functions can pick the right [`MizuError`]
/// variant back out of the opaque `serde` error type.
const LIMIT_ERROR_PREFIX: &str = "mizu-limit: ";

/// Parses a JSON document into a Mizu [`Value`], enforcing the depth and node
/// limits as it goes.
///
/// Deserializing straight from the input rather than by way of
/// `serde_json::Value` is what makes the limits meaningful: an intermediate
/// `serde_json::Value` would already be fully materialized in memory by the
/// time any limit could be checked.
///
/// Mapping:
/// * `null` → [`Value::Null`]
/// * `bool` → [`Value::Bool`]
/// * number without `.` or exponent → [`Value::Int`], exact over the full
///   `i64` range
/// * number with `.` or exponent → [`Value::Decimal`], scaled by
///   [`DECIMAL_SCALE`] (see `parse_json_number_exact` for the precision
///   rules)
/// * string → [`Value::String`]
/// * array → [`Value::List`] (elements converted recursively, depth-bounded)
/// * object → [`Value::Record`] (values converted recursively, depth-bounded)
///
/// The `Int`/`Decimal` split mirrors [`to_json`] exactly, so
/// `from_json_str(to_json(v))` reproduces `v`'s variants and not just its
/// magnitudes.
///
/// # Errors
///
/// [`MizuError::SecurityViolation`] if the payload exceeds `MAX_JSON_DEPTH`
/// or `MAX_JSON_NODES`, or if a number's scaled value doesn't fit in `i64`.
/// Over-limit input is rejected outright rather than truncated to
/// [`Value::Null`] — truncation would let a caller mistake attacker-controlled
/// data for a legitimate absence of a value.
///
/// [`MizuError::ParseError`] if the payload is not well-formed JSON. The two
/// are distinct because only the first says anything about intent.
pub fn from_json_str(payload: &str) -> Result<Value, MizuError> {
    let mut deserializer = serde_json::Deserializer::from_str(payload);
    disable_serde_recursion_limit(&mut deserializer);
    let mut nodes = 0;
    let seed = ValueSeed {
        depth: 0,
        nodes: &mut nodes,
        max_nodes: MAX_JSON_NODES,
    };
    serde::de::DeserializeSeed::deserialize(seed, &mut deserializer).map_err(classify_json_error)
}

/// [`from_json_str`] over bytes, with the node cap made conditional.
///
/// `is_trusted` is set only for storage rehydration, whose input is a record
/// this build previously wrote and encrypted under a key only this build
/// holds. Applying `MAX_JSON_NODES` there would mean a value the evaluator
/// was allowed to construct and persist could silently fail to load on the
/// next start — the same availability bug `MAX_JSON_DEPTH`'s doc comment
/// describes, and the reason that constant is tied to [`MAX_EVAL_DEPTH`]
/// rather than given a smaller independent value.
///
/// Depth is enforced either way: it is the bound that protects the native
/// stack during the parse itself, which is not a property trust can relax.
pub fn from_json_slice(payload: &[u8], is_trusted: bool) -> Result<Value, MizuError> {
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    disable_serde_recursion_limit(&mut deserializer);
    let mut nodes = 0;
    let seed = ValueSeed {
        depth: 0,
        nodes: &mut nodes,
        max_nodes: if is_trusted {
            usize::MAX
        } else {
            MAX_JSON_NODES
        },
    };
    serde::de::DeserializeSeed::deserialize(seed, &mut deserializer).map_err(classify_json_error)
}

/// Hands depth enforcement to `MAX_JSON_DEPTH` alone.
///
/// `serde_json` defaults to its own 128-level recursion cap, which is *lower*
/// than [`MAX_EVAL_DEPTH`]. Left in place it silently falsifies the invariant
/// `MAX_JSON_DEPTH`'s doc comment relies on: a `Value` the evaluator was
/// allowed to build 200 levels deep would serialize and persist fine, then
/// fail to deserialize on the next load — and `storage::read_all` logs that
/// failure and drops the record, so the data would simply disappear.
///
/// Turning it off is safe because [`ValueSeed::deserialize`] checks
/// `MAX_JSON_DEPTH` on entry to every level, *before* descending, so the
/// native stack is still bounded — by one limit instead of two that disagree.
///
/// # Safety
///
/// `disable_recursion_limit` is not `unsafe`; the caller's obligation is that
/// some other bound exists, which is the seed's depth check.
fn disable_serde_recursion_limit<'de, R>(deserializer: &mut serde_json::Deserializer<R>)
where
    R: serde_json::de::Read<'de>,
{
    deserializer.disable_recursion_limit();
}

/// Sorts a `serde_json` error into "we rejected it" versus "it was malformed".
fn classify_json_error(e: serde_json::Error) -> MizuError {
    let msg = e.to_string();
    match msg.strip_prefix(LIMIT_ERROR_PREFIX) {
        Some(limit) => MizuError::SecurityViolation(limit.to_string()),
        None => MizuError::ParseError(format!("malformed JSON payload: {msg}")),
    }
}

/// Converts one JSON number token, preserving the integer/decimal distinction
/// its spelling carries.
///
/// `serde_json` is built here with `arbitrary_precision`, so every number
/// arrives as its original text rather than pre-rounded through `f64`. That
/// is what makes an exact path possible at all:
///
/// * No `.` and no exponent → [`Value::Int`], exact across the whole `i64`
///   range. Scaling these into fixed point would cap integers at
///   `±92,233,720,368` for no reason.
/// * Otherwise → [`Value::Decimal`]. A plain fractional literal is assembled
///   digit by digit in `i64`, exactly, and rounded half-away-from-zero at the
///   9th fractional digit (matching `f64::round`, which the exponent path
///   below uses). An exponent form goes through `f64`, which carries only
///   ~15-17 significant decimal digits — insufficient to represent every
///   value in this type's range exactly — so `1.23456789012e5` may land on
///   the nearest `f64`-exact neighbour rather than its exact decimal value.
fn parse_json_number_exact(s: &str) -> Result<Value, MizuError> {
    let has_exponent = s.contains('e') || s.contains('E');
    let mut parts = s.split('.');
    let int_str = parts.next().unwrap_or("0");
    let frac_str = parts.next().unwrap_or("");
    let extra_dot = parts.next().is_some();

    if extra_dot || has_exponent {
        let float_val: f64 = s
            .parse()
            .map_err(|_| MizuError::ParseError(format!("malformed JSON number `{s}`")))?;
        let scaled = (float_val * (DECIMAL_SCALE as f64)).round();
        // `i64::MAX as f64` rounds *up* to 2^63, so the upper test must be
        // inclusive; `as i64` would otherwise saturate silently.
        if !scaled.is_finite() || scaled >= -(i64::MIN as f64) || scaled < i64::MIN as f64 {
            return Err(MizuError::SecurityViolation(
                "JSON number exceeds representable range".to_string(),
            ));
        }
        return Ok(Value::Decimal(scaled as i64));
    }

    let int_part: i64 = int_str.parse().map_err(|_| {
        MizuError::SecurityViolation(format!("JSON integer `{int_str}` exceeds the 64-bit range"))
    })?;

    // No fractional part: an exact integer, kept unscaled.
    if frac_str.is_empty() {
        return Ok(Value::Int(int_part));
    }

    let is_negative = int_str.starts_with('-');
    let mut frac_val: i64 = 0;
    let mut mult = DECIMAL_SCALE / 10;
    let mut round_up = false;
    for (i, c) in frac_str.bytes().enumerate() {
        if !c.is_ascii_digit() {
            return Err(MizuError::ParseError(format!(
                "malformed JSON number `{s}`"
            )));
        }
        let digit = (c - b'0') as i64;
        if i < 8 {
            frac_val += digit * mult;
            mult /= 10;
        } else {
            // Beyond the representable scale. Round half away from zero on
            // the first dropped digit and ignore the rest, rather than
            // truncating (which biases every value toward zero) or rejecting
            // (which would fail on ordinary API responses).
            if i == 8 {
                round_up = digit >= 5;
            }
            if round_up {
                break;
            }
        }
    }
    if round_up {
        frac_val += 1;
    }

    let base = int_part.checked_mul(DECIMAL_SCALE).ok_or_else(|| {
        MizuError::SecurityViolation(format!(
            "JSON number `{s}` exceeds the fixed-point range (±92,233,720,368.54775807)"
        ))
    })?;
    // `-0.5` has an integer part of `0`, which carries no sign of its own —
    // hence the textual check rather than `base < 0`.
    let scaled = if is_negative {
        base.checked_sub(frac_val)
    } else {
        base.checked_add(frac_val)
    }
    .ok_or_else(|| {
        MizuError::SecurityViolation(format!("JSON number `{s}` exceeds the fixed-point range"))
    })?;

    Ok(Value::Decimal(scaled))
}

/// Depth- and node-bounded [`serde::de::DeserializeSeed`] for [`Value`].
struct ValueSeed<'a> {
    depth: u32,
    /// Running node total, shared by every seed in one parse.
    nodes: &'a mut usize,
    /// Ceiling for `nodes`; `usize::MAX` disables the check for trusted input.
    max_nodes: usize,
}

impl<'a, 'de> serde::de::DeserializeSeed<'de> for ValueSeed<'a> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        *self.nodes += 1;
        if *self.nodes > self.max_nodes {
            return Err(serde::de::Error::custom(format!(
                "{LIMIT_ERROR_PREFIX}JSON payload exceeds the maximum of {} nodes",
                self.max_nodes
            )));
        }
        if self.depth > MAX_JSON_DEPTH {
            return Err(serde::de::Error::custom(format!(
                "{LIMIT_ERROR_PREFIX}JSON payload exceeds the maximum nesting depth of {MAX_JSON_DEPTH}"
            )));
        }
        deserializer.deserialize_any(self)
    }
}

impl<'a, 'de> serde::de::Visitor<'de> for ValueSeed<'a> {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(Value::Int(v))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        let i = i64::try_from(v).map_err(|_| {
            serde::de::Error::custom(format!(
                "{LIMIT_ERROR_PREFIX}JSON integer `{v}` exceeds the 64-bit range"
            ))
        })?;
        Ok(Value::Int(i))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        let scaled = (v * (DECIMAL_SCALE as f64)).round();
        // Inclusive upper bound: `i64::MAX as f64` rounds up to 2^63.
        if !scaled.is_finite() || scaled >= -(i64::MIN as f64) || scaled < i64::MIN as f64 {
            return Err(serde::de::Error::custom(format!(
                "{LIMIT_ERROR_PREFIX}JSON number exceeds representable range"
            )));
        }
        Ok(Value::Decimal(scaled as i64))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Value::String(std::sync::Arc::from(v)))
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(Value::String(std::sync::Arc::from(v.as_str())))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<V>(self, mut visitor: V) -> Result<Self::Value, V::Error>
    where
        V: serde::de::SeqAccess<'de>,
    {
        // No `with_capacity(size_hint)`: the hint comes from the payload, so
        // honouring it would let a short input reserve an arbitrary amount.
        let mut items = Vec::new();
        while let Some(value) = visitor.next_element_seed(ValueSeed {
            depth: self.depth + 1,
            nodes: self.nodes,
            max_nodes: self.max_nodes,
        })? {
            items.push(value);
        }
        Ok(Value::List(std::sync::Arc::new(items)))
    }

    fn visit_map<V>(self, mut visitor: V) -> Result<Self::Value, V::Error>
    where
        V: serde::de::MapAccess<'de>,
    {
        let mut pairs = Vec::new();
        while let Some(key) = visitor.next_key::<String>()? {
            // `arbitrary_precision` delivers every number as a one-field map
            // carrying the original token text; that is the only path numbers
            // take through this deserializer.
            if key == "$serde_json::private::Number" {
                let s: String = visitor.next_value()?;
                return parse_json_number_exact(&s).map_err(|e| match e {
                    // Re-tag so `classify_json_error` recovers the same
                    // distinction on the way back out.
                    MizuError::SecurityViolation(m) => {
                        serde::de::Error::custom(format!("{LIMIT_ERROR_PREFIX}{m}"))
                    }
                    other => serde::de::Error::custom(other.to_string()),
                });
            }
            let value = visitor.next_value_seed(ValueSeed {
                depth: self.depth + 1,
                nodes: self.nodes,
                max_nodes: self.max_nodes,
            })?;
            pairs.push((key, value));
        }
        Ok(Value::record_from_unsorted(pairs))
    }
}

/// Converts a Mizu [`Value`] into the corresponding `serde_json::Value`.
///
/// Mapping (inverse of [`from_json_str`] / [`from_json_slice`]):
/// * [`Value::Null`]   → `null`
/// * [`Value::Bool`]   → `bool`
/// * [`Value::Int`]    → `number`, written without a fractional part
/// * [`Value::Decimal`] → `number`, unscaled by [`DECIMAL_SCALE`] and *always*
///   written with a fractional part — `Decimal(500_000_000)` emits `5.0`, not
///   `5`. That trailing `.0` is what makes the round-trip variant-preserving:
///   the parser picks [`Value::Int`] versus [`Value::Decimal`] off exactly
///   this spelling, so emitting a bare `5` would turn every whole-numbered
///   decimal into an integer on the next read — silently changing its
///   arithmetic behaviour across a save/load cycle. The digits are assembled
///   textually and handed to `serde_json` at arbitrary precision, so no value
///   passes through `f64` on the way out.
/// * [`Value::String`] → `string`
/// * [`Value::List`]   → `array` (elements converted recursively)
/// * [`Value::Record`] → `object` (values converted recursively)
/// * [`Value::FileHandle`] → always `Err` (see the variant's own doc comment)
///   — never silently stringified. A local filesystem path reaching a JSON
///   body sent to a server (or written to disk via `store_local`) is its own
///   information leak; a `FileHandle` reaches the wire only via the dedicated
///   `Multipart` payload format, never through `to_json`.
///
/// # Errors
///
/// Returns [`MizuError::ExecutionError`] if `val` contains a
/// [`Value::FileHandle`] anywhere (including nested inside a `List`/`Record`).
pub fn to_json(val: &Value) -> Result<serde_json::Value, MizuError> {
    match val {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Int(i) => Ok(serde_json::Value::Number(serde_json::Number::from(*i))),
        Value::Decimal(i) => {
            use std::str::FromStr;

            let num_str = format_decimal(*i, true);
            serde_json::Number::from_str(&num_str)
                .map(serde_json::Value::Number)
                .map_err(|_| {
                    MizuError::ExecutionError(format!(
                        "failed to serialize decimal `{num_str}` exactly"
                    ))
                })
        }
        Value::String(s) => Ok(serde_json::Value::String(s.to_string())),
        Value::List(items) => {
            let arr = items.iter().map(to_json).collect::<Result<Vec<_>, _>>()?;
            Ok(serde_json::Value::Array(arr))
        }
        Value::Record(slice) => {
            let obj: serde_json::Map<String, serde_json::Value> = slice
                .iter()
                .map(|f| Ok((f.key.to_string(), to_json(&f.value)?)))
                .collect::<Result<_, MizuError>>()?;
            Ok(serde_json::Value::Object(obj))
        }
        Value::FileHandle(_) => Err(MizuError::ExecutionError(
            "a file selection cannot be JSON-encoded; use `as multipart` to upload it".to_string(),
        )),
    }
}
