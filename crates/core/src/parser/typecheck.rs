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
    Action, BinOp, ComputedBinding, Expr, ExprArena, MizuFunction, PayloadFormat, RootTimer,
    ValueType,
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
        let ty = infer(comp.expr.root(), &comp.expr.arena, &global_env, functions, interner)?;
        global_env.insert(comp.name, ty);
    }

    for func in functions.values() {
        let mut local_env = global_env.clone();
        for (sym, ty_ann) in &func.params {
            local_env.insert(*sym, Some(ty_ann.clone()));
        }
        infer(func.body.root(), &func.body.arena, &local_env, functions, interner)?;
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
            infer(expr.root(), &expr.arena, env, functions, interner)?;
        }
        Action::NetworkCall {
            payload,
            path_param,
            format,
            headers,
            ..
        } => {
            if let Some(p) = payload {
                let ty = infer(p.root(), &p.arena, env, functions, interner)?;
                check_payload_format_shape(*format, &ty)?;
            }
            if let Some(p) = path_param {
                infer(p.root(), &p.arena, env, functions, interner)?;
            }
            for (_, value_expr) in headers {
                infer(value_expr.root(), &value_expr.arena, env, functions, interner)?;
            }
        }
    }
    Ok(())
}

/// Statically validates that a `NetworkCall` payload's inferred type is
/// compatible with its declared [`PayloadFormat`], catching an obvious
/// author mistake (e.g. `as text` with a literal integer payload) at load
/// time.
///
/// When `ty` is `None` (dynamic/unknown, per `infer`'s documented
/// convention), this pass stays silent — the runtime check performed during
/// serialisation (`network::worker::payload::serialize_payload`) remains the
/// authoritative, always-enforced gate; this is defense in depth, not a
/// replacement.
fn check_payload_format_shape(
    format: PayloadFormat,
    ty: &Option<ValueType>,
) -> Result<(), MizuError> {
    let Some(ty) = ty else {
        return Ok(());
    };
    match format {
        PayloadFormat::Text => {
            if !matches!(ty, ValueType::Str) {
                return Err(MizuError::StaticTypeError(format!(
                    "network call `as text` payload must be type `string`, found `{ty}`"
                )));
            }
        }
        PayloadFormat::Form => match ty {
            ValueType::Record(fields) => {
                for (name, field_ty) in fields {
                    if matches!(field_ty, ValueType::List(_) | ValueType::Record(_)) {
                        return Err(MizuError::StaticTypeError(format!(
                            "network call `as form` payload field `{name}` must be a \
                             scalar (bool/num/string), found `{field_ty}`"
                        )));
                    }
                }
            }
            _ => {
                return Err(MizuError::StaticTypeError(format!(
                    "network call `as form` payload must be a record, found `{ty}`"
                )));
            }
        },
        PayloadFormat::Multipart => {
            if !matches!(ty, ValueType::Record(_)) {
                return Err(MizuError::StaticTypeError(format!(
                    "network call `as multipart` payload must be a record, found `{ty}`"
                )));
            }
        }
        PayloadFormat::Json | PayloadFormat::Yaml => {}
    }
    Ok(())
}

