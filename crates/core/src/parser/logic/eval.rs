//! The expression evaluator and binary-op semantics.

use rustc_hash::FxHashMap;

use crate::core::errors::MizuError;
use crate::core::types::{Symbol, Value, VariableStore};

use super::ast::{Action, BinOp, Expr, MizuFunction, ValueType};
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
    store.state_machine.instruction_count = 0;
    match action {
        Action::Assign { target, expr } => {
            if let Some(sym) = store.interner.get(target)
                && store.state_machine.computed_var_syms.contains(&sym)
            {
                return Err(MizuError::ExecutionError(format!(
                    "cannot assign to computed variable `{target}`"
                )));
            }
            let result = store
                .state_machine
                .evaluate(expr, 0, functions, &store.interner)?;
            store.set(target, result);
            Ok(true)
        }
        Action::Eval(expr) => {
            store
                .state_machine
                .evaluate(expr, 0, functions, &store.interner)?;
            Ok(false)
        }
        Action::Navigate { url } => {
            let eval_url = store
                .state_machine
                .evaluate(url, 0, functions, &store.interner)?;
            let url_str = match eval_url {
                Value::String(s) => s.to_string(),
                _ => {
                    return Err(MizuError::ExecutionError(
                        "Navigate URL must evaluate to a string".to_string(),
                    ));
                }
            };

            store
                .state_machine
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
        } => {
            // Evaluate optional payload and path_param expressions.
            let payload_val = if let Some(p) = payload {
                Some(
                    store
                        .state_machine
                        .evaluate(p, 0, functions, &store.interner)?,
                )
            } else {
                None
            };
            let path_param_str = if let Some(pp) = path_param {
                let v = store
                    .state_machine
                    .evaluate(pp, 0, functions, &store.interner)?;
                let s = match v {
                    Value::String(s) => s.to_string(),
                    Value::Int(n) => n.to_string(),
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
            let target_variable = store.interner.get_or_intern(target_var);
            store.state_machine.accumulated_actions.push(
                crate::messages::RuntimeAction::NetworkCall {
                    method: method.clone(),
                    endpoint_symbol: alias_sym.0,
                    payload: payload_val,
                    path_param: path_param_str,
                    target_variable,
                },
            );
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
    store: &mut VariableStore,
    functions: &FxHashMap<Symbol, MizuFunction>,
    frame_pointer: usize,
) -> Result<Value, MizuError> {
    store.state_machine.instruction_count = 0;
    store
        .state_machine
        .evaluate(expr, frame_pointer, functions, &store.interner)
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
) -> Result<Value, MizuError> {
    match (op, lv, rv) {
        // Num operations — Int×Int uses checked arithmetic to catch overflow in release builds.
        (BinOp::Add, Value::Int(l), Value::Int(r)) => l
            .checked_add(r)
            .map(Value::Int)
            .ok_or_else(|| MizuError::ExecutionError("integer overflow".to_owned())),

        (BinOp::Sub, Value::Int(l), Value::Int(r)) => l
            .checked_sub(r)
            .map(Value::Int)
            .ok_or_else(|| MizuError::ExecutionError("integer overflow".to_owned())),

        (BinOp::Mul, Value::Int(l), Value::Int(r)) => l
            .checked_mul(r)
            .map(|product| product / crate::core::types::DECIMAL_SCALE)
            .map(Value::Int)
            .ok_or_else(|| MizuError::ExecutionError("integer overflow".to_owned())),

        (BinOp::Div, Value::Int(l), Value::Int(r)) => {
            if r == 0 {
                return Err(MizuError::DivisionByZero);
            }
            let numerator = l.saturating_mul(crate::core::types::DECIMAL_SCALE);
            numerator
                .checked_div(r)
                .map(Value::Int)
                .ok_or_else(|| MizuError::ExecutionError("integer overflow".to_owned()))
        },

        // String concatenation via `+`: charge the combined length before
        // allocating, mirroring filter/count/sort — otherwise a chain of
        // nested `let`s doubling a string bypasses MAX_INSTRUCTIONS entirely
        // (each `+` is one AST node regardless of operand size) while real
        // allocation cost grows exponentially with nesting depth.
        (BinOp::Add, Value::String(l), Value::String(r)) => {
            let concat_cost = (l.len() as u64).saturating_add(r.len() as u64);
            *instruction_count = instruction_count.saturating_add(concat_cost);
            if *instruction_count > *crate::core::types::MAX_INSTRUCTIONS {
                return Err(MizuError::Timeout);
            }
            Ok(Value::String(std::sync::Arc::from(
                format!("{l}{r}").as_str(),
            )))
        }

        // Equality — works across numerics and strings/bools
        (BinOp::Eq, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::String(l), Value::String(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Null, Value::Null) => Ok(Value::Bool(true)),
        (BinOp::Eq, Value::Null, _) => Ok(Value::Bool(false)),
        (BinOp::Eq, _, Value::Null) => Ok(Value::Bool(false)),

        // Inequality — mirrors equality
        (BinOp::Ne, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::String(l), Value::String(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Null, Value::Null) => Ok(Value::Bool(false)),
        (BinOp::Ne, Value::Null, _) => Ok(Value::Bool(true)),
        (BinOp::Ne, _, Value::Null) => Ok(Value::Bool(true)),

        // Ordering — numeric types only
        (BinOp::Lt, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l < r)),
        (BinOp::Gt, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l > r)),
        (BinOp::Le, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l <= r)),
        (BinOp::Ge, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l >= r)),

        // Logical AND / OR — bool operands only
        (BinOp::And, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l && r)),
        (BinOp::Or, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l || r)),

        // Type mismatch
        (_, l, _) => Err(MizuError::TypeError {
            expected: "compatible operand types".to_string(),
            found: type_name(&l),
        }),
    }
}

