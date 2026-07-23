//! # `typecheck` — Static Load-Time Type Checker
//!
//! Enforces Phase B type constraints on logic expressions at document load time.
//!
//! ## Security Posture
//!
//! This pass and `flow.rs`'s taint propagation are independent lattices over the
//! same `Expr` tree — neither consumes the other's output. `typecheck.rs`
//! validates shape and type consistency, while `flow.rs` validates information
//! taint.
//!
//! ## Escape Hatch
//!
//! Unannotated parameters yield an implicit "dynamic" type (`None` in `Env`).
//! Operations on this type never fail statically in Phase B. This ensures
//! full backward compatibility with unannotated `.mizu` documents.
//! // REMOVED IN PHASE D

use crate::core::errors::MizuError;
use crate::core::types::Symbol;
use crate::parser::layout::{EventBlock, MizuNode};
use crate::parser::logic::{
    Action, BinOp, ComputedBinding, Expr, MizuFunction, RootTimer, ValueType,
};
use rustc_hash::FxHashMap;

type Env = FxHashMap<Symbol, Option<ValueType>>;

/// Run the static type checker over the entire document logic.
pub fn check_types(
    dom: &ego_tree::Tree<MizuNode>,
    timers: &[RootTimer],
    functions: &FxHashMap<Symbol, MizuFunction>,
    comps: &[ComputedBinding],
    interner: &crate::core::types::StringInterner,
) -> Result<(), MizuError> {
    let mut global_env = Env::default();

    for comp in comps {
        let ty = infer(&comp.expr, &global_env, functions, interner)?;
        global_env.insert(comp.name, ty);
    }

    for func in functions.values() {
        let mut local_env = global_env.clone();
        for (sym, ty_ann) in &func.params {
            local_env.insert(*sym, ty_ann.clone());
        }
        infer(&func.body, &local_env, functions, interner)?;
    }

    for node in dom.nodes() {
        for block in node.value().events.values() {
            match block {
                EventBlock::Click { action } | EventBlock::Submit { action } => {
                    check_action(action, &global_env, functions, interner)?;
                }
            }
        }
    }

    for timer in timers {
        check_action(&timer.action, &global_env, functions, interner)?;
    }

    Ok(())
}

fn check_action(
    action: &Action,
    env: &Env,
    functions: &FxHashMap<Symbol, MizuFunction>,
    interner: &crate::core::types::StringInterner,
) -> Result<(), MizuError> {
    match action {
        Action::Eval(expr) | Action::Assign { expr, .. } | Action::Navigate { url: expr } => {
            infer(expr, env, functions, interner)?;
        }
        Action::NetworkCall {
            payload,
            path_param,
            ..
        } => {
            if let Some(p) = payload {
                infer(p, env, functions, interner)?;
            }
            if let Some(p) = path_param {
                infer(p, env, functions, interner)?;
            }
        }
    }
    Ok(())
}

