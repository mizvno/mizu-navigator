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
            local_env.insert(*sym, Some(ty_ann.clone()));
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

}

#[cfg(kani)]
mod kani_proofs {
    use super::*;
    use crate::core::types::{Value, Symbol};
    use crate::parser::logic::check_type;

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
                        fields.push(("f".into(), any_value_type(depth - 1)));
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
                _ => Value::String(String::new().into()),
            }
        } else {
            match kani::any::<u8>() % 6 {
                0 => Value::Null,
                1 => Value::Bool(kani::any()),
                2 => Value::Int(kani::any()),
                3 => Value::String(String::new().into()),
                4 => {
                    let mut list = Vec::new();
                    if kani::any::<bool>() {
                        list.push(any_value(depth - 1));
                    }
                    Value::List(std::sync::Arc::new(list))
                }
                _ => {
                    let mut fields = Vec::new();
                    if kani::any::<bool>() {
                        fields.push(("f".into(), any_value(depth - 1)));
                    }
                    Value::Record(fields.into())
                }
            }
        }
    }

    fn any_expr(depth: usize) -> Expr {
        if depth == 0 {
            match kani::any::<u8>() % 2 {
                0 => Expr::Literal(any_value(0)),
                _ => Expr::Variable(Symbol(kani::any())),
            }
        } else {
            match kani::any::<u8>() % 8 {
                0 => Expr::Literal(any_value(depth - 1)),
                1 => Expr::Variable(Symbol(kani::any())),
                2 => Expr::BinaryOp {
                    left: Box::new(any_expr(depth - 1)),
                    op: match kani::any::<u8>() % 4 {
                        0 => BinOp::Add,
                        1 => BinOp::Eq,
                        2 => BinOp::And,
                        _ => BinOp::Lt,
                    },
                    right: Box::new(any_expr(depth - 1)),
                },
                3 => Expr::Let {
                    name: Symbol(kani::any()),
                    value: Box::new(any_expr(depth - 1)),
                    body: Box::new(any_expr(depth - 1)),
                },
                4 => Expr::Not(Box::new(any_expr(depth - 1))),
                5 => Expr::IfElse {
                    condition: Box::new(any_expr(depth - 1)),
                    then_expr: Box::new(any_expr(depth - 1)),
                    else_expr: Box::new(any_expr(depth - 1)),
                },
                6 => Expr::FieldAccess {
                    base: Box::new(any_expr(depth - 1)),
                    field: String::new().into(),
                },
                _ => Expr::FunctionCall {
                    name: Symbol(kani::any()),
                    args: {
                        let mut args = Vec::new();
                        if kani::any::<bool>() {
                            args.push(any_expr(depth - 1));
                        }
                        args
                    },
                },
            }
        }
    }

    #[kani::proof]
    #[kani::unwind(3)]
    fn infer_does_not_panic() {
        let expr = any_expr(2);
        let interner = crate::core::types::StringInterner::new();
        let env = Env::default();
        let fns = FxHashMap::default();
        let _ = infer(&expr, &env, &fns, &interner);
    }

    #[kani::proof]
    #[kani::unwind(3)]
    fn static_dynamic_agreement() {
        let val = any_value(2);
        let _ty = any_value_type(2);
        let expr = Expr::Literal(val.clone());
        let interner = crate::core::types::StringInterner::new();
        let env = Env::default();
        let fns = FxHashMap::default();
        
        if let Ok(Some(inferred_ty)) = infer(&expr, &env, &fns, &interner) {
            let result = check_type(&val, &inferred_ty, "f", "p");
            kani::assert(result.is_ok(), "Dynamic check_type rejected what static infer accepted");
        }
    }

    #[kani::proof]
    #[kani::unwind(3)]
    fn model_agreement_02_logic_basics() {
        let mut interner = crate::core::types::StringInterner::new();
        let env = Env::default();
        let fns = FxHashMap::default();

        let sym_double = interner.get_or_intern("double");
        let sym_x = interner.get_or_intern("x");

        let greeting_expr = Expr::Literal(Value::String("Hello, world!".into()));
        let t1 = infer(&greeting_expr, &env, &fns, &interner).unwrap().unwrap();
        kani::assert(matches!(t1, ValueType::Str), "Expected Str");

        let count_expr = Expr::Literal(Value::Int(0));
        let t2 = infer(&count_expr, &env, &fns, &interner).unwrap().unwrap();
        kani::assert(matches!(t2, ValueType::Num), "Expected Num");

        let double_body = Expr::BinaryOp {
            left: Box::new(Expr::Variable(sym_x)),
            op: BinOp::Mul,
            right: Box::new(Expr::Literal(Value::Int(2))),
        };
        let mut double_env = Env::default();
        double_env.insert(sym_x, Some(ValueType::Num));
        
        let t3 = infer(&double_body, &double_env, &fns, &interner).unwrap().unwrap();
        kani::assert(matches!(t3, ValueType::Num), "Expected Num");

        let call_empty = Expr::FunctionCall { name: sym_double, args: vec![] };
        let mut fns_map = FxHashMap::default();
        fns_map.insert(sym_double, MizuFunction {
            params: vec![(sym_x, ValueType::Num)],
            body: double_body.clone(),
        });
        let t4 = infer(&call_empty, &env, &fns_map, &interner);
        kani::assert(t4.is_err(), "Expected type mismatch error for arity");
    }
}
