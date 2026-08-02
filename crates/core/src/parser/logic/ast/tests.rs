//! Tests for the ast module.

use super::*;

#[test]
fn push_args_then_args_round_trips_exactly() {
    let mut arena = ExprArena::new();
    let a = arena.alloc(Expr::Literal(Value::Decimal(1)));
    let b = arena.alloc(Expr::Literal(Value::Decimal(2)));
    let c = arena.alloc(Expr::Literal(Value::Decimal(3)));
    let (start, len) = arena.push_args(&[a, b, c]).unwrap();
    assert_eq!(arena.args(start, len), &[a, b, c]);
}

#[test]
fn push_args_empty_slice_round_trips_to_empty() {
    let mut arena = ExprArena::new();
    let (start, len) = arena.push_args(&[]).unwrap();
    assert_eq!(arena.args(start, len), &[] as &[ExprId]);
}

#[test]
fn multiple_push_args_calls_do_not_overlap() {
    // Two FunctionCall nodes' argument ranges, pushed in sequence, must
    // each resolve back to exactly their own arguments -- proving the
    // shared pool doesn't let one call's args bleed into another's.
    let mut arena = ExprArena::new();
    let x = arena.alloc(Expr::Literal(Value::Decimal(10)));
    let y = arena.alloc(Expr::Literal(Value::Decimal(20)));
    let (start1, len1) = arena.push_args(&[x]).unwrap();
    let (start2, len2) = arena.push_args(&[y, x]).unwrap();
    assert_eq!(arena.args(start1, len1), &[x]);
    assert_eq!(arena.args(start2, len2), &[y, x]);
}

#[test]
fn function_call_args_resolve_through_shared_pool() {
    // End-to-end: a FunctionCall node stores (args_start, args_len),
    // not its own collection -- resolving it through the arena must
    // recover the exact argument ExprIds that were pushed.
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Decimal(42)));
    let (args_start, args_len) = arena.push_args(&[arg0]).unwrap();
    let call = Expr::FunctionCall {
        name: Symbol(0),
        args_start,
        args_len,
    };
    let Expr::FunctionCall {
        args_start,
        args_len,
        ..
    } = call
    else {
        unreachable!()
    };
    assert_eq!(arena.args(args_start, args_len), &[arg0]);
}
