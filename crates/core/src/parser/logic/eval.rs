//! The expression evaluator and binary-op semantics.

use rustc_hash::FxHashMap;

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value, VariableStore};

use super::ast::{Action, BinOp, Expr, ExprArena, MizuFunction, ValueType};
use super::parse::path_param_ok;

/// Executes a compiled [`Action`] against the provided variable store.
///
/// Returns `true` if the action was an assignment (store mutated), `false` otherwise.
pub fn execute_action(
    action: &Action,
    store: &mut VariableStore,
    functions: &FxHashMap<Symbol, MizuFunction>,
) -> Result<bool, MizuError> {
    // Reset the instruction counter so the budget applies per action, not cumulatively.
    store.evaluator.instruction_count = 0;
    match action {
        Action::Assign { target, expr } => {
            if let Some(sym) = store.interner.get(target)
                && store.evaluator.computed_var_syms.contains(&sym)
            {
                return Err(MizuError::ExecutionError(format!(
                    "cannot assign to computed variable `{target}`"
                )));
            }
            let result = store.evaluator.evaluate(
                expr.root(),
                0,
                functions,
                &store.interner,
                &expr.arena,
            )?;
            store.set_runtime(target, result);
            Ok(true)
        }
        Action::Eval(expr) => {
            store
                .evaluator
                .evaluate(expr.root(), 0, functions, &store.interner, &expr.arena)?;
            Ok(false)
        }
        Action::Navigate { url } => {
            let eval_url =
                store
                    .evaluator
                    .evaluate(url.root(), 0, functions, &store.interner, &url.arena)?;
            let url_str = match eval_url {
                Value::String(ref s) => s.to_string(),
                _ => {
                    return Err(MizuError::ExecutionError(
                        "Navigate URL must evaluate to a string".to_string(),
                    ));
                }
            };

            store
                .evaluator
                .accumulated_actions
                .push(crate::messages::RuntimeAction::Navigate { url: url_str });
            Ok(true)
        }
        Action::NetworkCall {
            method,
            alias_sym,
            payload,
            path_param,
            target_var,
            format,
            headers,
        } => {
            // Evaluate optional payload and path_param expressions.
            let payload_val = if let Some(p) = payload {
                Some(
                    store
                        .evaluator
                        .evaluate(p.root(), 0, functions, &store.interner, &p.arena)?,
                )
            } else {
                None
            };
            let path_param_str = if let Some(pp) = path_param {
                let v = store.evaluator.evaluate(
                    pp.root(),
                    0,
                    functions,
                    &store.interner,
                    &pp.arena,
                )?;
                let s = match v {
                    Value::String(ref s) => s.to_string(),
                    Value::Decimal(n) => n.to_string(),
                    _ => {
                        return Err(MizuError::ExecutionError(
                            "path_param must be a string or number".to_string(),
                        ));
                    }
                };
                if !path_param_ok(&s) {
                    return Err(MizuError::ExecutionError(
                        "path_param must be a single path segment".to_string(),
                    ));
                }
                Some(s)
            } else {
                None
            };
            let target_variable = store.interner.get(target_var).ok_or_else(|| {
                MizuError::ExecutionError(format!(
                    "Network target variable `{}` was not declared in the logic block",
                    target_var
                ))
            })?;
            // Custom header values are runtime expressions (unlike their
            // names, fixed at parse time) — evaluate each here, same as the
            // payload.
            let mut header_values = Vec::with_capacity(headers.len());
            for (name, expr) in headers {
                let v = store.evaluator.evaluate(
                    expr.root(),
                    0,
                    functions,
                    &store.interner,
                    &expr.arena,
                )?;
                header_values.push((name.clone(), v));
            }
            store
                .evaluator
                .accumulated_actions
                .push(crate::messages::RuntimeAction::NetworkCall {
                    method: method.clone(),
                    endpoint_symbol: alias_sym.0,
                    payload: payload_val,
                    path_param: path_param_str,
                    target_variable,
                    format: *format,
                    headers: header_values,
                });
            Ok(true)
        }
    }
}

