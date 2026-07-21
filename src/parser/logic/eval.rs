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
                .push(crate::network::RuntimeAction::Navigate { url: url_str });
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
                crate::network::RuntimeAction::NetworkCall {
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
            expected: "compatible operand types",
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

/// Verifies that a runtime argument value matches the declared parameter type.
///
/// `None` means the parameter has no type annotation — any value is accepted.
pub(crate) fn check_type(
    val: &Value,
    expected: Option<&ValueType>,
    func_name: &str,
    param_name: &str,
) -> Result<(), MizuError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let ok = matches!(
        (val, expected),
        (Value::Int(_), ValueType::Num)
            
            | (Value::String(_), ValueType::Str)
            | (Value::Bool(_), ValueType::Bool)
            | (Value::List(_), ValueType::List)
    );
    if !ok {
        return Err(MizuError::TypeError {
            expected: expected.as_str(),
            found: type_name(val),
        });
    }
    let _ = (func_name, param_name);
    Ok(())
}
