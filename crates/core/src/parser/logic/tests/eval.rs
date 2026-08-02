//! Tests for `eval.rs`: `evaluate`, `execute_action`, `apply_binop`, and the
//! `Evaluator`'s instruction-budget bookkeeping.

use super::*;

// ────────────────────────────────────────────────────────────────────────
// Evaluator — happy paths
// ────────────────────────────────────────────────────────────────────────

#[test]
fn evaluate_literal_num() {
    let expr = Expr::Literal(Value::Decimal(420_000));
    let arena = ExprArena::new();
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::Decimal(420_000)
    );
}

#[test]
fn evaluate_literal_bool() {
    let expr = Expr::Literal(Value::Bool(true));
    let arena = ExprArena::new();
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::Bool(true)
    );
}

#[test]
fn evaluate_variable_lookup() {
    let mut store = VariableStore::new();
    store.set("x", Value::Int(70_000));
    let mut store = store.freeze();
    let x_sym = store.interner.get("x").unwrap();
    let store = Rc::new(store);
    let expr = Expr::Variable(x_sym);
    let arena = ExprArena::new();
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::Int(70_000)
    );
}

#[test]
fn evaluate_addition() {
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::Decimal(30_000)));
    let right = arena.alloc(Expr::Literal(Value::Decimal(40_000)));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Add,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::Decimal(70_000)
    );
}

#[test]
fn evaluate_subtraction() {
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::Decimal(100_000)));
    let right = arena.alloc(Expr::Literal(Value::Decimal(35_000)));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Sub,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::Decimal(65_000)
    );
}

#[test]
fn evaluate_multiplication() {
    // 6 * 7 = 42
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::Decimal(
        6 * crate::core::types::DECIMAL_SCALE,
    )));
    let right = arena.alloc(Expr::Literal(Value::Decimal(
        7 * crate::core::types::DECIMAL_SCALE,
    )));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Mul,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::Decimal(42 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn evaluate_division() {
    // 15 / 3 = 5
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::Decimal(
        15 * crate::core::types::DECIMAL_SCALE,
    )));
    let right = arena.alloc(Expr::Literal(Value::Decimal(
        3 * crate::core::types::DECIMAL_SCALE,
    )));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Div,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::Decimal(5 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn evaluate_string_concatenation() {
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::String(std::sync::Arc::from(
        "Hello, ",
    ))));
    let right = arena.alloc(Expr::Literal(Value::String(std::sync::Arc::from("Mizu!"))));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Add,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    assert_eq!(
        evaluate(&expr, &arena, &store, &fns).unwrap(),
        Value::String(std::sync::Arc::from("Hello, Mizu!"))
    );
}