/// Evaluates a Mizu expression to a concrete [`Value`].
///
/// Resets `instruction_count` to `0` before delegating so the per-expression
/// budget is enforced from scratch on each call.
pub fn evaluate(
    expr: &Expr,
    arena: &ExprArena,
    store: &mut VariableStore,
    functions: &FxHashMap<Symbol, MizuFunction>,
    frame_pointer: usize,
) -> Result<Value, MizuError> {
    store.evaluator.instruction_count = 0;
    store
        .evaluator
        .evaluate(expr, frame_pointer, functions, &store.interner, arena)
}

/// Applies a binary arithmetic operator to two already-evaluated values.
///
/// `instruction_count` is threaded through so string concatenation — the one
/// case here whose real cost (an O(len(l)+len(r)) allocation and copy) is not
/// a flat unit of work — can charge proportionally to its actual size before
/// performing the allocation, the same discipline `filter`/`count`/`sort`
/// already apply to their native passes in `types.rs`.
pub(crate) fn apply_binop(
    op: &BinOp,
    lv: Value,
    rv: Value,
    instruction_count: &mut u64,
    max_instructions: u64,
) -> Result<Value, MizuError> {
    match (op, lv, rv) {
        // Int / Int -> Int (integer division)
        (BinOp::Add, Value::Int(l), Value::Int(r)) => l.checked_add(r).map(Value::Int).ok_or(MizuError::IntegerOverflow),
        (BinOp::Sub, Value::Int(l), Value::Int(r)) => l.checked_sub(r).map(Value::Int).ok_or(MizuError::IntegerOverflow),
        (BinOp::Mul, Value::Int(l), Value::Int(r)) => l.checked_mul(r).map(Value::Int).ok_or(MizuError::IntegerOverflow),
        (BinOp::Div, Value::Int(l), Value::Int(r)) => {
            let scaled_l = l.checked_mul(crate::core::types::DECIMAL_SCALE).ok_or(MizuError::IntegerOverflow)?;
            let scaled_r = r.checked_mul(crate::core::types::DECIMAL_SCALE).ok_or(MizuError::IntegerOverflow)?;
            apply_binop(&BinOp::Div, Value::Decimal(scaled_l), Value::Decimal(scaled_r), instruction_count, max_instructions)
        }

        // Decimal operations
        (BinOp::Add, Value::Decimal(l), Value::Decimal(r)) => l
            .checked_add(r)
            .map(Value::Decimal)
            .ok_or(MizuError::IntegerOverflow),

        (BinOp::Sub, Value::Decimal(l), Value::Decimal(r)) => l
            .checked_sub(r)
            .map(Value::Decimal)
            .ok_or(MizuError::IntegerOverflow),

        (BinOp::Mul, Value::Decimal(l), Value::Decimal(r)) => {
            let product = (l as i128) * (r as i128);
            let scaled = product / (crate::core::types::DECIMAL_SCALE as i128);
            i64::try_from(scaled)
                .map(Value::Decimal)
                .map_err(|_| MizuError::IntegerOverflow)
        }

        (BinOp::Div, Value::Decimal(l), Value::Decimal(r)) => {
            if r == 0 {
                return Err(MizuError::DivisionByZero);
            }
            let numerator = (l as i128) * (crate::core::types::DECIMAL_SCALE as i128);
            let quotient = numerator / (r as i128);
            i64::try_from(quotient)
                .map(Value::Decimal)
                .map_err(|_| MizuError::IntegerOverflow)
        }

        // Mixed Int/Decimal operations (promote Int to Decimal)
        (op, Value::Int(l), Value::Decimal(r)) => {
            let scaled_l = l.checked_mul(crate::core::types::DECIMAL_SCALE).ok_or(MizuError::IntegerOverflow)?;
            apply_binop(op, Value::Decimal(scaled_l), Value::Decimal(r), instruction_count, max_instructions)
        }
        (op, Value::Decimal(l), Value::Int(r)) => {
            let scaled_r = r.checked_mul(crate::core::types::DECIMAL_SCALE).ok_or(MizuError::IntegerOverflow)?;
            apply_binop(op, Value::Decimal(l), Value::Decimal(scaled_r), instruction_count, max_instructions)
        }

        // String concatenation via `+`: charge the combined length before
        // allocating, mirroring filter/count/sort — otherwise a chain of
        // nested `let`s doubling a string bypasses MAX_INSTRUCTIONS entirely
        // (each `+` is one AST node regardless of operand size) while real
        // allocation cost grows exponentially with nesting depth.
        (BinOp::Add, Value::String(ref l), Value::String(ref r)) => {
            let concat_cost = (l.len() as u64).saturating_add(r.len() as u64);
            *instruction_count = instruction_count.saturating_add(concat_cost);
            if *instruction_count > max_instructions {
                return Err(MizuError::Timeout);
            }
            let mut buf = String::with_capacity(l.len() + r.len());
            buf.push_str(&l);
            buf.push_str(&r);
            Ok(Value::String(std::sync::Arc::from(buf)))
        }

        // Equality — works across numerics and strings/bools
        (BinOp::Eq, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Decimal(l), Value::Decimal(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::String(ref l), Value::String(ref r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Null, Value::Null) => Ok(Value::Bool(true)),
        (BinOp::Eq, Value::Null, _) => Ok(Value::Bool(false)),
        (BinOp::Eq, _, Value::Null) => Ok(Value::Bool(false)),

        // Inequality — mirrors equality
        (BinOp::Ne, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Decimal(l), Value::Decimal(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::String(ref l), Value::String(ref r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Null, Value::Null) => Ok(Value::Bool(false)),
        (BinOp::Ne, Value::Null, _) => Ok(Value::Bool(true)),
        (BinOp::Ne, _, Value::Null) => Ok(Value::Bool(true)),

        // Ordering
        (BinOp::Lt, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l < r)),
        (BinOp::Gt, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l > r)),
        (BinOp::Le, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l <= r)),
        (BinOp::Ge, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l >= r)),
        (BinOp::Lt, Value::Decimal(l), Value::Decimal(r)) => Ok(Value::Bool(l < r)),
        (BinOp::Gt, Value::Decimal(l), Value::Decimal(r)) => Ok(Value::Bool(l > r)),
        (BinOp::Le, Value::Decimal(l), Value::Decimal(r)) => Ok(Value::Bool(l <= r)),
        (BinOp::Ge, Value::Decimal(l), Value::Decimal(r)) => Ok(Value::Bool(l >= r)),
        (BinOp::Lt, Value::String(ref l), Value::String(ref r)) => Ok(Value::Bool(l < r)),
        (BinOp::Gt, Value::String(ref l), Value::String(ref r)) => Ok(Value::Bool(l > r)),
        (BinOp::Le, Value::String(ref l), Value::String(ref r)) => Ok(Value::Bool(l <= r)),
        (BinOp::Ge, Value::String(ref l), Value::String(ref r)) => Ok(Value::Bool(l >= r)),

        // Logical AND / OR — bool operands only
        (BinOp::And, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l && r)),
        (BinOp::Or, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l || r)),

        // Type mismatch
        (_, l, _) => Err(MizuError::TypeError {
            expected: Box::new("compatible operand types".to_string()),
            found: type_name(&l),
        }),
    }
}