/// Returns the Mizu type-name string for a runtime value.
pub(crate) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "num",
        Value::String(_) => "string",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Record(_) => "record",
        Value::Null => "null",
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
                for ((found_name, found_val), (exp_name, exp_type)) in fields.iter().zip(expected_fields.iter()) {
                    if found_name.as_ref() != exp_name.as_ref() {
                        all_ok = false;
                        break;
                    }
                    if check_type(found_val, exp_type, func_name, param_name).is_err() {
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
            expected: expected.to_string(),
            found: type_name(val),
        });
    }
    let _ = (func_name, param_name);
    Ok(())
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    fn any_value_type(depth: usize) -> ValueType {
        if depth == 0 {
            match kani::any::<u8>() % 3 {
                0 => ValueType::Num,
                1 => ValueType::Str,
                _ => ValueType::Bool,
            }
        } else {
            match kani::any::<u8>() % 6 {
                0 => ValueType::Num,
                1 => ValueType::Str,
                2 => ValueType::Bool,
                3 => ValueType::List(Box::new(any_value_type(depth - 1))),
                4 => {
                    let mut fields = Vec::new();
                    if kani::any::<bool>() {
                        fields.push((crate::core::types::Symbol(kani::any()), any_value_type(depth - 1)));
                    }
                    ValueType::Record(fields)
                }
                _ => ValueType::Nullable(Box::new(any_value_type(depth - 1))),
            }
        }
    }

    fn any_value(depth: usize) -> Value {
        if depth == 0 {
            match kani::any::<u8>() % 4 {
                0 => Value::Null,
                1 => Value::Bool(kani::any()),
                2 => Value::Int(kani::any()),
                _ => Value::String(String::new()),
            }
        } else {
            match kani::any::<u8>() % 6 {
                0 => Value::Null,
                1 => Value::Bool(kani::any()),
                2 => Value::Int(kani::any()),
                3 => Value::String(String::new()),
                4 => {
                    let mut list = Vec::new();
                    if kani::any::<bool>() {
                        list.push(any_value(depth - 1));
                    }
                    Value::List(list)
                }
                _ => {
                    let mut fields = Vec::new();
                    if kani::any::<bool>() {
                        fields.push((crate::core::types::Symbol(kani::any()), any_value(depth - 1)));
                    }
                    Value::Record(fields)
                }
            }
        }
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn check_type_does_not_panic() {
        let val = any_value(2);
        let ty = any_value_type(2);
        let _ = check_type(&val, &ty, "f", "p");
    }
}
