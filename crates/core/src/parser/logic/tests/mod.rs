//! Test suite for `parser::logic`, split to mirror the source modules it
//! exercises: [`parse`] (lexing/parsing), [`eval`] (evaluation, `apply_binop`,
//! `execute_action`), [`comp`] (computed bindings), [`purity`]
//! (`find_side_effect_call`), and [`lexer`] (tokenization edge cases).
//!
//! Shared fixtures live here so every submodule can reach them via
//! `use super::*;`, the same way they were all in scope in the single file
//! this replaced.

use super::{
    Action, BinOp, ComputedBinding, Expr, ExprArena, MizuFunction, NetworkMethod, PayloadFormat,
    TimerInterval, ValueType, parse_action, parse_action_with_urls, parse_logic, parse_root_timers,
};
use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, Value, VariableStore};
use rustc_hash::{FxHashMap, FxHashSet};
use std::rc::Rc;

mod comp;
mod eval;
mod lexer;
mod parse;
mod purity;

fn single_fn(src: &str) -> Result<(FxHashMap<Symbol, MizuFunction>, StringInterner), MizuError> {
    let mut interner = StringInterner::new();
    let fns = parse_logic(src, &mut interner)?;
    Ok((fns, interner))
}

fn evaluate(
    expr: &Expr,
    arena: &ExprArena,
    store: &Rc<VariableStore>,
    functions: &FxHashMap<Symbol, MizuFunction>,
) -> Result<Value, MizuError> {
    let mut temp_store = (**store).clone();
    super::evaluate(expr, arena, &mut temp_store, functions, 0)
}

fn execute_action(
    action: &Action,
    store: &mut Rc<VariableStore>,
    functions: &FxHashMap<Symbol, MizuFunction>,
) -> Result<bool, MizuError> {
    let mut temp_store = (**store).clone();
    let result = super::execute_action(action, &mut temp_store, functions)?;
    *store = Rc::new(temp_store);
    Ok(result)
}

fn eval_src(src: &str) -> Result<Value, MizuError> {
    let wrapper = format!("  f() : {src}\n");
    let (fns, interner) = single_fn(&wrapper)?;
    let f_sym = interner
        .get("f")
        .ok_or_else(|| MizuError::ParseError("f not found in interner".to_string()))?;
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    evaluate(
        fns[&f_sym].body.root(),
        &fns[&f_sym].body.arena,
        &store,
        &fns,
    )
}
