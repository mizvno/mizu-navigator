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
        // Int arithmetic stays exact and unscaled.
        (BinOp::Add, Value::Int(l), Value::Int(r)) => l
            .checked_add(r)
            .map(Value::Int)
            .ok_or(MizuError::IntegerOverflow),
        (BinOp::Sub, Value::Int(l), Value::Int(r)) => l
            .checked_sub(r)
            .map(Value::Int)
            .ok_or(MizuError::IntegerOverflow),
        (BinOp::Mul, Value::Int(l), Value::Int(r)) => l
            .checked_mul(r)
            .map(Value::Int)
            .ok_or(MizuError::IntegerOverflow),
        // Division always yields a `Decimal`, so `7 / 2` is `3.5` and not `3`:
        // whether an expression truncates must not depend on how its operands
        // happened to be spelled. Computed directly in `i128` rather than by
        // upscaling both sides first — `l * DECIMAL_SCALE` would overflow for
        // any |l| above ~9.2e10, making `1000000000000 / 2` an error.
        (BinOp::Div, Value::Int(l), Value::Int(r)) => {
            if r == 0 {
                return Err(MizuError::DivisionByZero);
            }
            let quotient = (l as i128 * crate::core::types::DECIMAL_SCALE as i128) / (r as i128);
            i64::try_from(quotient)
                .map(Value::Decimal)
                .map_err(|_| MizuError::IntegerOverflow)
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

        // Mixed Int/Decimal comparison — evaluated in `i128` so that upscaling
        // cannot overflow. These must precede the arithmetic promotion below:
        // going through it, comparing an out-of-fixed-point-range integer
        // (`9223372036854775807 > 1.5`) would raise `IntegerOverflow` instead
        // of answering the perfectly well-defined question that was asked.
        (
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge,
            Value::Int(l),
            Value::Decimal(r),
        ) => {
            let scale = crate::core::types::DECIMAL_SCALE as i128;
            Ok(Value::Bool(compare_op(op, (l as i128) * scale, r as i128)))
        }
        (
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge,
            Value::Decimal(l),
            Value::Int(r),
        ) => {
            let scale = crate::core::types::DECIMAL_SCALE as i128;
            Ok(Value::Bool(compare_op(op, l as i128, (r as i128) * scale)))
        }

        // Mixed Int/Decimal arithmetic (promote Int to Decimal). Here an
        // overflow *is* the right answer: the result genuinely has no
        // fixed-point representation.
        (op, Value::Int(l), Value::Decimal(r)) => {
            let scaled_l = l
                .checked_mul(crate::core::types::DECIMAL_SCALE)
                .ok_or(MizuError::IntegerOverflow)?;
            apply_binop(
                op,
                Value::Decimal(scaled_l),
                Value::Decimal(r),
                instruction_count,
                max_instructions,
            )
        }
        (op, Value::Decimal(l), Value::Int(r)) => {
            let scaled_r = r
                .checked_mul(crate::core::types::DECIMAL_SCALE)
                .ok_or(MizuError::IntegerOverflow)?;
            apply_binop(
                op,
                Value::Decimal(l),
                Value::Decimal(scaled_r),
                instruction_count,
                max_instructions,
            )
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

/// Applies one of the six comparison operators to an already-normalised
/// numeric pair.
///
/// Only ever called with a comparison `op` — the arithmetic operators are
/// matched out before this is reached — so the fallback arm is unreachable
/// rather than a silent default.
#[inline]
fn compare_op(op: &BinOp, l: i128, r: i128) -> bool {
    match op {
        BinOp::Eq => l == r,
        BinOp::Ne => l != r,
        BinOp::Lt => l < r,
        BinOp::Gt => l > r,
        BinOp::Le => l <= r,
        BinOp::Ge => l >= r,
        _ => false,
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
                for (found_field, (exp_name, exp_type)) in fields.iter().zip(expected_fields.iter())
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
// # Why there are no Kani harnesses in this module
//
// There were three, asserting that `check_type` accepts `Decimal` for `Num`,
// `Bool` for `Bool`, and `String` for `Str`. They verified successfully and
// quickly, and proved essentially nothing: each one restates the match arm
// immediately above it. The only symbolic input, the `i64` payload, cannot
// influence the verdict — `check_type` matches on the variant, never on the
// value. A harness whose proof obligation is a restatement of the code it
// checks costs maintenance and buys no assurance, so they were removed rather
// than kept for the harness count.
//
// The parts of this module actually worth verifying are out of CBMC's reach,
// and not for want of trying. `Value` and `ValueType` are self-referential
// (`Value::List(Arc<Vec<Value>>)`, `ValueType::List(Box<ValueType>)`), and
// bisecting against a minimal throwaway crate isolated three independent
// triggers, any one of which is enough to hang verification:
//
//   1. `kani::any()`-driven branching that selects among a self-referential
//      enum's *own* variants hangs even when restricted to leaf variants.
//   2. Any path calling `.to_string()` on a `ValueType` — i.e. `check_type`'s
//      *error* path, which formats `expected` into the returned
//      `MizuError::TypeError`. `ValueType`'s `Display` is recursive-shaped,
//      and CBMC's cost for it holds regardless of the concrete variant.
//   3. Constructing two or more instances of a recursive-payload type in one
//      harness (independent of both of the above).
//
// So `check_type`'s error paths, its `List`/`Record`/`Nullable` recursion, and
// `parser::typecheck`'s `infer` are all unattempted here — including, for
// `infer`, a fully-concrete zero-`kani::any()` harness that still failed to
// complete after 3+ minutes. Reaching them needs either a `cfg(kani)`-only
// non-recursive shadow representation of `Value`/`ValueType`/`Expr`, engineered
// and validated on its own, or the project's Lean 4 development (`formal/`),
// which does structural induction over recursive types natively — exactly what
// bounded model checking fights against here. T4 (Type Soundness) stays "Open"
// in `RESULTS.md` pending that work.
//
// In the meantime the practical coverage for this module is the property tests
// in `parser::logic::tests`, which run natively and so have none of these
// limits.