fn infer(
    expr: &Expr,
    env: &Env,
    functions: &FxHashMap<Symbol, MizuFunction>,
    interner: &crate::core::types::StringInterner,
) -> Result<Option<ValueType>, MizuError> {
    match expr {
        Expr::Literal(val) => match val {
            crate::core::types::Value::Int(_) => {
                Ok(Some(ValueType::Num))
            }
            crate::core::types::Value::String(_) => Ok(Some(ValueType::Str)),
            crate::core::types::Value::Bool(_) => Ok(Some(ValueType::Bool)),
            crate::core::types::Value::List(_) => Ok(None),
            crate::core::types::Value::Record(_) => Ok(None),
            crate::core::types::Value::Null => Ok(Some(ValueType::Nullable(Box::new(ValueType::Num)))),
        },
        Expr::Variable(sym) => {
            if let Some(ty) = env.get(sym) {
                Ok(ty.clone())
            } else {
                Ok(None)
            }
        }
        Expr::BinaryOp { left, op, right } => {
            infer(left, env, functions, interner)?;
            infer(right, env, functions, interner)?;
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => Ok(Some(ValueType::Num)),
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge
                | BinOp::And | BinOp::Or => Ok(Some(ValueType::Bool)),
            }
        }
        Expr::Let { name, value, body } => {
            let val_ty = infer(value, env, functions, interner)?;
            let mut local_env = env.clone();
            local_env.insert(*name, val_ty);
            infer(body, &local_env, functions, interner)
        }
        Expr::Not(inner) => {
            infer(inner, env, functions, interner)?;
            Ok(Some(ValueType::Bool))
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            infer(condition, env, functions, interner)?;
            let then_ty = infer(then_expr, env, functions, interner)?;
            let else_ty = infer(else_expr, env, functions, interner)?;
            if then_ty == else_ty {
                Ok(then_ty)
            } else {
                Ok(None)
            }
        }
        Expr::FieldAccess { base, field } => {
            let base_ty = infer(base, env, functions, interner)?;
            match base_ty {
                Some(ValueType::Record(fields)) => {
                    for (name, ty) in fields.iter() {
                        if name.as_ref() == field.as_ref() {
                            return Ok(Some(ty.clone()));
                        }
                    }
                    Err(MizuError::StaticTypeError(format!(
                        "field `{}` not found in record type",
                        field
                    )))
                }
                Some(other) => Err(MizuError::StaticTypeError(format!(
                    "cannot access field `{}` on type `{}`",
                    field, other
                ))),
                None => Ok(None),
            }
        }
        Expr::FunctionCall { name, args } => {
            let func_name = interner.resolve(*name).unwrap_or("");
            if func_name == "filter" && args.len() == 3 {
                let list_ty = infer(&args[0], env, functions, interner)?;
                infer(&args[1], env, functions, interner)?;
                infer(&args[2], env, functions, interner)?;
                match list_ty {
                    Some(ValueType::List(inner)) => Ok(Some(ValueType::List(inner))),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "filter expects a list, got `{}`",
                        other
                    ))),
                    None => Ok(None),
                }
            } else if func_name == "count" && args.len() == 3 {
                let list_ty = infer(&args[0], env, functions, interner)?;
                infer(&args[1], env, functions, interner)?;
                infer(&args[2], env, functions, interner)?;
                match list_ty {
                    Some(ValueType::List(_)) | None => Ok(Some(ValueType::Num)),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "count expects a list, got `{}`",
                        other
                    ))),
                }
            } else if func_name == "sort" && args.len() == 3 {
                let list_ty = infer(&args[0], env, functions, interner)?;
                infer(&args[1], env, functions, interner)?;
                infer(&args[2], env, functions, interner)?;
                match list_ty {
                    Some(ValueType::List(inner)) => Ok(Some(ValueType::List(inner))),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "sort expects a list, got `{}`",
                        other
                    ))),
                    None => Ok(None),
                }
            } else if let Some(func) = functions.get(name) {
                if args.len() != func.params.len() {
                    return Err(MizuError::StaticTypeError(format!(
                        "function `{}` expects {} arguments, got {}",
                        func_name,
                        func.params.len(),
                        args.len()
                    )));
                }
                for arg in args.iter() {
                    infer(arg, env, functions, interner)?;
                }
                // We do not infer return types of functions in Phase B yet,
                // or we could if we memoize/check them. For now, functions return dynamic.
                Ok(None)
            } else {
                // Builtin like 'download' or undefined function
                for arg in args {
                    infer(arg, env, functions, interner)?;
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::StringInterner;
    use crate::parser::logic::parse_logic;


    // Helper to parse logic string and typecheck the functions
    fn check_logic_string(src: &str) -> Result<(), MizuError> {
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner)?;
        let dom = ego_tree::Tree::new(MizuNode {
            primitive: crate::parser::layout::Primitive::Box,
            attributes: std::collections::HashMap::new(),
            events: std::collections::HashMap::new(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        });
        check_types(&dom, &[], &fns, &[], &interner)
    }

    #[test]
    fn annotated_param_accepted() {
        let src = "f(x: num) : x + 1";
        assert!(check_logic_string(src).is_ok());
    }

    #[test]
    fn unannotated_param_accepted() {
        let src = "f(x) : x.some_field + 1";
        assert!(check_logic_string(src).is_ok());
    }

    #[test]
    fn missing_field_on_record_rejected() {
        let src = "f(r: record{a: num}) : r.b";
        let err = check_logic_string(src).unwrap_err();
        assert!(matches!(err, MizuError::StaticTypeError(_)));
        if let MizuError::StaticTypeError(msg) = err {
            assert!(msg.contains("field `b` not found"));
        }
    }

    #[test]
    fn field_on_non_record_rejected() {
        let src = "f(x: num) : x.field";
        let err = check_logic_string(src).unwrap_err();
        assert!(matches!(err, MizuError::StaticTypeError(_)));
    }

    #[test]
    fn mixed_params_accepted_and_checked() {
        let src = "f(x: num, y) : x + y";
        assert!(check_logic_string(src).is_ok());
        
        // x is a num, so accessing a field on it fails
        let src2 = "f(x: num, y) : x.f + y";
        assert!(check_logic_string(src2).is_err());
    }
}