#[test]
fn evaluate_inline_function_call() {
    let src = "    vat(p: num) : p * 1.22\n";
    let (fns, interner) = single_fn(src).unwrap();
    let vat_sym = interner.get("vat").unwrap();
    let mut store = VariableStore::with_interner(interner.freeze());
    store.set_runtime("p", Value::Decimal(100 * crate::core::types::DECIMAL_SCALE));
    let store = Rc::new(store);
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Decimal(
        100 * crate::core::types::DECIMAL_SCALE,
    )));
    let (args_start, args_len) = arena.push_args(&[arg0]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: vat_sym,
        args_start,
        args_len,
    };
    let result = evaluate(&call_expr, &arena, &store, &fns).unwrap();
    // 100 * 1.22 = 122
    assert_eq!(
        result,
        Value::Decimal(122 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn evaluate_function_calling_function() {
    let src = r"
    double(x: num) : x * 2
    quadruple(x: num) : double(double(x))
";
    let (fns, interner) = single_fn(src).unwrap();
    let quadruple_sym = interner.get("quadruple").unwrap();
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Decimal(
        3 * crate::core::types::DECIMAL_SCALE,
    )));
    let (args_start, args_len) = arena.push_args(&[arg0]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: quadruple_sym,
        args_start,
        args_len,
    };
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(&call_expr, &arena, &store, &fns).unwrap();
    // 3 * 4 = 12
    assert_eq!(
        result,
        Value::Decimal(12 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn evaluate_multiline_function_with_let_binding() {
    let src = r"
    total(price: num, qty: num)
        netto = price * qty
        netto * 1.22
";
    let (fns, interner) = single_fn(src).unwrap();
    let total_sym = interner.get("total").unwrap();
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Decimal(
        10 * crate::core::types::DECIMAL_SCALE,
    )));
    let arg1 = arena.alloc(Expr::Literal(Value::Decimal(
        3 * crate::core::types::DECIMAL_SCALE,
    )));
    let (args_start, args_len) = arena.push_args(&[arg0, arg1]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: total_sym,
        args_start,
        args_len,
    };
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(&call_expr, &arena, &store, &fns).unwrap();
    // netto = 10 * 3 = 30, result = 30 * 1.22 = 36.6
    assert_eq!(result, Value::Decimal(3_660_000_000));
}

#[test]
fn evaluate_function_with_store_variables() {
    // Outer store values should NOT bleed into the function's local scope.
    let src = "    area(w: num, h: num) : w * h\n";
    let (fns, interner) = single_fn(src).unwrap();
    let area_sym = interner.get("area").unwrap();
    let mut outer_store = VariableStore::with_interner(interner.freeze());
    outer_store.set_runtime("w", Value::Decimal(999 * crate::core::types::DECIMAL_SCALE)); // should be ignored inside the function
    let outer_store = Rc::new(outer_store);
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Decimal(
        5 * crate::core::types::DECIMAL_SCALE,
    )));
    let arg1 = arena.alloc(Expr::Literal(Value::Decimal(
        4 * crate::core::types::DECIMAL_SCALE,
    )));
    let (args_start, args_len) = arena.push_args(&[arg0, arg1]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: area_sym,
        args_start,
        args_len,
    };
    // Function arguments override the outer store inside the function body.
    let result = evaluate(&call_expr, &arena, &outer_store, &fns).unwrap();
    assert_eq!(
        result,
        Value::Decimal(20 * crate::core::types::DECIMAL_SCALE)
    );
}

// ────────────────────────────────────────────────────────────────────────
// Type error paths
// ────────────────────────────────────────────────────────────────────────

#[test]
fn error_num_plus_bool_is_type_error() {
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::Decimal(1)));
    let right = arena.alloc(Expr::Literal(Value::Bool(true)));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Add,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    let result = evaluate(&expr, &arena, &store, &fns);
    assert!(
        matches!(result, Err(MizuError::TypeError { .. })),
        "expected TypeError, got: {result:?}"
    );
}

#[test]
fn error_num_mul_string_is_type_error() {
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::Decimal(2)));
    let right = arena.alloc(Expr::Literal(Value::String(std::sync::Arc::from("oops"))));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Mul,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    let result = evaluate(&expr, &arena, &store, &fns);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn error_bool_sub_num_is_type_error() {
    let mut arena = ExprArena::new();
    let left = arena.alloc(Expr::Literal(Value::Bool(true)));
    let right = arena.alloc(Expr::Literal(Value::Decimal(1)));
    let expr = Expr::BinaryOp {
        left,
        op: BinOp::Sub,
        right,
    };
    let store = Rc::new(VariableStore::new().freeze());
    let fns = FxHashMap::default();
    let result = evaluate(&expr, &arena, &store, &fns);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn error_wrong_argument_type_for_function() {
    // `vat` expects `num`, but receives `bool`.
    let src = "    vat(p: num) : p * 1.22\n";
    let (fns, interner) = single_fn(src).unwrap();
    let vat_sym = interner.get("vat").unwrap();
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Bool(true)));
    let (args_start, args_len) = arena.push_args(&[arg0]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: vat_sym,
        args_start,
        args_len,
    };
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(&call_expr, &arena, &store, &fns);
    assert!(
        matches!(result, Err(MizuError::TypeError { .. })),
        "expected TypeError for wrong argument type, got: {result:?}"
    );
}

#[test]
fn error_wrong_arity_too_few() {
    let src = "    add(a: num, b: num) : a + b\n";
    let (fns, interner) = single_fn(src).unwrap();
    let add_sym = interner.get("add").unwrap();
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Decimal(1)));
    let (args_start, args_len) = arena.push_args(&[arg0]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: add_sym,
        args_start,
        args_len,
    };
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(&call_expr, &arena, &store, &fns);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("argument")),
        "expected arity error, got: {result:?}"
    );
}