/// Returns the Mizu type-name string for a runtime value.
pub(crate) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "num",
        Value::Decimal(_) => "num",
        Value::String(_) => "string",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Record(_) => "record",
        Value::Null => "null",
        Value::FileHandle(_) => "file",
    }
}

pub(crate) fn check_type(
    val: &Value,
    expected: &ValueType,
    func_name: &str,
    param_name: &str,
) -> Result<(), MizuError> {
    let ok = match (val, expected) {
        (Value::Int(_), ValueType::Num) => true,
        (Value::Decimal(_), ValueType::Num) => true,
        (Value::String(_), ValueType::Str) => true,
        (Value::Bool(_), ValueType::Bool) => true,
        (Value::List(items), ValueType::List(inner)) => {
            let mut all_ok = true;
            for item in items.iter() {
                if check_type(item, inner, func_name, param_name).is_err() {
                    all_ok = false;
                    break;
                }
            }
            all_ok
        }
        (Value::Record(fields), ValueType::Record(expected_fields)) => {
            let mut all_ok = true;
            if fields.len() != expected_fields.len() {
                all_ok = false;
            } else {
                for (found_field, (exp_name, exp_type)) in
                    fields.iter().zip(expected_fields.iter())
                {
                    if found_field.key.as_ref() != exp_name.as_ref() {
                        all_ok = false;
                        break;
                    }
                    if check_type(&found_field.value, exp_type, func_name, param_name).is_err() {
                        all_ok = false;
                        break;
                    }
                }
            }
            all_ok
        }
        (Value::Null, ValueType::Nullable(_)) => true,
        (v, ValueType::Nullable(inner)) => check_type(v, inner, func_name, param_name).is_ok(),
        _ => false,
    };
    if !ok {
        return Err(MizuError::TypeError {
            expected: Box::new(expected.to_string()),
            found: type_name(val),
        });
    }
    let _ = (func_name, param_name);
    Ok(())
}

