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

/// The set of all primitive values in the Mizu type system.
#[derive(Debug, Clone)]
pub struct RecordField {
    pub key: Arc<str>,
    pub hash: u32,
    pub value: Value,
}

#[inline]
pub fn hash_field(s: &str) -> u32 {
    let mut hash: u32 = 2166136261;
    for b in s.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

#[cfg(test)]
impl PartialEq for RecordField {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.value == other.value
    }
}


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

#[cfg(test)]
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        let mut stack = vec![(self, other)];
        let mut budget = 100_000_u32;

        while let Some((a, b)) = stack.pop() {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                panic!("Value PartialEq budget exceeded in tests! Use budget_eq.");
            }

            match (a, b) {
                (Value::Null, Value::Null) => {}
                (Value::Bool(x), Value::Bool(y)) if x == y => {}
                (Value::Int(x), Value::Int(y)) if x == y => {}
                (Value::Decimal(x), Value::Decimal(y)) if x == y => {}
                (Value::Int(x), Value::Decimal(y)) | (Value::Decimal(y), Value::Int(x)) => {
                    if let Some(scaled_x) = x.checked_mul(DECIMAL_SCALE) {
                        if scaled_x == *y { continue; }
                    }
                    return false;
                }
                (Value::String(x), Value::String(y)) if x == y => {}
                (Value::FileHandle(x), Value::FileHandle(y)) if Arc::ptr_eq(x, y) => {}
                (Value::List(x), Value::List(y)) => {
                    if Arc::ptr_eq(x, y) { continue; }
                    if x.len() != y.len() { return false; }
                    for (vx, vy) in x.iter().zip(y.iter()) {
                        stack.push((vx, vy));
                    }
                }
                (Value::Record(x), Value::Record(y)) => {
                    if Arc::ptr_eq(x, y) { continue; }
                    if x.len() != y.len() { return false; }
                    for (fx, fy) in x.iter().zip(y.iter()) {
                        if fx.key != fy.key { return false; }
                        stack.push((&fx.value, &fy.value));
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

    // The Drop implementation was removed.

impl Value {
    /// Builds a [`Value::Record`] from unordered key-value pairs, establishing
    /// the lexicographic ordering the variant requires and deduplicating keys.
    ///
    /// This is the only place records are ordered; every construction site
    /// goes through it so the invariant cannot drift between them. Duplicate
    /// keys are deduplicated to ensure deterministic object layouts.
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
    /// is a [`Value::Record`]. Uses a binary search over the sorted fields.
    pub fn get_field(&self, field_hash: u32, field_name: &str) -> Option<&Value> {
        if let Value::Record(slice) = self {
            for f in slice.iter() {
                if f.hash == field_hash && f.key.as_ref() == field_name {
                    return Some(&f.value);
                }
            }
        }
        None
    }

    /// Performs structural equality check with a hard budget to prevent
    /// "Billion Laughs" algorithmic DoS on deeply nested identical `Arc` references.
    pub fn budget_eq(&self, other: &Self, budget: &mut u64, max_budget: u64) -> Result<bool, MizuError> {
        *budget = budget.saturating_add(1);
        if *budget > max_budget {
            return Err(MizuError::Timeout);
        }
        match (self, other) {
            (Value::List(la), Value::List(lb)) => {
                if Arc::ptr_eq(la, lb) { return Ok(true); }
                if la.len() != lb.len() { return Ok(false); }
                for (va, vb) in la.iter().zip(lb.iter()) {
                    if !va.budget_eq(vb, budget, max_budget)? { return Ok(false); }
                }
                Ok(true)
            },
            (Value::Record(ra), Value::Record(rb)) => {
                if Arc::ptr_eq(ra, rb) { return Ok(true); }
                if ra.len() != rb.len() { return Ok(false); }
                for (fa, fb) in ra.iter().zip(rb.iter()) {
                    if fa.key != fb.key { return Ok(false); }
                    if !fa.value.budget_eq(&fb.value, budget, max_budget)? { return Ok(false); }
                }
                Ok(true)
            },
            (Value::Null, Value::Null) => Ok(true),
            (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
            (Value::Int(x), Value::Int(y)) => Ok(x == y),
            (Value::Decimal(x), Value::Decimal(y)) => Ok(x == y),
            (Value::Int(x), Value::Decimal(y)) | (Value::Decimal(y), Value::Int(x)) => {
                if let Some(scaled_x) = x.checked_mul(DECIMAL_SCALE) {
                    Ok(scaled_x == *y)
                } else {
                    Ok(false)
                }
            },
            (Value::String(x), Value::String(y)) => Ok(x == y),
            (Value::FileHandle(x), Value::FileHandle(y)) => Ok(Arc::ptr_eq(x, y)),
            _ => Ok(false),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Decimal(i) => {
                let integer_part = i / DECIMAL_SCALE;
                let fractional_part = (i % DECIMAL_SCALE).abs();

                if fractional_part == 0 {
                    write!(f, "{}", integer_part)
                } else {
                    if *i < 0 && integer_part == 0 {
                        write!(f, "-0.")?;
                    } else {
                        write!(f, "{}.", integer_part)?;
                    }
                    let mut frac = fractional_part;
                    let mut num_digits = 8;
                    while frac % 10 == 0 && frac > 0 {
                        frac /= 10;
                        num_digits -= 1;
                    }
                    write!(f, "{:0width$}", frac, width = num_digits)
                }
            },
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
            // Sanitize control characters to prevent log/terminal injection.
            Value::FileHandle(handle) => {
                let safe_name: String = handle.filename.chars().filter(|c| !c.is_control()).collect();
                write!(f, "<file: {}>", safe_name)
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

/// Maximum total number of items in a JSON payload to prevent
/// memory exhaustion DoS from excessively large payloads.
const MAX_JSON_NODES: usize = 32768;

/// Converts a `serde_json::Value` into a Mizu [`Value`].
///
/// Mapping:
/// * `null` → [`Value::Null`]
/// * `bool` → [`Value::Bool`]
/// * number → [`Value::Decimal`], scaled by `DECIMAL_SCALE` (`Value` has no
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
pub fn from_json_str(payload: &str) -> Result<Value, MizuError> {
    let mut deserializer = serde_json::Deserializer::from_str(payload);
    let mut nodes = 0;
    let seed = ValueSeed { depth: 0, nodes: &mut nodes };
    serde::de::DeserializeSeed::deserialize(seed, &mut deserializer)
        .map_err(|e| MizuError::SecurityViolation(e.to_string()))
}

pub fn from_json_slice(payload: &[u8], is_trusted: bool) -> Result<Value, MizuError> {
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    let mut nodes = 0;
    let seed = ValueSeed { depth: 0, nodes: if is_trusted { &mut 0 } else { &mut nodes } };
    serde::de::DeserializeSeed::deserialize(seed, &mut deserializer)
        .map_err(|e| MizuError::SecurityViolation(e.to_string()))
}

fn parse_json_number_exact(s: &str) -> Result<Value, MizuError> {
    let mut parts = s.split('.');
    let int_str = parts.next().unwrap_or("0");
    let frac_str = parts.next().unwrap_or("0");
    
    if parts.next().is_some() || s.contains('e') || s.contains('E') {
        let float_val: f64 = s.parse().map_err(|_| MizuError::SecurityViolation("JSON number could not be parsed".to_string()))?;
        let scaled = (float_val * (DECIMAL_SCALE as f64)).round();
        if !scaled.is_finite() || scaled > i64::MAX as f64 || scaled < i64::MIN as f64 {
            return Err(MizuError::SecurityViolation("JSON number exceeds representable range".to_string()));
        }
        return Ok(Value::Decimal(scaled as i64));
    }
    
    let is_negative = int_str.starts_with('-');
    let int_part: i64 = int_str.parse().map_err(|_| MizuError::SecurityViolation("JSON number integer overflow".to_string()))?;
    
    let mut frac_val: i64 = 0;
    let mut mult = DECIMAL_SCALE / 10;
    for (i, c) in frac_str.chars().enumerate() {
        if !c.is_ascii_digit() { return Err(MizuError::SecurityViolation("Invalid fraction".to_string())); }
        if i >= 8 {
            break;
        }
        if mult > 0 {
            let digit = (c as u8 - b'0') as i64;
            frac_val += digit * mult;
            mult /= 10;
        }
    }
    
    let base = int_part.checked_mul(DECIMAL_SCALE).ok_or_else(|| MizuError::SecurityViolation("JSON number integer overflow".to_string()))?;
    let scaled = if is_negative && base == 0 {
        -frac_val
    } else if is_negative {
        base.checked_sub(frac_val).ok_or_else(|| MizuError::SecurityViolation("JSON number overflow".to_string()))?
    } else {
        base.checked_add(frac_val).ok_or_else(|| MizuError::SecurityViolation("JSON number overflow".to_string()))?
    };
    
    Ok(Value::Decimal(scaled))
}

struct ValueSeed<'a> {
    depth: u32,
    nodes: &'a mut usize,
}

impl<'a, 'de> serde::de::DeserializeSeed<'de> for ValueSeed<'a> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        *self.nodes += 1;
        if *self.nodes > MAX_JSON_NODES {
            return Err(serde::de::Error::custom(format!("JSON payload exceeds maximum total node limit of {MAX_JSON_NODES}")));
        }
        if self.depth > MAX_JSON_DEPTH {
            return Err(serde::de::Error::custom(format!("JSON payload exceeds maximum nesting depth of {MAX_JSON_DEPTH}")));
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
        let i = i64::try_from(v).map_err(|_| serde::de::Error::custom("JSON number integer overflow"))?;
        Ok(Value::Int(i))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        let scaled = (v * (DECIMAL_SCALE as f64)).round();
        if !scaled.is_finite() || scaled > i64::MAX as f64 || scaled < i64::MIN as f64 {
            return Err(serde::de::Error::custom("JSON number exceeds representable range"));
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
        let mut items = Vec::new();
        while let Some(value) = visitor.next_element_seed(ValueSeed {
            depth: self.depth + 1,
            nodes: self.nodes,
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
            if key == "$serde_json::private::Number" {
                let s: String = visitor.next_value()?;
                return parse_json_number_exact(&s).map_err(serde::de::Error::custom);
            }
            let value = visitor.next_value_seed(ValueSeed {
                depth: self.depth + 1,
                nodes: self.nodes,
            })?;
            pairs.push((key, value));
        }
        Ok(Value::record_from_unsorted(pairs))
    }
}

/// Converts a Mizu [`Value`] into the corresponding `serde_json::Value`.
///
/// Mapping (inverse of [`from_json`]):
/// * [`Value::Null`]    → `null`
/// * [`Value::Bool`]   → `bool`
/// * [`Value::Decimal`]    → `number` (unscaled by `DECIMAL_SCALE` — `Value` has
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
        Value::Int(i) => Ok(serde_json::Value::Number(serde_json::Number::from(*i))),
        Value::Decimal(i) => {
            let integer_part = i / DECIMAL_SCALE;
            let fractional_part = (i % DECIMAL_SCALE).abs();

            if fractional_part == 0 {
                use std::str::FromStr;
                let num_str = format!("{}.0", integer_part);
                Ok(serde_json::Value::Number(serde_json::Number::from_str(&num_str).unwrap()))
            } else {
                use std::str::FromStr;
                let sign = if *i < 0 && integer_part == 0 { "-" } else { "" };
                
                let mut frac = fractional_part;
                let mut num_digits = 8;
                while frac % 10 == 0 && frac > 0 {
                    frac /= 10;
                    num_digits -= 1;
                }
                
                let num_str = format!("{}{}.{:0width$}", sign, integer_part, frac, width = num_digits);
                
                Ok(serde_json::Number::from_str(&num_str)
                    .map(serde_json::Value::Number)
                    .map_err(|_| MizuError::ExecutionError("failed to serialize decimal exactly".to_string()))?)
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
