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
/// spirit as the fixed-point `Value::Int` scale and the evaluator's bounded
/// recursion); loading a whole file into a `Value` the evaluator freely
/// passes through `filter`/`store_local`/comparisons would blow past that,
/// and would make raw file content reachable from ordinary logic in ways
/// nothing here is designed to gate. The file's bytes are read only by the
/// network worker, in bounded chunks, at the moment a `Multipart` request is
/// actually sent (see `network::worker::multipart`).
#[derive(Debug)]
pub struct FileHandleData {
    /// Absolute filesystem path to the selected file.
    pub path: std::path::PathBuf,
    /// User-visible original filename (the last path component at selection
    /// time), used for the `Content-Disposition: filename=` part of a
    /// multipart upload. Never a path — see `network::worker::multipart`'s
    /// filename sanitisation for why even this is not trusted verbatim.
    pub filename: String,
}

/// The set of all primitive values in the Mizu type system.
/// Pre-calculates a 32-bit key hash used to short-circuit field lookups.
///
/// FNV-1a, 32-bit. Deliberately *not* `FxHasher` (or any `DefaultHasher`):
/// those mix at the native word size, so the same key hashes differently on a
/// 32-bit and a 64-bit target. These hashes are baked into parsed field-access
/// nodes, so a word-size-dependent hash would make the generated program
/// representation architecture-dependent. Every operation here is `u32`
/// wrapping arithmetic — no `usize` anywhere — so the result is identical on
/// all targets.
#[inline]
pub fn hash_field(key: &str) -> u32 {
    const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in key.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[derive(Debug, Clone)]
pub struct RecordField {
    /// [`hash_field`] of `key`, precomputed so lookups compare a `u32` before
    /// ever touching the string bytes.
    pub hash: u32,
    pub key: Arc<str>,
    pub value: Value,
}

impl PartialEq for RecordField {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.key == other.key && self.value == other.value
    }
}


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

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Record(a), Value::Record(b)) => a == b,
            // `FileHandle` is deliberately never equal to anything, even to
            // itself — comparing file selections isn't a meaningful
            // operation this type system supports, and path-based equality
            // would leak filesystem layout through comparison behavior.
            _ => false,
        }
    }
}

impl Value {
    /// Builds a [`Value::Record`] from unordered key-value pairs, computing
    /// each key's hash and establishing the lexicographic ordering the variant
    /// requires.
    ///
    /// This is the only place records are ordered; every construction site
    /// goes through it so the invariant cannot drift between them. Duplicate
    /// keys are not deduplicated here — callers building from a map type
    /// cannot produce them, and `get_field` would return the first match.
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
    /// needing a fallback pass.
    pub fn get_field(&self, field_hash: u32, field_name: &str) -> Option<&Value> {
        match self {
            Value::Record(slice) => slice
                .iter()
                .find(|f| f.hash == field_hash && f.key.as_ref() == field_name)
                .map(|f| &f.value),
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
                    let mut frac_str = format!("{:08}", fractional_part);
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
                while let Some(field) = iter.next() {
                    write!(f, "{}: {}", field.key, field.value)?;
                    if iter.peek().is_some() {
                        write!(f, ", ")?;
                    }
                }
                write!(f, "}}")
            }
            // Redact to the display filename only — never the full path.
            Value::FileHandle(handle) => write!(f, "<file: {}>", handle.filename),
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
/// * number → [`Value::Int`], scaled by `DECIMAL_SCALE` (`Value` has no
///   separate floating-point variant). Integer literals (no `.` or
///   exponent in the source) take an exact `checked_mul` path. Fractional
///   literals go through `f64`, which carries only ~15-17 significant
///   decimal digits — insufficient to exactly represent every value in
///   this type's range (an 11-digit integer part plus 8 fractional digits
///   is up to ~19 significant digits), so a fractional literal near the
///   top of the representable range may round to the nearest `f64`-exact
///   neighbor rather than its exact decimal value.
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
/// for a legitimate absence of a value. Also returns
/// [`MizuError::SecurityViolation`] if a JSON number's scaled value doesn't
/// fit in `i64`, rather than silently truncating or wrapping it.
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
            // Integer JSON literals take an exact path: `checked_mul` either
            // produces the exact scaled value or fails cleanly on overflow,
            // never silently truncating. Fractional literals still go
            // through `f64`, whose ~15-17 significant decimal digits cannot
            // exactly represent every value in this type's range (an
            // 11-digit integer part plus 8 fractional digits is up to ~19
            // significant digits) — this is a real precision boundary, not
            // an oversight, so overflow/non-finite results are rejected
            // rather than silently rounded to a nearby-but-wrong value.
            if let Some(i) = n.as_i64() {
                return i.checked_mul(DECIMAL_SCALE).map(Value::Int).ok_or_else(|| {
                    MizuError::SecurityViolation(
                        "JSON number exceeds representable range".to_string(),
                    )
                });
            }
            let float_val = n.as_f64().ok_or_else(|| {
                MizuError::SecurityViolation("JSON number could not be parsed".to_string())
            })?;
            let scaled = (float_val * (DECIMAL_SCALE as f64)).round();
            if !scaled.is_finite() || scaled > i64::MAX as f64 || scaled < i64::MIN as f64 {
                return Err(MizuError::SecurityViolation(
                    "JSON number exceeds representable range".to_string(),
                ));
            }
            Ok(Value::Int(scaled as i64))
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
            let pairs = map
                .iter()
                .map(|(k, v)| Ok((k.as_str(), from_json_bounded(v, depth + 1)?)))
                .collect::<Result<Vec<(&str, Value)>, MizuError>>()?;
            Ok(Value::record_from_unsorted(pairs))
        }
    }
}

/// Converts a Mizu [`Value`] into the corresponding `serde_json::Value`.
///
/// Mapping (inverse of [`from_json`]):
/// * [`Value::Null`]    → `null`
/// * [`Value::Bool`]   → `bool`
/// * [`Value::Int`]    → `number` (unscaled by `DECIMAL_SCALE` — `Value` has
///   no floating-point variant of its own; the fixed-point `Int`
///   representation stands in for both integers and floats. A value that is
///   a whole number at this scale emits an exact JSON integer; otherwise it
///   unscales through `f64` and falls back to `null` if the result isn't
///   finite, which `serde_json::Number` cannot represent.)
/// * [`Value::String`] → `string`
/// * [`Value::List`]   → `array` (elements converted recursively)
/// * [`Value::Record`] → `object` (values converted recursively)
/// * [`Value::FileHandle`] → always `Err` (see the field's own doc comment
///   below) — never silently stringified. A local filesystem path reaching a
///   JSON body sent to a server (or written to disk via `store_local`) is
///   its own information leak; a `FileHandle` reaches the wire only via the
///   dedicated `Multipart` payload format, never through `to_json`.
///
/// # Errors
///
/// Returns [`MizuError::ExecutionError`] if `val` contains a
/// [`Value::FileHandle`] anywhere (including nested inside a `List`/`Record`).
pub fn to_json(val: &Value) -> Result<serde_json::Value, MizuError> {
    match val {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Int(i) => {
            // Whole values take the exact integer path (mirrors from_json's
            // exact path for whole-number JSON literals) instead of always
            // dividing through f64, which cannot exactly represent every
            // value in this type's range.
            if i % DECIMAL_SCALE == 0 {
                Ok(serde_json::Value::Number(serde_json::Number::from(
                    i / DECIMAL_SCALE,
                )))
            } else {
                let unscaled = *i as f64 / (DECIMAL_SCALE as f64);
                Ok(serde_json::Number::from_f64(unscaled)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null))
            }
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
