//! Tests for untrusted wire → core rehydration.
//!
//! The interesting cases are all adversarial: these assert that a *forged*
//! archive — one rkyv's `bytecheck` pass would happily accept, because it is
//! structurally well-formed Rust data — is still rejected by Mizu's own
//! invariant checks.

use super::*;
use crate::wire::reload::{WireExpr, WireExprTree};

fn tree(nodes: Vec<WireExpr>, args_pool: Vec<u32>, root: u32) -> WireExprTree {
    WireExprTree {
        nodes,
        args_pool,
        root,
    }
}

#[test]
fn a_valid_tree_rehydrates() {
    // (1 + 2) as: [Literal(1), Literal(2), BinaryOp(0 + 1)]
    let t = tree(
        vec![
            WireExpr::Literal(WireValue::Int(1)),
            WireExpr::Literal(WireValue::Int(2)),
            WireExpr::BinaryOp {
                left: 0,
                op: WireBinOp::Add,
                right: 1,
            },
        ],
        vec![],
        2,
    );
    let rebuilt = rehydrate_expr_tree(&t).expect("well-formed tree must rehydrate");
    assert_eq!(rebuilt.root.index(), 2);
    assert!(matches!(rebuilt.root(), Expr::BinaryOp { .. }));
}

#[test]
fn a_child_reference_past_the_end_is_rejected() {
    // BinaryOp's right operand points at node 99, which does not exist.
    let t = tree(
        vec![
            WireExpr::Literal(WireValue::Int(1)),
            WireExpr::BinaryOp {
                left: 0,
                op: WireBinOp::Add,
                right: 99,
            },
        ],
        vec![],
        1,
    );
    let e = rehydrate_expr_tree(&t).expect_err("dangling child must be rejected");
    assert!(
        e.to_string().contains("99"),
        "error should name the bad index, got: {e}"
    );
}

#[test]
fn a_root_past_the_end_is_rejected() {
    let t = tree(vec![WireExpr::Literal(WireValue::Int(1))], vec![], 7);
    let e = rehydrate_expr_tree(&t).expect_err("dangling root must be rejected");
    assert!(
        e.to_string().contains("root"),
        "error should mention the root, got: {e}"
    );
}

#[test]
fn an_argument_pool_entry_past_the_end_is_rejected() {
    let t = tree(
        vec![WireExpr::FunctionCall {
            name: 0,
            args_start: 0,
            args_len: 1,
        }],
        vec![42], // node 42 does not exist
        0,
    );
    let e = rehydrate_expr_tree(&t).expect_err("dangling pool entry must be rejected");
    assert!(
        e.to_string().contains("42"),
        "error should name the bad index, got: {e}"
    );
}

#[test]
fn a_function_call_window_past_the_pool_is_rejected() {
    let t = tree(
        vec![WireExpr::FunctionCall {
            name: 0,
            args_start: 0,
            args_len: 5, // pool holds 1
        }],
        vec![0],
        0,
    );
    let e = rehydrate_expr_tree(&t).expect_err("oversized args window must be rejected");
    assert!(
        e.to_string().contains("pool"),
        "error should mention the pool, got: {e}"
    );
}

#[test]
fn a_function_call_window_cannot_wrap_around() {
    // start + len overflows u32; the check must be done in wider arithmetic
    // or this window would appear to fit.
    let t = tree(
        vec![WireExpr::FunctionCall {
            name: 0,
            args_start: u32::MAX,
            args_len: 2,
        }],
        vec![0],
        0,
    );
    rehydrate_expr_tree(&t).expect_err("wrapping args window must be rejected");
}

#[test]
fn deeply_nested_values_are_bounded() {
    let mut v = WireValue::Int(0);
    for _ in 0..500 {
        v = WireValue::List(vec![v]);
    }
    rehydrate_value(&v, 0).expect_err("over-deep value must be rejected, not overflow the stack");
}

#[test]
fn records_are_reordered_into_canonical_form() {
    use crate::wire::value::WireRecordField;
    // Fields arrive out of lexicographic order; `Value::Record`'s invariant
    // says they must not stay that way.
    let v = WireValue::Record(vec![
        WireRecordField {
            key: "zebra".to_string(),
            hash: 0,
            value: WireValue::Int(1),
        },
        WireRecordField {
            key: "apple".to_string(),
            hash: 0,
            value: WireValue::Int(2),
        },
    ]);
    let rebuilt = rehydrate_value(&v, 0).expect("record must rehydrate");
    match rebuilt {
        Value::Record(fields) => {
            assert_eq!(&*fields[0].key, "apple");
            assert_eq!(&*fields[1].key, "zebra");
        }
        other => panic!("expected a record, got {other:?}"),
    }
}