#[test]
fn error_wrong_arity_too_many() {
    let src = "    inc(x: num) : x + 1\n";
    let (fns, interner) = single_fn(src).unwrap();
    let inc_sym = interner.get("inc").unwrap();
    let mut arena = ExprArena::new();
    let arg0 = arena.alloc(Expr::Literal(Value::Decimal(1)));
    let arg1 = arena.alloc(Expr::Literal(Value::Decimal(2)));
    let (args_start, args_len) = arena.push_args(&[arg0, arg1]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: inc_sym,
        args_start,
        args_len,
    };
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(&call_expr, &arena, &store, &fns);
    assert!(matches!(result, Err(MizuError::ParseError(_))));
}

#[test]
fn error_undefined_function_call() {
    let mut interner = StringInterner::new();
    let ghost_sym = interner.get_or_intern("ghost");
    let mut arena = ExprArena::new();
    let (args_start, args_len) = arena.push_args(&[]).unwrap();
    let call_expr = Expr::FunctionCall {
        name: ghost_sym,
        args_start,
        args_len,
    };
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let fns = FxHashMap::default();
    let result = evaluate(&call_expr, &arena, &store, &fns);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("ghost")),
        "expected undefined-function error, got: {result:?}"
    );
}

#[test]
fn error_variable_not_found() {
    let mut interner = StringInterner::new();
    let missing_sym = interner.get_or_intern("missing");
    let expr = Expr::Variable(missing_sym);
    let arena = ExprArena::new();
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let fns = FxHashMap::default();
    let result = evaluate(&expr, &arena, &store, &fns);
    assert!(
        matches!(result, Err(MizuError::VariableNotFound(_))),
        "expected VariableNotFound, got: {result:?}"
    );
}

#[test]
fn execute_action_assignment_mutates_store() {
    let mut store = VariableStore::new();
    store.set("count", Value::Int(10_000));
    let mut store = store.freeze();
    let mut store = Rc::new(store);
    let functions = FxHashMap::default();

    let action = parse_action("count = count + 1", &mut StringInterner::new()).unwrap();
    let mutated = execute_action(&action, &mut store, &functions).unwrap();
    assert!(mutated);
    assert_eq!(*store.get("count").unwrap(), Value::Int(10_001));
}

#[test]
fn execute_action_pure_expression_no_mutation() {
    let mut store = VariableStore::new();
    store.set("count", Value::Int(10_000));
    let mut store = store.freeze();
    let mut store = Rc::new(store);
    let functions = FxHashMap::default();

    let action = parse_action("count + 1", &mut StringInterner::new()).unwrap();
    let mutated = execute_action(&action, &mut store, &functions).unwrap();
    assert!(!mutated);
    // Ensure count wasn't mutated
    assert_eq!(*store.get("count").unwrap(), Value::Int(10_000));
}

// ────────────────────────────────────────────────────────────────────────
// execute_action — path_param validation gate (G2)
// ────────────────────────────────────────────────────────────────────────

fn network_call_action(path_param_value: &str) -> Action {
    let mut arena = ExprArena::new();
    let root = arena.alloc(Expr::Literal(Value::from(path_param_value)));
    Action::NetworkCall {
        method: NetworkMethod::Get,
        alias_sym: Symbol(0),
        payload: None,
        path_param: Some(crate::parser::logic::ExprTree { arena, root }),
        target_var: "data".to_string(),
        format: crate::parser::logic::PayloadFormat::Json,
        headers: vec![],
    }
}

#[test]
fn path_param_ok_accepts_single_alphanumeric_segment() {
    assert!(super::super::path_param_ok("abc123"));
    assert!(super::super::path_param_ok("foo-bar_123.~baz"));
}

#[test]
fn path_param_ok_rejects_forward_slash() {
    assert!(!super::super::path_param_ok("a/b"));
}

#[test]
fn path_param_ok_rejects_backslash() {
    assert!(!super::super::path_param_ok("a\\b"));
}

#[test]
fn path_param_ok_rejects_traversal_substring() {
    assert!(!super::super::path_param_ok(".."));
    assert!(!super::super::path_param_ok("a..b"));
}

