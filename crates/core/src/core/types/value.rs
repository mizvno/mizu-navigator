//! The `Value` enum and JSON (de)serialization.

use std::fmt;
use std::sync::Arc;

use crate::core::errors::MizuError;

use super::eval::MAX_EVAL_DEPTH;

/// The scale factor used for fixed-point arithmetic.
pub const DECIMAL_SCALE: i64 = 10_000;

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