// # Why this module is small, and what it deliberately does not attempt
//
// `Value` and `ValueType` are self-referential enums (`Value::List(Arc<Vec<Value>>)`,
// `ValueType::List(Box<ValueType>)`, etc). An earlier version of this module built
// arbitrary instances with a recursive `any_value(depth)`/`any_value_type(depth)`
// generator that picked among *all* of a type's own variants via `kani::any()`,
// including the recursive ones. That does not scale in CBMC — it failed to
// complete even at `depth == 0`, i.e. even when the generator could only ever
// produce a `Null`/`Bool`/`Int`/`String` leaf, never the recursive payload.
//
// Bisecting against a minimal throwaway crate (outside this repo) isolated the
// triggers precisely, and there turn out to be several independent ones, not one:
//
//   1. `kani::any()`-driven branching that selects among a self-referential
//      enum's *own* variants (e.g. `match kani::any::<u8>() % N { .. Num, .. Str
//      .. }` where the match's result type is the recursive enum itself) hangs,
//      even restricted to non-recursive leaf variants.
//   2. Any code path that calls `.to_string()` on a `ValueType` — i.e.
//      `check_type`'s *error* path, which formats `expected` into the returned
//      `MizuError::TypeError` — hangs. Confirmed with a clean, isolated 4-minute
//      run that never completed. `ValueType`'s `Display` impl is itself
//      recursive-shaped (it has match arms that recurse into nested `List`/
//      `Record`/`Nullable` payloads), and CBMC's cost for it appears to hold
//      regardless of which concrete variant is actually being formatted.
//   3. Constructing two or more separate instances of a recursive-payload type
//      in the same harness (confirmed independent of both of the above).
//
// Given that, only harnesses with a *fixed* (compile-time-known) top-level
// shape, a *single* instance of the recursive type, symbolic content restricted
// to *primitives* (`bool`/`i64`), and a code path that stays on `check_type`'s
// success side (never touches the `.to_string()` error path) verify in well
// under a second. The three below are exactly that — real, useful, and fast.
//
// What this does **not** cover: `check_type`'s error paths, its `List`/`Record`/
// `Nullable` recursion cases, and anything in `parser::typecheck`'s `infer` are
// all left unattempted here. This is not a gap left by oversight: every harness
// attempted for those (including, for `infer`, the *original*, fully-concrete,
// zero-`kani::any()` harness that predates today's investigation) failed to
// complete even after 3+ minutes in isolation. Getting there would need either
// a substantially different verification strategy for `Value`/`ValueType`/`Expr`
// (e.g. a `cfg(kani)`-only non-recursive shadow representation, engineered and
// validated on its own), or is simply better served by the project's existing
// Lean 4 development (`formal/`), which does structural induction over
// recursive types natively — exactly what CBMC's bounded model checking is
// fighting against here. T4 (Type Soundness) stays "Open" in `RESULTS.md`
// pending that larger piece of work; these three harnesses are a real but
// modest down payment on it, not a substitute for it.
#[cfg(kani)]
mod kani_proofs {
    use super::*;
    use std::sync::Arc;

    #[kani::proof]
    fn check_type_int_matches_num() {
        let n: i64 = kani::any();
        assert!(check_type(&Value::Decimal(n), &ValueType::Num, "f", "p").is_ok());
    }

    #[kani::proof]
    fn check_type_bool_matches_bool() {
        let b: bool = kani::any();
        assert!(check_type(&Value::Bool(b), &ValueType::Bool, "f", "p").is_ok());
    }

    #[kani::proof]
    fn check_type_string_matches_str() {
        let val = Value::String(Arc::from("x"));
        assert!(check_type(&val, &ValueType::Str, "f", "p").is_ok());
    }
}