#[test]
fn path_param_ok_rejects_control_characters() {
    assert!(!super::super::path_param_ok("a\nb"));
    assert!(!super::super::path_param_ok("a\tb"));
    assert!(!super::super::path_param_ok("a\u{7F}b"));
}

#[test]
fn execute_action_network_call_valid_path_param_accepted() {
    let mut base = VariableStore::new();
    base.interner.get_or_intern("data");
    let mut store = Rc::new(base.freeze());
    let functions = FxHashMap::default();

    let action = network_call_action("abc123");
    let mutated = execute_action(&action, &mut store, &functions).unwrap();
    assert!(mutated);
    assert_eq!(store.evaluator.accumulated_actions.len(), 1);
    match &store.evaluator.accumulated_actions[0] {
        crate::messages::RuntimeAction::NetworkCall { path_param, .. } => {
            assert_eq!(path_param.as_deref(), Some("abc123"));
        }
        other => panic!("expected NetworkCall, got {other:?}"),
    }
}

#[test]
fn execute_action_network_call_path_param_with_slash_rejected() {
    let mut store = Rc::new(VariableStore::new().freeze());
    let functions = FxHashMap::default();

    let action = network_call_action("../etc/passwd");
    let err = execute_action(&action, &mut store, &functions).unwrap_err();
    assert!(
        matches!(err, MizuError::ExecutionError(_)),
        "expected ExecutionError, got {err:?}"
    );
    assert!(
        store.evaluator.accumulated_actions.is_empty(),
        "a rejected path_param must not be queued as a network action"
    );
}

