//! Value comparison and dot-path resolution: `compare_values` (Strict Weak
//! Ordering across every `Value` variant, via `variant_weight`), and
//! `resolve_dot_path` (nested-record field navigation). Also `field_value`,
//! a test-only convenience wrapper.

use super::super::value::Value;

/// Returns the value of `field` in `item` if `item` is a `Record`.
///
/// Test-only. It hashes `field` on every call, so the production callers that
/// look one field up across a whole list — `sort`, `count`, `filter` — hoist
/// [`crate::core::types::hash_field`] out of their loop and call
/// [`Value::get_field`] directly instead of going through this.
#[cfg(test)]
pub(crate) fn field_value<'a>(item: &'a Value, field: &str) -> Option<&'a Value> {
    item.get_field(crate::core::types::hash_field(field), field)
}

/// Navigates a dot-separated path through nested `Value::Record` values,
/// returning a reference to the leaf.
pub(super) fn resolve_dot_path<'a, I>(root: &'a Value, segments: I) -> Option<&'a Value>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut current = root;
    for segment in segments {
        current = current.get_field(
            crate::core::types::hash_field(segment.as_ref()),
            segment.as_ref(),
        )?;
    }
    Some(current)
}

/// Returns a stable numeric weight for each `Value` variant.
///
/// This weight is the tiebreaker used by [`compare_values`] when the two
/// values belong to different variants.  The ordering is arbitrary but fixed,
/// which is sufficient to satisfy Strict Weak Ordering.
///
/// Weights: Null=1, Bool=2, numeric=3, String=4, List=5, Record=6,
/// FileHandle=7.
///
/// `Int` and `Decimal` deliberately share a weight: they are one surface type
/// (`num`), so a heterogeneous pair of them must not be ordered by variant.
/// [`compare_values`] handles every such pair numerically before reaching
/// here, so the shared weight is never actually the deciding factor.
#[inline]
pub(crate) fn variant_weight(v: &Value) -> u8 {
    match v {
        Value::Null => 1,
        Value::Bool(_) => 2,
        Value::Int(_) => 3,
        Value::Decimal(_) => 3,
        Value::String(_) => 4,
        Value::List(_) => 5,
        Value::Record(_) => 6,
        // Not meaningfully orderable — see `Value::PartialEq`'s doc comment;
        // this only needs a stable position for `sort`'s heterogeneous-pair
        // tiebreaker, never a real ordering between two file selections.
        Value::FileHandle(_) => 7,
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
pub(crate) fn compare_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,

        (Some(Value::Null), Some(Value::Null)) => Ordering::Equal,
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),

        // Numerics compare across the `Int`/`Decimal` split, in `i128` so the
        // upscale cannot overflow. Without the mixed arms these pairs would
        // fall through to `variant_weight`, which assigns both variants the
        // same weight — every comparison would answer `Equal` and `sort`
        // would silently leave a list of numbers in its original order.
        (Some(Value::Int(x)), Some(Value::Int(y))) => x.cmp(y),
        (Some(Value::Decimal(x)), Some(Value::Decimal(y))) => x.cmp(y),
        (Some(Value::Int(x)), Some(Value::Decimal(y))) => {
            (*x as i128 * super::super::value::DECIMAL_SCALE as i128).cmp(&(*y as i128))
        }
        (Some(Value::Decimal(x)), Some(Value::Int(y))) => {
            (*x as i128).cmp(&(*y as i128 * super::super::value::DECIMAL_SCALE as i128))
        }

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
            for (fa, fb) in x.iter().zip(y.iter()) {
                let key_ord = fa.key.cmp(&fb.key);
                if key_ord != Ordering::Equal {
                    return key_ord;
                }
                let val_ord = compare_values(Some(&fa.value), Some(&fb.value));
                if val_ord != Ordering::Equal {
                    return val_ord;
                }
            }
            x.len().cmp(&y.len())
        }

        (Some(x), Some(y)) => variant_weight(x).cmp(&variant_weight(y)),
    }
}