fn infer(
    expr: &Expr,
    arena: &ExprArena,
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
            // Unreachable in practice: there is no source-level literal
            // syntax that produces a `FileHandle` (only the `type "file"`
            // input's native picker does), but the match must stay
            // exhaustive — treated as dynamic/unknown, like List/Record.
            crate::core::types::Value::FileHandle(_) => Ok(None),
        },
        Expr::Variable(sym) => {
            if let Some(ty) = env.get(sym) {
                Ok(ty.clone())
            } else {
                Ok(None)
            }
        }
        Expr::BinaryOp { left, op, right } => {
            infer(&arena[*left], arena, env, functions, interner)?;
            infer(&arena[*right], arena, env, functions, interner)?;
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => Ok(Some(ValueType::Num)),
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge
                | BinOp::And | BinOp::Or => Ok(Some(ValueType::Bool)),
            }
        }
        Expr::Let { name, value, body } => {
            let val_ty = infer(&arena[*value], arena, env, functions, interner)?;
            let mut local_env = env.clone();
            local_env.insert(*name, val_ty);
            infer(&arena[*body], arena, &local_env, functions, interner)
        }
        Expr::Not(inner) => {
            infer(&arena[*inner], arena, env, functions, interner)?;
            Ok(Some(ValueType::Bool))
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            infer(&arena[*condition], arena, env, functions, interner)?;
            let then_ty = infer(&arena[*then_expr], arena, env, functions, interner)?;
            let else_ty = infer(&arena[*else_expr], arena, env, functions, interner)?;
            if then_ty == else_ty {
                Ok(then_ty)
            } else {
                Ok(None)
            }
        }
        Expr::FieldAccess { base, field } => {
            let base_ty = infer(&arena[*base], arena, env, functions, interner)?;
            match base_ty {
                Some(ValueType::Record(fields)) => {
                    let field_name = interner.resolve(*field).unwrap_or("");
                    for (name, ty) in fields.iter() {
                        if name.as_ref() == field_name {
                            return Ok(Some(ty.clone()));
                        }
                    }
                    Err(MizuError::StaticTypeError(format!(
                        "field `{}` not found in record type",
                        field_name
                    )))
                }
                Some(other) => {
                    let field_name = interner.resolve(*field).unwrap_or("");
                    Err(MizuError::StaticTypeError(format!(
                        "cannot access field `{}` on type `{}`",
                        field_name, other
                    )))
                }
                None => Ok(None),
            }
        }
        Expr::FunctionCall { name, args_start, args_len } => {
            let args = arena.args(*args_start, *args_len);
            let func_name = interner.resolve(*name).unwrap_or("");
            if func_name == "filter" && args.len() == 4 {
                let list_ty = infer(&arena[args[0]], arena, env, functions, interner)?;
                infer(&arena[args[1]], arena, env, functions, interner)?;
                infer(&arena[args[2]], arena, env, functions, interner)?;
                infer(&arena[args[3]], arena, env, functions, interner)?;
                match list_ty {
                    Some(ValueType::List(inner)) => Ok(Some(ValueType::List(inner))),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "filter expects a list, got `{}`",
                        other
                    ))),
                    None => Ok(None),
                }
            } else if func_name == "count" && args.len() == 3 {
                let list_ty = infer(&arena[args[0]], arena, env, functions, interner)?;
                infer(&arena[args[1]], arena, env, functions, interner)?;
                infer(&arena[args[2]], arena, env, functions, interner)?;
                match list_ty {
                    Some(ValueType::List(_)) | None => Ok(Some(ValueType::Num)),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "count expects a list, got `{}`",
                        other
                    ))),
                }
            } else if func_name == "sort" && args.len() == 3 {
                let list_ty = infer(&arena[args[0]], arena, env, functions, interner)?;
                infer(&arena[args[1]], arena, env, functions, interner)?;
                infer(&arena[args[2]], arena, env, functions, interner)?;
                match list_ty {
                    Some(ValueType::List(inner)) => Ok(Some(ValueType::List(inner))),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "sort expects a list, got `{}`",
                        other
                    ))),
                    None => Ok(None),
                }
            } else if func_name == "length" && args.len() == 1 {
                let arg_ty = infer(&arena[args[0]], arena, env, functions, interner)?;
                match arg_ty {
                    Some(ValueType::List(_)) | Some(ValueType::Str) | None => Ok(Some(ValueType::Num)),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "length expects a list or string, got `{}`",
                        other
                    ))),
                }
            } else if func_name == "to_string" && args.len() == 1 {
                let arg_ty = infer(&arena[args[0]], arena, env, functions, interner)?;
                match arg_ty {
                    Some(ValueType::Num) | Some(ValueType::Bool) | None => Ok(Some(ValueType::Str)),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "to_string expects num or bool, got `{}`",
                        other
                    ))),
                }
            } else if func_name == "contains" && args.len() == 2 {
                let haystack_ty = infer(&arena[args[0]], arena, env, functions, interner)?;
                infer(&arena[args[1]], arena, env, functions, interner)?;
                match haystack_ty {
                    Some(ValueType::Str) | None => Ok(Some(ValueType::Bool)),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "contains expects a string, got `{}`",
                        other
                    ))),
                }
            } else if func_name == "has_field" && args.len() == 2 {
                let record_ty = infer(&arena[args[0]], arena, env, functions, interner)?;
                infer(&arena[args[1]], arena, env, functions, interner)?;
                match record_ty {
                    Some(ValueType::Record(_)) | None => Ok(Some(ValueType::Bool)),
                    Some(other) => Err(MizuError::StaticTypeError(format!(
                        "has_field expects a record, got `{}`",
                        other
                    ))),
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
                for &arg in args.iter() {
                    infer(&arena[arg], arena, env, functions, interner)?;
                }
                // We do not infer return types of functions in Phase B yet,
                // or we could if we memoize/check them. For now, functions return dynamic.
                Ok(None)
            } else {
                // Builtin like 'download' or undefined function
                for &arg in args {
                    infer(&arena[arg], arena, env, functions, interner)?;
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
            attributes: rustc_hash::FxHashMap::default(),
            events: rustc_hash::FxHashMap::default(),
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

    // ────────────────────────────────────────────────────────────────────────
    // NetworkCall — `as` format-dependent static payload shape checks
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn network_call_as_text_rejects_int_literal_payload() {
        let mut interner = StringInterner::new();
        let functions = FxHashMap::default();
        let env = Env::default();
        let action = crate::parser::logic::parse_action("POST(orders, 42) -> resp as text", &mut interner)
            .unwrap();
        let err = check_action(&action, &env, &functions, &interner).unwrap_err();
        assert!(matches!(err, MizuError::StaticTypeError(_)));
    }

    #[test]
    fn network_call_as_text_accepts_string_literal_payload() {
        let mut interner = StringInterner::new();
        let functions = FxHashMap::default();
        let env = Env::default();
        let action = crate::parser::logic::parse_action(
            r#"POST(orders, "hi") -> resp as text"#,
            &mut interner,
        )
        .unwrap();
        assert!(check_action(&action, &env, &functions, &interner).is_ok());
    }

    #[test]
    fn network_call_as_json_accepts_any_literal_payload() {
        // The default/explicit `json` format imposes no static shape constraint.
        let mut interner = StringInterner::new();
        let functions = FxHashMap::default();
        let env = Env::default();
        let action =
            crate::parser::logic::parse_action("POST(orders, 42) -> resp", &mut interner).unwrap();
        assert!(check_action(&action, &env, &functions, &interner).is_ok());
    }

    #[test]
    fn network_call_as_multipart_rejects_int_literal_payload() {
        let mut interner = StringInterner::new();
        let functions = FxHashMap::default();
        let env = Env::default();
        let action = crate::parser::logic::parse_action(
            "POST(orders, 42) -> resp as multipart",
            &mut interner,
        )
        .unwrap();
        let err = check_action(&action, &env, &functions, &interner).unwrap_err();
        assert!(matches!(err, MizuError::StaticTypeError(_)));
    }

}

// This module intentionally has no Kani proofs.
//
// Every harness attempted here — including the *original*, fully concrete,
// zero-`kani::any()` harness that predates the investigation below — failed
// to complete even after 3+ minutes in isolation, confirmed with a clean,
// sequential CBMC run. That includes `infer` on the simplest possible input
// (`Expr::Literal(Value::Int(n))`) with no symbolic branching over `Expr`'s
// own variants at all, so this isn't the same "self-referential enum"
// bottleneck diagnosed and worked around in `parser::logic::eval`'s
// `kani_proofs` module (see that module's doc comment for the full
// bisection). Something specific to this module — plausibly the unrelated,
// heavier `parser::layout::{EventBlock, MizuNode}` types pulled in by
// `check_types`'s signature, or something else not yet isolated — makes
// CBMC choke here even on inputs that are cheap for `check_type` alone.
//
// Rather than ship harnesses that silently never complete in CI (which is
// the state this module was actually in before today, undetected), T4 (Type
// Soundness) stays "Open" in `RESULTS.md` and un-attempted here pending a
// real investigation into *why* this module specifically is intractable —
// not a rewritten generator, which was tried and does not help.