#[test]
fn execute_action_network_call_path_param_with_backslash_rejected() {
    let mut store = Rc::new(VariableStore::new().freeze());
    let functions = FxHashMap::default();

    let action = network_call_action("a\\b");
    let err = execute_action(&action, &mut store, &functions).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[test]
fn execute_action_network_call_path_param_with_control_char_rejected() {
    let mut store = Rc::new(VariableStore::new().freeze());
    let functions = FxHashMap::default();

    let action = network_call_action("a\nb");
    let err = execute_action(&action, &mut store, &functions).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[test]
fn error_variable_fallback_no_implicit_call() {
    let mut interner = StringInterner::new();
    let fns = parse_logic("    count = 10\n", &mut interner).unwrap();
    let count_sym = interner.get("count").unwrap();
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let expr = Expr::Variable(count_sym);
    let arena = ExprArena::new();
    let result = evaluate(&expr, &arena, &store, &fns);
    assert!(
        matches!(result, Err(MizuError::VariableNotFound(ref name)) if name == "count"),
        "expected VariableNotFound for count, got: {result:?}"
    );
}

#[test]
fn test_cooperative_checkpointing_timeout() {
    use crate::core::types::Evaluator;

    // Pre-saturate the instruction counter to the instruction budget.
    // The very next call to `evaluate` increments it to the instruction budget + 1,
    // triggering the `instruction_count > the instruction budget` check immediately.
    // This avoids building a deep recursive tree that would overflow the call
    // stack in debug mode before the instruction limit is ever reached.
    let mut sm = Evaluator::new(crate::core::config::CONFIG.max_instructions);
    sm.instruction_count = crate::core::config::CONFIG.max_instructions;

    let interner = crate::core::types::StringInterner::new().freeze();
    let fns = FxHashMap::default();
    let expr = Expr::Literal(Value::Decimal(1));
    let arena = ExprArena::new();

    let res = sm.evaluate(&expr, 0, &fns, &interner, &arena);
    assert!(
        matches!(res, Err(MizuError::Timeout)),
        "expected Timeout, got: {res:?}"
    );
}

#[test]
fn test_instruction_budget_resets_per_action() {
    // Verify that execute_action resets instruction_count to 0 before each evaluation,
    // so two consecutive actions each get the full the instruction budget budget.

    let mut store = VariableStore::new();
    let fns = FxHashMap::default();
    let mut interner = crate::core::types::StringInterner::new();
    let x_sym = interner.get_or_intern("x");
    store.interner = interner;
    let mut store = store.freeze();
    store.evaluator.set_global(x_sym, Value::Decimal(0));

    // First action — must succeed even if counter was near-exhausted from a prior call.
    store.evaluator.instruction_count = crate::core::config::CONFIG.max_instructions - 1;
    let mut arena1 = ExprArena::new();
    let root1 = arena1.alloc(Expr::Literal(Value::Decimal(1)));
    let action1 = Action::Assign {
        target: "x".to_string(),
        expr: crate::parser::logic::ExprTree {
            arena: arena1,
            root: root1,
        },
    };
    let r1 = super::super::execute_action(&action1, &mut store, &fns);
    assert!(
        r1.is_ok(),
        "first action should succeed (counter reset to 0): {r1:?}"
    );

    // Second action — counter was reset by execute_action, must also succeed.
    let mut arena2 = ExprArena::new();
    let root2 = arena2.alloc(Expr::Literal(Value::Decimal(2)));
    let action2 = Action::Assign {
        target: "x".to_string(),
        expr: crate::parser::logic::ExprTree {
            arena: arena2,
            root: root2,
        },
    };
    let r2 = super::super::execute_action(&action2, &mut store, &fns);
    assert!(
        r2.is_ok(),
        "second action should succeed (counter reset to 0): {r2:?}"
    );
}

#[test]
fn test_flat_state_machine_scoping() {
    use crate::core::types::Evaluator;

    let mut sm = Evaluator::new(crate::core::config::CONFIG.max_instructions);
    let mut interner = crate::core::types::StringInterner::new();
    let fns = FxHashMap::default();

    // Set global variables
    let x_sym = interner.get_or_intern("x");
    let y_sym = interner.get_or_intern("y");
    sm.set_global(x_sym, Value::Decimal(10));
    sm.set_global(y_sym, Value::Decimal(20));

    // Evaluate an expression shadowing 'x' using Let binding:
    // let x = 15 in x + y
    let mut arena = ExprArena::new();
    let value = arena.alloc(Expr::Literal(Value::Decimal(15)));
    let left = arena.alloc(Expr::Variable(x_sym));
    let right = arena.alloc(Expr::Variable(y_sym));
    let body = arena.alloc(Expr::BinaryOp {
        left,
        op: BinOp::Add,
        right,
    });
    let expr = Expr::Let {
        name: x_sym,
        value,
        body,
    };

    let interner = interner.freeze();
    let res = sm.evaluate(&expr, 0, &fns, &interner, &arena).unwrap();
    assert_eq!(res, Value::Decimal(35));
}

// ────────────────────────────────────────────────────────────────────────
// Comparison operators
// ────────────────────────────────────────────────────────────────────────

#[test]
fn compare_int_eq_true() {
    assert_eq!(eval_src("3 == 3").unwrap(), Value::Bool(true));
}

#[test]
fn compare_int_eq_false() {
    assert_eq!(eval_src("3 == 4").unwrap(), Value::Bool(false));
}

#[test]
fn compare_int_ne() {
    assert_eq!(eval_src("3 != 4").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("3 != 3").unwrap(), Value::Bool(false));
}

#[test]
fn compare_int_lt_gt() {
    assert_eq!(eval_src("2 < 5").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("5 < 2").unwrap(), Value::Bool(false));
    assert_eq!(eval_src("5 > 2").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("2 > 5").unwrap(), Value::Bool(false));
}

#[test]
fn compare_int_le_ge() {
    assert_eq!(eval_src("3 <= 3").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("2 <= 3").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("4 <= 3").unwrap(), Value::Bool(false));
    assert_eq!(eval_src("3 >= 3").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("4 >= 3").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("2 >= 3").unwrap(), Value::Bool(false));
}

#[test]
fn compare_float_int_mixed() {
    assert_eq!(eval_src("3.0 == 3").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("3 < 3.5").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("4 > 3.5").unwrap(), Value::Bool(true));
}

#[test]
fn compare_strings_eq_ne() {
    assert_eq!(
        eval_src(r#""hello" == "hello""#).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        eval_src(r#""hello" == "world""#).unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        eval_src(r#""hello" != "world""#).unwrap(),
        Value::Bool(true)
    );
}

#[test]
fn compare_bools_eq() {
    assert_eq!(eval_src("true == true").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("true == false").unwrap(), Value::Bool(false));
}

// ────────────────────────────────────────────────────────────────────────
// Logical operators (&&, ||, !)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn logical_and() {
    assert_eq!(eval_src("true && true").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("true && false").unwrap(), Value::Bool(false));
    assert_eq!(eval_src("false && false").unwrap(), Value::Bool(false));
}

#[test]
fn logical_or() {
    assert_eq!(eval_src("true || false").unwrap(), Value::Bool(true));
    assert_eq!(eval_src("false || false").unwrap(), Value::Bool(false));
    assert_eq!(eval_src("false || true").unwrap(), Value::Bool(true));
}

#[test]
fn logical_not() {
    assert_eq!(eval_src("!true").unwrap(), Value::Bool(false));
    assert_eq!(eval_src("!false").unwrap(), Value::Bool(true));
}

#[test]
fn logical_combined_precedence() {
    // `3 > 2 && 1 < 5` → `true && true` → `true`
    assert_eq!(eval_src("3 > 2 && 1 < 5").unwrap(), Value::Bool(true));
    // `!false || false` → `true || false` → `true`
    assert_eq!(eval_src("!false || false").unwrap(), Value::Bool(true));
}

// ────────────────────────────────────────────────────────────────────────
// Conditional expressions: if/then/else and ternary ?:
// ────────────────────────────────────────────────────────────────────────

#[test]
fn if_then_else_true_branch() {
    assert_eq!(
        eval_src("if true then 1 else 2").unwrap(),
        Value::Decimal(1 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn if_then_else_false_branch() {
    assert_eq!(
        eval_src("if false then 1 else 2").unwrap(),
        Value::Decimal(2 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn if_then_else_with_expression_condition() {
    assert_eq!(
        eval_src("if 3 > 2 then 10 else 20").unwrap(),
        Value::Decimal(10 * crate::core::types::DECIMAL_SCALE)
    );
    assert_eq!(
        eval_src("if 1 > 2 then 10 else 20").unwrap(),
        Value::Decimal(20 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn if_then_else_returns_string() {
    assert_eq!(
        eval_src(r#"if true then "si" else "no""#).unwrap(),
        Value::String(std::sync::Arc::from("si"))
    );
}

#[test]
fn ternary_true_branch() {
    assert_eq!(
        eval_src("true ? 1 : 2").unwrap(),
        Value::Decimal(1 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn ternary_false_branch() {
    assert_eq!(
        eval_src("false ? 1 : 2").unwrap(),
        Value::Decimal(2 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn ternary_with_expression_condition() {
    assert_eq!(
        eval_src("5 > 3 ? 100 : 200").unwrap(),
        Value::Decimal(100 * crate::core::types::DECIMAL_SCALE)
    );
    assert_eq!(
        eval_src("1 == 2 ? 100 : 200").unwrap(),
        Value::Decimal(200 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn ternary_right_associative() {
    // `true ? 1 : false ? 2 : 3` → `true ? 1 : (false ? 2 : 3)` → 1
    assert_eq!(
        eval_src("true ? 1 : false ? 2 : 3").unwrap(),
        Value::Decimal(1 * crate::core::types::DECIMAL_SCALE)
    );
    // `false ? 1 : false ? 2 : 3` → `false ? 1 : (false ? 2 : 3)` → 3
    assert_eq!(
        eval_src("false ? 1 : false ? 2 : 3").unwrap(),
        Value::Decimal(3 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn if_else_non_bool_condition_is_type_error() {
    let err = eval_src("if 42 then 1 else 2").unwrap_err();
    assert!(matches!(err, MizuError::TypeError { .. }));
}

#[test]
fn ternary_non_bool_condition_is_type_error() {
    let err = eval_src(r#""yes" ? 1 : 2"#).unwrap_err();
    assert!(matches!(err, MizuError::TypeError { .. }));
}

#[test]
fn if_then_missing_else_is_parse_error() {
    let src = "doppio(n: num) : if n > 0 then n";
    let err = parse_logic(src, &mut StringInterner::new()).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

#[test]
fn if_else_used_in_function_body() {
    let src = "
absolute_value(n: num) : if n >= 0 then n else 0 - n
";
    let mut interner = StringInterner::new();
    let fns = parse_logic(src.trim(), &mut interner).unwrap();
    let va_sym = interner.get("absolute_value").unwrap();
    let mut store = VariableStore::with_interner(interner.freeze());
    let pos = fns[&va_sym].body.clone();
    store.set_runtime("n", Value::Decimal(5 * crate::core::types::DECIMAL_SCALE));
    let v = store
        .evaluator
        .evaluate(pos.root(), 0, &fns, &store.interner, &pos.arena)
        .unwrap();
    // just verify the function compiles — full eval needs param binding
    let _ = v;
    // Smoke test: parse succeeds and body is IfElse
    assert!(matches!(fns[&va_sym].body.root(), Expr::IfElse { .. }));
}

// ────────────────────────────────────────────────────────────────────────
// Type-error failure paths for new operators
// ────────────────────────────────────────────────────────────────────────

#[test]
fn string_lt_is_lexicographic_ordering() {
    // Strings gained ordering-operator support (previously a TypeError)
    // so `filter`/`sort`'s new comparison-operator forms can reuse the
    // language's own `<`/`>`/`<=`/`>=` instead of a separate,
    // filter-only comparison implementation.
    assert_eq!(eval_src(r#""a" < "b""#).unwrap(), Value::Bool(true));
    assert_eq!(eval_src(r#""b" < "a""#).unwrap(), Value::Bool(false));
    assert_eq!(eval_src(r#""a" > "b""#).unwrap(), Value::Bool(false));
    assert_eq!(eval_src(r#""a" <= "a""#).unwrap(), Value::Bool(true));
    assert_eq!(eval_src(r#""a" >= "a""#).unwrap(), Value::Bool(true));
}

#[test]
fn error_lt_on_string_and_int_is_type_error() {
    // Ordering across *different* types is still rejected — only
    // String×String and Int×Int are defined.
    let result = eval_src(r#""a" < 1"#);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn error_and_on_nums_is_type_error() {
    let result = eval_src("1 && 0");
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn error_not_on_num_is_type_error() {
    let result = eval_src("!42");
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

// ────────────────────────────────────────────────────────────────────────
// Integer overflow — apply_binop checked arithmetic
// ────────────────────────────────────────────────────────────────────────

#[test]
fn apply_binop_add_overflow() {
    let mut ic = 0u64;
    let result = super::super::apply_binop(
        &BinOp::Add,
        Value::Decimal(i64::MAX),
        Value::Decimal(1),
        &mut ic,
        crate::core::config::CONFIG.max_instructions,
    );
    assert!(
        matches!(result, Err(MizuError::IntegerOverflow)),
        "expected IntegerOverflow for i64::MAX + 1, got: {result:?}"
    );
}

#[test]
fn apply_binop_mul_overflow() {
    // With i128-widened intermediates, `i64::MAX * 2` no longer overflows
    // the multiply step itself (i128 comfortably holds it) — only the
    // final narrowing back to i64 can fail, and only when the *true*
    // result doesn't fit. `i64::MAX * i64::MAX` genuinely doesn't fit.
    let mut ic = 0u64;
    let result = super::super::apply_binop(
        &BinOp::Mul,
        Value::Decimal(i64::MAX),
        Value::Decimal(i64::MAX),
        &mut ic,
        crate::core::config::CONFIG.max_instructions,
    );
    assert!(
        matches!(result, Err(MizuError::IntegerOverflow)),
        "expected IntegerOverflow for i64::MAX * i64::MAX, got: {result:?}"
    );
}

#[test]
fn apply_binop_mul_i128_widening_avoids_collapsed_range() {
    // Real-valued 1000 * 1000 = 1,000,000. At the old i64-only
    // "checked_mul(l, r) before dividing by DECIMAL_SCALE" shape, the raw
    // scaled product `(1000 * DECIMAL_SCALE) * (1000 * DECIMAL_SCALE)`
    // overflows i64 at this scale even though both operands and the true
    // result comfortably fit i64. The i128-widened implementation must
    // succeed here with the exact expected value — this is the test that
    // proves the wider scale's range is actually usable, not just wider
    // on paper.
    let mut ic = 0u64;
    let scale = crate::core::types::DECIMAL_SCALE;
    let l = 1_000 * scale;
    let r = 1_000 * scale;
    // Sanity-check the premise: the old i64-only shape would have overflowed here.
    assert!(
        l.checked_mul(r).is_none(),
        "premise broken: raw product no longer overflows i64 — pick larger operands"
    );
    let result = super::super::apply_binop(
        &BinOp::Mul,
        Value::Decimal(l),
        Value::Decimal(r),
        &mut ic,
        crate::core::config::CONFIG.max_instructions,
    )
    .unwrap();
    assert_eq!(result, Value::Decimal(1_000_000 * scale));
}

#[test]
fn apply_binop_div_i128_widening_avoids_collapsed_range() {
    // Real-valued 1,000,000 / 1,000 = 1,000. At the old i64-only
    // "checked_mul(l, DECIMAL_SCALE) before dividing by r" shape, the raw
    // numerator `(1_000_000 * DECIMAL_SCALE) * DECIMAL_SCALE` overflows
    // i64 at this scale even though the true result fits comfortably.
    let mut ic = 0u64;
    let scale = crate::core::types::DECIMAL_SCALE;
    let l = 1_000_000 * scale;
    let r = 1_000 * scale;
    // Sanity-check the premise: the old i64-only shape would have overflowed here.
    assert!(
        l.checked_mul(scale).is_none(),
        "premise broken: raw numerator no longer overflows i64 — pick a larger operand"
    );
    let result = super::super::apply_binop(
        &BinOp::Div,
        Value::Decimal(l),
        Value::Decimal(r),
        &mut ic,
        crate::core::config::CONFIG.max_instructions,
    )
    .unwrap();
    assert_eq!(result, Value::Decimal(1_000 * scale));
}

#[test]
fn apply_binop_sub_underflow() {
    let mut ic = 0u64;
    let result = super::super::apply_binop(
        &BinOp::Sub,
        Value::Decimal(i64::MIN),
        Value::Decimal(1),
        &mut ic,
        crate::core::config::CONFIG.max_instructions,
    );
    assert!(
        matches!(result, Err(MizuError::IntegerOverflow)),
        "expected IntegerOverflow for i64::MIN - 1, got: {result:?}"
    );
}

#[test]
fn apply_binop_div_overflow() {
    let mut ic = 0u64;
    let result = super::super::apply_binop(
        &BinOp::Div,
        Value::Decimal(i64::MIN),
        Value::Decimal(-1),
        &mut ic,
        crate::core::config::CONFIG.max_instructions,
    );
    assert!(
        matches!(result, Err(MizuError::IntegerOverflow)),
        "expected IntegerOverflow for i64::MIN / -1, got: {result:?}"
    );
}

#[test]
fn apply_binop_div_numerator_overflow_errors_not_corrupts() {
    // `l` just past `i64::MAX / DECIMAL_SCALE`, so `l.checked_mul(DECIMAL_SCALE)`
    // overflows `i64`. Pre-fix this used `saturating_mul`, which silently
    // clamped to `i64::MAX` and returned a wrong-but-plausible result
    // instead of failing — this pins the fail-secure behavior.
    let mut ic = 0u64;
    let l = i64::MAX / crate::core::types::DECIMAL_SCALE + 1;
    let result = super::super::apply_binop(
        &BinOp::Div,
        Value::Decimal(l),
        Value::Decimal(1),
        &mut ic,
        crate::core::config::CONFIG.max_instructions,
    );
    match result {
        Err(MizuError::IntegerOverflow) => {}
        other => panic!("expected IntegerOverflow for numerator overflow, got: {other:?}"),
    }
}

#[test]
fn apply_binop_div_non_terminating_decimal() {
    // 5 / 2 = 2.5 — fixed-point division must not truncate the fraction.
    let mut ic = 0u64;
    let l = 5 * crate::core::types::DECIMAL_SCALE;
    let r = 2 * crate::core::types::DECIMAL_SCALE;
    let result = super::super::apply_binop(
        &BinOp::Div,
        Value::Decimal(l),
        Value::Decimal(r),
        &mut ic,
        10_000,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Decimal(crate::core::types::DECIMAL_SCALE * 5 / 2)
    );
}

#[test]
fn apply_binop_div_by_zero() {
    let mut ic = 0u64;
    let result = super::super::apply_binop(
        &BinOp::Div,
        Value::Decimal(10 * crate::core::types::DECIMAL_SCALE),
        Value::Decimal(0),
        &mut ic,
        10_000,
    );
    assert!(
        matches!(result, Err(MizuError::DivisionByZero)),
        "expected DivisionByZero, got: {result:?}"
    );
}
