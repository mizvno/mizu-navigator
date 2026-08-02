//! Tests for `eval.rs`: `Evaluator` scoping, `FieldAccess` evaluation, every
//! built-in function (`filter`/`count`/`sort`/`length`/`to_string`/
//! `contains`/`has_field`/`get_system_time`), instruction/depth budgets, and
//! `compare_values`/`variant_weight` (the ordering `filter`/`sort` build on).
//!
//! Built-ins live here rather than in a separate file because they *are*
//! `FunctionCall` arms inside `Evaluator::evaluate` — there is no module
//! boundary between "built-in function" and "the evaluator" to split along.

use super::*;

#[test]
fn state_machine_get_local_o1_shadowing() {
    let mut sm = Evaluator::new(crate::core::config::CONFIG.max_instructions);
    let mut interner = StringInterner::default();
    let x = interner.get_or_intern("x");
    let y = interner.get_or_intern("y");

    sm.push_local(x, Value::Decimal(1));
    let outer_fp = sm.local_stack.len();

    sm.push_local(x, Value::Decimal(2));

    assert_eq!(
        sm.get_local(x, outer_fp),
        Some(&Value::Decimal(2)),
        "inner binding must shadow outer at frame_pointer={outer_fp}"
    );
    // y is not bound in any frame
    assert_eq!(sm.get_local(y, outer_fp), None);

    sm.pop_local();
    assert_eq!(
        sm.get_local(x, 0),
        Some(&Value::Decimal(1)),
        "after pop, outer x=1 must be visible from fp=0"
    );
    // But x is no longer visible from inner_fp (the binding index is below inner_fp)
    assert_eq!(
        sm.get_local(x, outer_fp),
        None,
        "outer binding must not be visible from inner frame_pointer"
    );

    sm.pop_local();
    assert_eq!(sm.get_local(x, 0), None);

    assert!(
        sm.local_index.get(&x).map(|v| v.is_empty()).unwrap_or(true),
        "local_index must be empty after all pops"
    );
}

#[test]
fn state_machine_truncate_locals_removes_index_entries() {
    let mut sm = Evaluator::new(crate::core::config::CONFIG.max_instructions);
    let mut interner = StringInterner::default();
    let a = interner.get_or_intern("a");
    let b = interner.get_or_intern("b");

    let fp = sm.local_stack.len();
    sm.push_local(a, Value::Decimal(10));
    sm.push_local(b, Value::Decimal(20));

    assert_eq!(sm.get_local(a, fp), Some(&Value::Decimal(10)));
    assert_eq!(sm.get_local(b, fp), Some(&Value::Decimal(20)));

    sm.truncate_locals(fp);

    assert_eq!(sm.get_local(a, fp), None, "a must be gone after truncate");
    assert_eq!(sm.get_local(b, fp), None, "b must be gone after truncate");
    assert!(sm.local_stack.is_empty());
    assert!(sm.local_index.get(&a).map(|v| v.is_empty()).unwrap_or(true));
    assert!(sm.local_index.get(&b).map(|v| v.is_empty()).unwrap_or(true));
}

#[test]
fn eval_field_access_on_record() {
    use crate::core::types::Symbol;
    use crate::parser::logic::{Expr, ExprArena, MizuFunction};
    use rustc_hash::FxHashMap;

    let mut store = VariableStore::new();
    let mut map: Vec<(Arc<str>, Value)> = Vec::new();
    map.push((Arc::from("name"), Value::String(Arc::from("Neko"))));
    store.set("item", Value::record_from_unsorted(map));

    let item_sym = store.interner.get_or_intern("item");
    let mut arena = ExprArena::new();
    let base = arena.alloc(Expr::Variable(item_sym));
    let field_sym = store.interner.get_or_intern("name");
    let mut store = store.freeze();
    let expr = Expr::FieldAccess {
        base,
        field: field_sym,
        field_hash: crate::core::types::hash_field("name"),
    };

    let funcs: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    store.evaluator.instruction_count = 0;
    let result = store
        .evaluator
        .evaluate(&expr, 0, &funcs, &store.interner, &arena);
    assert_eq!(result.unwrap(), Value::String(Arc::from("Neko")));
}

#[test]
fn eval_field_access_missing_field() {
    use crate::core::types::Symbol;
    use crate::parser::logic::{Expr, ExprArena, MizuFunction};
    use rustc_hash::FxHashMap;

    let mut store = VariableStore::new();
    let map: Vec<(Arc<str>, Value)> = Vec::new();
    store.set("item", Value::record_from_unsorted(map));

    let item_sym = store.interner.get_or_intern("item");
    let mut arena = ExprArena::new();
    let base = arena.alloc(Expr::Variable(item_sym));
    let field_sym = store.interner.get_or_intern("missing");
    let mut store = store.freeze();
    let expr = Expr::FieldAccess {
        base,
        field: field_sym,
        field_hash: crate::core::types::hash_field("missing"),
    };

    let funcs: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    store.evaluator.instruction_count = 0;
    let result = store
        .evaluator
        .evaluate(&expr, 0, &funcs, &store.interner, &arena);
    assert!(matches!(result, Err(MizuError::VariableNotFound(_))));
}

#[test]
fn eval_field_access_on_non_record() {
    use crate::core::types::Symbol;
    use crate::parser::logic::{Expr, ExprArena, MizuFunction};
    use rustc_hash::FxHashMap;

    let mut store = VariableStore::new();
    store.set("item", Value::String(Arc::from("hello")));

    let item_sym = store.interner.get_or_intern("item");
    let mut arena = ExprArena::new();
    let base = arena.alloc(Expr::Variable(item_sym));
    let field_sym = store.interner.get_or_intern("field");
    let mut store = store.freeze();
    let expr = Expr::FieldAccess {
        base,
        field: field_sym,
        field_hash: crate::core::types::hash_field("field"),
    };

    let funcs: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    store.evaluator.instruction_count = 0;
    let result = store
        .evaluator
        .evaluate(&expr, 0, &funcs, &store.interner, &arena);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

/// Builds a small list of records for use in built-in tests.
///
/// Records:
///   { done: true,  priority: 3, name: "alpha" }
///   { done: false, priority: 1, name: "beta"  }
///   { done: true,  priority: 2, name: "gamma" }
///   { done: false, priority: 1, name: "delta" }
///   { done: true,  priority: 1, name: "epsilon" }
fn make_task_list() -> Value {
    let rows: &[(&str, bool, i64, &str)] = &[
        ("alpha", true, 3, "alpha"),
        ("beta", false, 1, "beta"),
        ("gamma", true, 2, "gamma"),
        ("delta", false, 1, "delta"),
        ("epsilon", true, 1, "epsilon"),
    ];
    let items: Vec<Value> = rows
        .iter()
        .map(|(name, done, priority, _)| {
            let mut m: Vec<(Arc<str>, Value)> = Vec::new();
            m.push((Arc::from("done"), Value::Bool(*done)));
            m.push((Arc::from("name"), Value::String(Arc::from(*name))));
            m.push((Arc::from("priority"), Value::Decimal(*priority)));
            Value::record_from_unsorted(m)
        })
        .collect();
    Value::List(Arc::new(items))
}

/// Helper: evaluate a FunctionCall built-in via `Evaluator::evaluate`.
fn eval_builtin(
    store: &mut VariableStore,
    name: &str,
    args: Vec<crate::parser::logic::Expr>,
) -> Result<Value, MizuError> {
    use crate::core::types::Symbol;
    use crate::parser::logic::{ExprArena, MizuFunction};
    use rustc_hash::FxHashMap;
    let sym = store
        .interner
        .get(name)
        .unwrap_or_else(|| panic!("`{name}` must be interned before the store is frozen"));
    let mut arena = ExprArena::new();
    let arg_ids: Vec<_> = args.into_iter().map(|a| arena.alloc(a)).collect();
    let (args_start, args_len) = arena.push_args(&arg_ids).unwrap();
    let expr = crate::parser::logic::Expr::FunctionCall {
        name: sym,
        args_start,
        args_len,
    };
    let fns: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    store.evaluator.instruction_count = 0;
    store
        .evaluator
        .evaluate(&expr, 0, &fns, &store.interner, &arena)
}

#[test]
fn test_filter_by_bool() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("done"))),
        Expr::Literal(Value::String(Arc::from("eq"))),
        Expr::Literal(Value::Bool(true)),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 3);
    for item in items.iter() {
        assert_eq!(
            item.get_field(crate::core::types::hash_field("done"), "done"),
            Some(&Value::Bool(true))
        );
    }
}

#[test]
fn test_filter_by_string() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("name"))),
        Expr::Literal(Value::String(Arc::from("eq"))),
        Expr::Literal(Value::String(Arc::from("gamma"))),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].get_field(crate::core::types::hash_field("name"), "name"),
        Some(&Value::String(Arc::from("gamma")))
    );
}

#[test]
fn test_filter_by_num() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("priority"))),
        Expr::Literal(Value::String(Arc::from("eq"))),
        Expr::Literal(Value::Decimal(1)),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 3); // beta, delta, epsilon
}

#[test]
fn test_filter_empty_result() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("priority"))),
        Expr::Literal(Value::String(Arc::from("eq"))),
        Expr::Literal(Value::Decimal(99)),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 0);
}

#[test]
fn test_count_basic() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("count");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("done"))),
        Expr::Literal(Value::Bool(false)),
    ];
    let result = eval_builtin(&mut store, "count", args).unwrap();
    // An element count is an exact integer, not a fixed-point quantity.
    assert_eq!(result, Value::Int(2));
}

#[test]
fn test_sort_asc() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("sort");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("priority"))),
        Expr::Literal(Value::String(Arc::from("asc"))),
    ];
    let result = eval_builtin(&mut store, "sort", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    let priorities: Vec<i64> = items
        .iter()
        .map(|item| {
            if let Some(&Value::Decimal(p)) =
                item.get_field(crate::core::types::hash_field("priority"), "priority")
            {
                p
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(priorities, vec![1, 1, 1, 2, 3]);
}

#[test]
fn test_sort_desc() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("sort");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("priority"))),
        Expr::Literal(Value::String(Arc::from("desc"))),
    ];
    let result = eval_builtin(&mut store, "sort", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    let priorities: Vec<i64> = items
        .iter()
        .map(|item| {
            if let Some(&Value::Decimal(p)) =
                item.get_field(crate::core::types::hash_field("priority"), "priority")
            {
                p
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(priorities, vec![3, 2, 1, 1, 1]);
}

#[test]
fn test_sort_string() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("sort");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("name"))),
        Expr::Literal(Value::String(Arc::from("asc"))),
    ];
    let result = eval_builtin(&mut store, "sort", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    let names: Vec<String> = items
        .iter()
        .map(|item| {
            if let Some(Value::String(s)) =
                item.get_field(crate::core::types::hash_field("name"), "name")
            {
                s.to_string()
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(names, vec!["alpha", "beta", "delta", "epsilon", "gamma"]);
}

#[test]
fn test_sort_direction_keyword_is_never_shadowed_by_a_real_variable() {
    // Regression test for the fixed bug: `sort`'s third argument used to
    // be evaluated as a normal expression *except* when its resolved
    // variable name happened to be literally "asc"/"desc", in which case
    // the keyword interpretation silently won over the variable's real
    // value. Now `asc`/`desc` in this position are hard parse-time
    // keywords (parse_sort_call_args) — never `Expr::Variable` at all —
    // so a real variable named `asc`, bound to something completely
    // unrelated to sort direction, must have zero effect on the result.
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    // A real variable literally named `asc`, bound to a value that is
    // not `"asc"`/`"desc"` at all — if the old bug were still present in
    // spirit, this is the exact scenario it would have mishandled.
    store.set("asc", Value::Decimal(999));
    let mut store = store.freeze();

    let result = eval_parsed(&mut store, r#"sort(tasks, "priority", asc)"#).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    let priorities: Vec<i64> = items
        .iter()
        .map(
            |item| match item.get_field(crate::core::types::hash_field("priority"), "priority") {
                Some(&Value::Decimal(p)) => p,
                _ => panic!(),
            },
        )
        .collect();
    assert_eq!(
        priorities,
        vec![1, 1, 1, 2, 3],
        "sort(tasks, \"priority\", asc) must sort ascending regardless of \
             the unrelated in-scope variable named `asc`"
    );

    // The variable itself must still be untouched/unread by this call —
    // sort's third argument never becomes a variable lookup at all.
    assert_eq!(store.get("asc").unwrap(), &Value::Decimal(999));
}

/// Parses `src` as a standalone expression through the real parser (so
/// it exercises `parse_filter_call_args`/`parse_sort_call_args`'s
/// keyword-recognition logic, unlike `eval_builtin`'s hand-built
/// `Expr::FunctionCall`) and evaluates it against `store`.
fn eval_parsed(store: &mut VariableStore, src: &str) -> Result<Value, MizuError> {
    use crate::core::types::StringInterner;
    use crate::parser::logic::{MizuFunction, parse_expr_standalone};
    use rustc_hash::FxHashMap;

    // `store` is frozen, but the parser must be able to mint symbols for names
    // that appear only in `src`. Parse against a scratch table seeded from the
    // frozen one: seeding copies the table verbatim, so every pre-existing
    // Symbol keeps its ID and the parsed tree still agrees with the store's
    // globals. Only the scratch table grows, and it dies with this call.
    let mut scratch = StringInterner {
        map: store.interner.map.clone(),
        vec: store.interner.vec.clone(),
    };
    let expr_tree = parse_expr_standalone(src, &mut scratch)?;
    let scratch = scratch.freeze();

    let fns: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    store.evaluator.instruction_count = 0;
    store
        .evaluator
        .evaluate(expr_tree.root(), 0, &fns, &scratch, &expr_tree.arena)
}

#[test]
fn test_filter_op_ne() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("priority"))),
        Expr::Literal(Value::String(Arc::from("ne"))),
        Expr::Literal(Value::Decimal(1)),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 2, "alpha (3) and gamma (2) have priority != 1");
}

#[test]
fn test_filter_op_lt() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("name"))),
        Expr::Literal(Value::String(Arc::from("lt"))),
        Expr::Literal(Value::String(Arc::from("delta"))),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 2, "alpha, beta < \"delta\" lexicographically");
}

#[test]
fn test_filter_op_le() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("name"))),
        Expr::Literal(Value::String(Arc::from("le"))),
        Expr::Literal(Value::String(Arc::from("delta"))),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 3, "alpha, beta, delta <= \"delta\"");
}

#[test]
fn test_filter_op_gt() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("name"))),
        Expr::Literal(Value::String(Arc::from("gt"))),
        Expr::Literal(Value::String(Arc::from("delta"))),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(
        items.len(),
        2,
        "gamma, epsilon > \"delta\" lexicographically"
    );
}

#[test]
fn test_filter_op_ge() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("name"))),
        Expr::Literal(Value::String(Arc::from("ge"))),
        Expr::Literal(Value::String(Arc::from("delta"))),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 3, "delta, gamma, epsilon >= \"delta\"");
}

#[test]
fn test_filter_op_contains() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("name"))),
        Expr::Literal(Value::String(Arc::from("contains"))),
        Expr::Literal(Value::String(Arc::from("am"))),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 1, "only \"gamma\" contains \"am\"");
    assert_eq!(
        items[0].get_field(crate::core::types::hash_field("name"), "name"),
        Some(&Value::String(Arc::from("gamma")))
    );
}

#[test]
fn test_filter_three_argument_form_still_behaves_as_eq() {
    // Backward compatibility: the pre-existing 3-argument surface
    // syntax `filter(list, field, value)` must still parse and behave
    // identically to the explicit 4-argument `op = eq` form — this
    // drives the *real* parser (parse_filter_call_args), not a
    // hand-built Expr, so it actually exercises the 3-arg desugaring.
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let mut store = store.freeze();

    let three_arg = eval_parsed(&mut store, r#"filter(tasks, "done", true)"#).unwrap();
    let four_arg = eval_parsed(&mut store, r#"filter(tasks, "done", eq, true)"#).unwrap();
    assert_eq!(three_arg, four_arg);

    let Value::List(ref items) = three_arg else {
        panic!("expected list")
    };
    assert_eq!(items.len(), 3, "alpha, gamma, epsilon have done = true");
}

#[test]
fn test_filter_on_non_list() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("not_a_list", Value::Decimal(42));
    let sym = store.interner.get_or_intern("not_a_list");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(sym),
        Expr::Literal(Value::String(Arc::from("field"))),
        Expr::Literal(Value::String(Arc::from("eq"))),
        Expr::Literal(Value::Bool(true)),
    ];
    let result = eval_builtin(&mut store, "filter", args);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

/// Build a list of `n` records each containing a single int field `v`.
fn make_large_list(n: usize) -> Value {
    let items: Vec<Value> = (0..n)
        .map(|i| {
            let mut m: Vec<(Arc<str>, Value)> = Vec::new();
            m.push((Arc::from("v"), Value::Decimal(i as i64)));
            Value::record_from_unsorted(m)
        })
        .collect();
    Value::List(Arc::new(items))
}

#[test]
fn test_filter_large_list_triggers_timeout() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("big", make_large_list(25_000));
    let sym = store.interner.get_or_intern("big");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(sym),
        Expr::Literal(Value::String(Arc::from("v"))),
        Expr::Literal(Value::String(Arc::from("eq"))),
        Expr::Literal(Value::Decimal(1)),
    ];
    let result = eval_builtin(&mut store, "filter", args);
    assert!(
        matches!(result, Err(MizuError::Timeout)),
        "filter on 25 000-element list must return Timeout, got: {result:?}"
    );
}

#[test]
fn test_count_large_list_triggers_timeout() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("big", make_large_list(25_000));
    let sym = store.interner.get_or_intern("big");
    store.interner.get_or_intern("count");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(sym),
        Expr::Literal(Value::String(Arc::from("v"))),
        Expr::Literal(Value::Decimal(1)),
    ];
    let result = eval_builtin(&mut store, "count", args);
    assert!(
        matches!(result, Err(MizuError::Timeout)),
        "count on 25 000-element list must return Timeout, got: {result:?}"
    );
}

#[test]
fn test_sort_large_list_triggers_timeout() {
    use crate::parser::logic::Expr;
    // n=2000: log2_n = usize::BITS - 2000_usize.leading_zeros() = 11
    // sorting_cost = 2000 * 11 = 22_000 > the instruction budget(20_000) → Timeout.
    let mut store = VariableStore::new();
    store.set("big", make_large_list(2_000));
    let sym = store.interner.get_or_intern("big");
    store.interner.get_or_intern("sort");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(sym),
        Expr::Literal(Value::String(Arc::from("v"))),
        Expr::Literal(Value::String(Arc::from("asc"))),
    ];
    let result = eval_builtin(&mut store, "sort", args);
    assert!(
        matches!(result, Err(MizuError::Timeout)),
        "sort on 2 000-element list must return Timeout, got: {result:?}"
    );
}

#[test]
fn string_concat_doubling_chain_triggers_timeout_early() {
    // Reproduces the exponential-doubling bypass: a chain of nested
    // `let`s each doubling a string (`let s = s + s in …`). Before the
    // concat charge, this was bounded only by MAX_EVAL_DEPTH (256) and
    // would reach gigabyte-scale strings within ~30-40 levels while
    // burning under 1% of the nominal instruction budget. With the
    // concat charge, cumulative cost after k doublings from a
    // 1-byte seed is 2*(2^k - 1) instructions, which exceeds
    // the instruction budget (20 000) around k≈14 — so 40 levels (well under
    // the 256-level depth guard, and nowhere near problematic string
    // sizes) must already time out.
    use crate::parser::logic::{BinOp, Expr, ExprArena};
    use rustc_hash::FxHashMap;

    let mut store = VariableStore::new();
    let sym = store.interner.get_or_intern("s");
    let mut store = store.freeze();

    let mut arena = ExprArena::new();
    let mut body = Expr::Variable(sym);
    for _ in 0..40 {
        let left = arena.alloc(Expr::Variable(sym));
        let right = arena.alloc(Expr::Variable(sym));
        let double_val = Expr::BinaryOp {
            left,
            op: BinOp::Add,
            right,
        };
        let value = arena.alloc(double_val);
        let body_id = arena.alloc(body);
        body = Expr::Let {
            name: sym,
            value,
            body: body_id,
        };
    }
    let value = arena.alloc(Expr::Literal(Value::String(Arc::from("a"))));
    let body_id = arena.alloc(body);
    let ast = Expr::Let {
        name: sym,
        value,
        body: body_id,
    };

    store.evaluator.instruction_count = 0;
    store.evaluator.eval_depth = 0;
    let fns = FxHashMap::default();
    let result = store
        .evaluator
        .evaluate(&ast, 0, &fns, &store.interner, &arena);

    assert!(
        matches!(result, Err(MizuError::Timeout)),
        "40-level string-doubling chain must hit the instruction budget \
             (around level 14) instead of completing, got: {result:?}"
    );
}

#[test]
fn test_filter_small_list_still_works() {
    // The budget charge must not break normal-sized lists.
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list()); // 5 elements
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("filter");
    let mut store = store.freeze();
    let args = vec![
        Expr::Variable(tasks_sym),
        Expr::Literal(Value::String(Arc::from("done"))),
        Expr::Literal(Value::String(Arc::from("eq"))),
        Expr::Literal(Value::Bool(true)),
    ];
    let result = eval_builtin(&mut store, "filter", args).unwrap();
    let Value::List(ref items) = result else {
        panic!("expected list")
    };
    assert_eq!(
        items.len(),
        3,
        "filter of 5-element list must still succeed"
    );
}

#[test]
fn test_length_of_list() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("length");
    let mut store = store.freeze();
    let result = eval_builtin(&mut store, "length", vec![Expr::Variable(tasks_sym)]).unwrap();
    assert_eq!(result, Value::Int(5));
}

#[test]
fn test_length_of_string_counts_chars_not_bytes() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("length");
    let mut store = store.freeze();
    // "héllo": 5 Unicode scalar values, but 6 UTF-8 bytes ('é' is 2 bytes)
    // — proves this counts chars, not bytes.
    let args = vec![Expr::Literal(Value::String(Arc::from("héllo")))];
    let result = eval_builtin(&mut store, "length", args).unwrap();
    assert_eq!(result, Value::Int(5));
}

#[test]
fn test_length_type_error() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("length");
    let mut store = store.freeze();
    let args = vec![Expr::Literal(Value::Decimal(1))];
    let result = eval_builtin(&mut store, "length", args);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn test_length_large_string_triggers_timeout() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("length");
    let mut store = store.freeze();
    let big = "a".repeat(25_000);
    let args = vec![Expr::Literal(Value::String(Arc::from(big)))];
    let result = eval_builtin(&mut store, "length", args);
    assert!(
        matches!(result, Err(MizuError::Timeout)),
        "length() on a 25 000-char string must return Timeout, got: {result:?}"
    );
}

#[test]
fn test_to_string_int() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("to_string");
    let mut store = store.freeze();
    // 3.5 in fixed-point representation.
    let n = 3 * DECIMAL_SCALE + DECIMAL_SCALE / 2;
    let args = vec![Expr::Literal(Value::Decimal(n))];
    let result = eval_builtin(&mut store, "to_string", args).unwrap();
    assert_eq!(result, Value::String(Arc::from("3.5")));
}

#[test]
fn test_to_string_bool() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("to_string");
    let mut store = store.freeze();
    let args = vec![Expr::Literal(Value::Bool(true))];
    let result = eval_builtin(&mut store, "to_string", args).unwrap();
    assert_eq!(result, Value::String(Arc::from("true")));
}

#[test]
fn test_to_string_type_error_on_string() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("to_string");
    let mut store = store.freeze();
    let args = vec![Expr::Literal(Value::String(Arc::from("already a string")))];
    let result = eval_builtin(&mut store, "to_string", args);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn test_to_string_type_error_on_list() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.set("tasks", make_task_list());
    let tasks_sym = store.interner.get_or_intern("tasks");
    store.interner.get_or_intern("to_string");
    let mut store = store.freeze();
    let result = eval_builtin(&mut store, "to_string", vec![Expr::Variable(tasks_sym)]);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn test_contains_true() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("contains");
    let mut store = store.freeze();
    let args = vec![
        Expr::Literal(Value::String(Arc::from("hello world"))),
        Expr::Literal(Value::String(Arc::from("wor"))),
    ];
    let result = eval_builtin(&mut store, "contains", args).unwrap();
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn test_contains_false() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("contains");
    let mut store = store.freeze();
    let args = vec![
        Expr::Literal(Value::String(Arc::from("hello world"))),
        Expr::Literal(Value::String(Arc::from("xyz"))),
    ];
    let result = eval_builtin(&mut store, "contains", args).unwrap();
    assert_eq!(result, Value::Bool(false));
}

#[test]
fn test_contains_type_error() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("contains");
    let mut store = store.freeze();
    let args = vec![
        Expr::Literal(Value::Decimal(1)),
        Expr::Literal(Value::String(Arc::from("x"))),
    ];
    let result = eval_builtin(&mut store, "contains", args);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

#[test]
fn test_contains_large_haystack_triggers_timeout() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("contains");
    let mut store = store.freeze();
    let big = "a".repeat(25_000);
    let args = vec![
        Expr::Literal(Value::String(Arc::from(big))),
        Expr::Literal(Value::String(Arc::from("zzz"))),
    ];
    let result = eval_builtin(&mut store, "contains", args);
    assert!(
        matches!(result, Err(MizuError::Timeout)),
        "contains() over a 25 000-byte haystack must return Timeout, got: {result:?}"
    );
}

/// The first record in `make_task_list()`'s underlying list.
fn first_task_record() -> Value {
    match make_task_list() {
        Value::List(ref items) => items[0].clone(),
        _ => panic!("expected list"),
    }
}

#[test]
fn test_has_field_present() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("has_field");
    let mut store = store.freeze();
    let args = vec![
        Expr::Literal(first_task_record()),
        Expr::Literal(Value::String(Arc::from("priority"))),
    ];
    let result = eval_builtin(&mut store, "has_field", args).unwrap();
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn test_has_field_absent() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("has_field");
    let mut store = store.freeze();
    let args = vec![
        Expr::Literal(first_task_record()),
        Expr::Literal(Value::String(Arc::from("nonexistent_field"))),
    ];
    let result = eval_builtin(&mut store, "has_field", args).unwrap();
    assert_eq!(result, Value::Bool(false));
}

#[test]
fn test_has_field_type_error() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("has_field");
    let mut store = store.freeze();
    let args = vec![
        Expr::Literal(Value::Decimal(1)),
        Expr::Literal(Value::String(Arc::from("field"))),
    ];
    let result = eval_builtin(&mut store, "has_field", args);
    assert!(matches!(result, Err(MizuError::TypeError { .. })));
}

// ────────────────────────────────────────────────────────────────────────
// get_system_time — dynamic write-target closed (RM-04)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn get_system_time_bare_variable_queues_correct_target() {
    use crate::messages::RuntimeAction;
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    let target_sym = store.interner.get_or_intern("elapsed");
    store.interner.get_or_intern("get_system_time");
    let mut store = store.freeze();
    let args = vec![Expr::Variable(target_sym)];
    let result = eval_builtin(&mut store, "get_system_time", args).unwrap();
    assert_eq!(result, Value::Bool(true));
    assert_eq!(store.evaluator.accumulated_actions.len(), 1);
    match &store.evaluator.accumulated_actions[0] {
        RuntimeAction::GetSystemTime { target_variable } => {
            assert_eq!(*target_variable, target_sym);
        }
        other => panic!("expected GetSystemTime, got: {other:?}"),
    }
}

#[test]
fn get_system_time_non_variable_arg_rejected_at_runtime() {
    // Defense in depth: even if an `Expr::FunctionCall` for
    // get_system_time were constructed directly (bypassing the parser's
    // own bare-identifier restriction — e.g. from a future code path,
    // or a test), the evaluator itself must still reject a target that
    // isn't a bare Symbol fixed at construction time. This is exactly
    // the shape the pre-fix code accepted: an expression (here a
    // literal, but conceptually `$form.x`) evaluated at runtime to pick
    // the write target.
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    store.interner.get_or_intern("get_system_time");
    let mut store = store.freeze();
    let args = vec![Expr::Literal(Value::String(Arc::from("evil_target")))];
    let err = eval_builtin(&mut store, "get_system_time", args).unwrap_err();
    assert!(
        matches!(err, MizuError::ExecutionError(_)),
        "expected ExecutionError for a non-bare-identifier target, got: {err:?}"
    );
    assert!(
        store.evaluator.accumulated_actions.is_empty(),
        "a rejected target must not queue a GetSystemTime action"
    );
}

#[test]
fn get_system_time_computed_variable_target_rejected_at_runtime() {
    use crate::parser::logic::Expr;
    let mut store = VariableStore::new();
    let comp_sym = store.interner.get_or_intern("derived");
    store.interner.get_or_intern("get_system_time");
    let mut store = store.freeze();
    store.evaluator.computed_var_syms.insert(comp_sym);
    let args = vec![Expr::Variable(comp_sym)];
    let err = eval_builtin(&mut store, "get_system_time", args).unwrap_err();
    assert!(
        matches!(err, MizuError::ExecutionError(_)),
        "expected ExecutionError when targeting a computed variable, got: {err:?}"
    );
}

#[test]
fn test_strict_weak_ordering_heterogeneous() {
    // Records where the sorted field contains different Value variants.
    // Before the fix, heterogeneous pairs collapsed to Equal, violating
    // transitivity and causing undefined sort behaviour.
    let mut items = vec![
        // score: String("hello")  — variant weight 4
        {
            let mut m: Vec<(Arc<str>, Value)> = Vec::new();
            m.push((Arc::from("score"), Value::String(Arc::from("hello"))));
            Value::record_from_unsorted(m)
        },
        // score: Int(10)  — variant weight 3
        {
            let mut m: Vec<(Arc<str>, Value)> = Vec::new();
            m.push((Arc::from("score"), Value::Decimal(10)));
            Value::record_from_unsorted(m)
        },
        // score: Int(1)  — variant weight 3, lower numeric value
        {
            let mut m: Vec<(Arc<str>, Value)> = Vec::new();
            m.push((Arc::from("score"), Value::Decimal(1)));
            Value::record_from_unsorted(m)
        },
    ];

    // Must not panic; the comparator must be a valid strict-weak order.
    items.sort_by(|a, b| compare_values(field_value(a, "score"), field_value(b, "score")));

    // Expected: Int(1) < Int(10) < String("hello")
    // (all Ints have weight 3 < String weight 4; within Ints, 1 < 10)
    let scores: Vec<String> = items
        .iter()
        .map(|item| {
            item.get_field(crate::core::types::hash_field("score"), "score")
                .map(|v| match v {
                    Value::Decimal(n) => n.to_string(),
                    Value::String(s) => s.to_string(),
                    _ => "?".to_string(),
                })
                .unwrap_or_else(|| "?".to_string())
        })
        .collect();

    assert_eq!(
        scores,
        vec!["1", "10", "hello"],
        "heterogeneous sort must be stable, deterministic, and panic-free: {scores:?}"
    );
}

#[test]
fn test_variant_weight_ordering() {
    // None < Null < Bool < Int < String < List < Record
    assert!(variant_weight(&Value::Null) < variant_weight(&Value::Bool(true)));
    assert!(variant_weight(&Value::Bool(true)) < variant_weight(&Value::Decimal(0)));
    assert!(variant_weight(&Value::Decimal(0)) < variant_weight(&Value::String(Arc::from(""))));
    assert!(
        variant_weight(&Value::String(Arc::from("")))
            < variant_weight(&Value::List(Arc::new(vec![])))
    );
    assert!(
        variant_weight(&Value::List(Arc::new(vec![])))
            < variant_weight(&Value::record_from_unsorted(Vec::<(Arc<str>, Value)>::new()))
    );
}

#[test]
fn test_none_is_less_than_some() {
    use std::cmp::Ordering;
    assert_eq!(compare_values(None, Some(&Value::Null)), Ordering::Less);
    assert_eq!(
        compare_values(None, Some(&Value::Decimal(0))),
        Ordering::Less
    );
    assert_eq!(
        compare_values(Some(&Value::Decimal(0)), None),
        Ordering::Greater
    );
    assert_eq!(compare_values(None::<&Value>, None), Ordering::Equal);
}

#[test]
fn eval_depth_guard() {
    // evaluate_impl is a large function; in debug mode each call frame can
    // be several KB. With MAX_EVAL_DEPTH=256 the guard fires after
    // 257 × evaluate + 256 × evaluate_impl frames, which can approach the
    // 2 MB default test-thread stack. Run this test in a thread with an
    // explicitly enlarged stack so it works in both debug and release builds.
    let handle = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024) // 16 MB
        .spawn(|| {
            use crate::core::errors::MizuError;
            use crate::parser::logic::{BinOp, Expr, ExprArena};
            use rustc_hash::FxHashMap;

            // Build a 300-level deep BinaryOp chain entirely in Rust.
            // The parser would reject this before evaluation, so we bypass
            // it to test the evaluator's own depth guard directly.
            let mut arena = ExprArena::new();
            let mut ast = Expr::Literal(Value::Decimal(0));
            for _ in 0..300 {
                let left = arena.alloc(ast);
                let right = arena.alloc(Expr::Literal(Value::Decimal(0)));
                ast = Expr::BinaryOp {
                    left,
                    op: BinOp::Add,
                    right,
                };
            }

            let mut store = VariableStore::new();
            let mut store = store.freeze();
            store.evaluator.instruction_count = 0;
            store.evaluator.eval_depth = 0;
            let fns = FxHashMap::default();

            let result = store
                .evaluator
                .evaluate(&ast, 0, &fns, &store.interner, &arena);
            match result {
                Err(MizuError::ExecutionError(msg)) => {
                    assert!(
                        msg.contains("nesting too deep"),
                        "error must mention nesting depth: {msg}"
                    );
                }
                Err(MizuError::Timeout) => {} // budget may expire first — also acceptable
                Ok(_) => panic!("expected depth error for 300-level AST, got Ok"),
                Err(other) => panic!("unexpected error variant: {other:?}"),
            }
        })
        .expect("thread spawn must succeed");

    handle
        .join()
        .expect("depth-guard test thread must not panic");
}

/// Cross-function composition of `MAX_EVAL_DEPTH`.
///
/// [`crate::parser::logic::MAX_PARSE_DEPTH`] (256) bounds nesting depth
/// **per expression tree parsed in isolation** — a function body is one
/// such tree, and the expression at a call site is another. Nothing at
/// parse time prevents a ~250-level-deep function body from being
/// invoked from within a call-site expression that is itself nested
/// several levels deep, which would compose to a total `evaluate()`
/// recursion depth exceeding 256 even though neither individual tree
/// violates `MAX_PARSE_DEPTH`.
///
/// This test builds exactly that scenario directly on the AST (bypassing
/// the parser, as `eval_depth_guard` above does) and checks that
/// `eval_depth` — which is a single running counter on `Evaluator`,
/// never reset at a function-call boundary (only `local_stack` is
/// truncated there, see the `Expr::FunctionCall` arm of `evaluate_impl`)
/// — still fires cleanly.
///
/// Unlike `eval_depth_guard`, this test deliberately does **not** run on
/// an arbitrarily-generous stack. Production's `LogicWorker`
/// (`parser::logic_worker::LogicWorker::spawn`) evaluates on a thread
/// started with an explicit
/// [`crate::parser::logic_worker::LogicWorker::STACK_SIZE_BYTES`]-sized
/// stack (16 MiB) — so this test re-execs the test binary as a child
/// process and runs the scenario on a thread built with that exact same
/// constant, to determine whether the depth guard reliably wins the race
/// against native stack exhaustion under the conditions production
/// actually runs under, rather than under the artificially generous
/// conditions of `eval_depth_guard`. A real native stack overflow aborts
/// the process (it cannot be caught with `catch_unwind`), so this has to
/// be observed from a parent process inspecting the child's exit status.
#[test]
fn cross_function_composition_depth_guard() {
    const CHILD_ENV: &str = "MIZU_DEPTH_COMPOSITION_CHILD";
    const OK_MARKER: &str = "DEPTH_GUARD_FIRED_CLEANLY";

    if std::env::var_os(CHILD_ENV).is_some() {
        run_cross_function_composition_child(OK_MARKER);
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .arg("core::types::tests::eval::cross_function_composition_depth_guard")
        .arg("--exact")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_ENV, "1")
        .output()
        .expect("failed to spawn child test process");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success() && stdout.contains(OK_MARKER),
        "cross-function eval_depth composition did not cleanly hit the \
             MAX_EVAL_DEPTH guard on a default-size thread (status={:?}). \
             This may indicate a native stack overflow occurring before the \
             eval_depth check can intervene, which would be a SEPARATE, \
             more serious finding than a missing guard.\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
        output.status,
        stdout,
        stderr
    );
}

/// Runs the actual cross-function composition scenario on a thread built
/// with the same `stack_size` production's `LogicWorker::spawn` uses
/// ([`crate::parser::logic_worker::LogicWorker::STACK_SIZE_BYTES`]), and
/// prints `ok_marker` iff `evaluate` returned the expected
/// `MAX_EVAL_DEPTH` error rather than panicking, hanging, or (silently,
/// from this process's point of view) crashing.
fn run_cross_function_composition_child(ok_marker: &'static str) {
    use crate::parser::logic_worker::LogicWorker;

    let handle = std::thread::Builder::new()
        .stack_size(LogicWorker::STACK_SIZE_BYTES)
        .spawn(move || run_cross_function_composition_scenario(ok_marker))
        .expect("thread spawn must succeed");

    handle
        .join()
        .expect("composition scenario thread must not panic");
}

/// The actual cross-function composition scenario, run on whatever
/// thread `run_cross_function_composition_child` builds.
fn run_cross_function_composition_scenario(ok_marker: &str) {
    use crate::parser::logic::{BinOp, Expr, ExprArena, ExprTree, MizuFunction};
    use rustc_hash::FxHashMap;

    let mut store = VariableStore::new();
    let param = store.interner.get_or_intern("x");
    let func_sym = store.interner.get_or_intern("deeply_nested_fn");
    let mut store = store.freeze();

    // Function body: ~250 levels of BinaryOp nesting -- representative
    // of the deepest single expression tree the parser will accept
    // under MAX_PARSE_DEPTH (256) for a function body parsed on its own.
    let mut body_arena = ExprArena::new();
    let mut body = Expr::Variable(param);
    for _ in 0..250 {
        let left = body_arena.alloc(body);
        let right = body_arena.alloc(Expr::Literal(Value::Decimal(0)));
        body = Expr::BinaryOp {
            left,
            op: BinOp::Add,
            right,
        };
    }
    let body_root = body_arena.alloc(body);
    let func = MizuFunction {
        params: vec![(param, crate::parser::logic::ValueType::Num)],
        body: ExprTree {
            arena: body_arena,
            root: body_root,
        },
    };
    let mut functions = FxHashMap::default();
    functions.insert(func_sym, func);

    // Call-site expression: another ~20 levels of nesting -- itself
    // comfortably under MAX_PARSE_DEPTH on its own -- wrapping a call
    // to the function above. Neither tree alone violates
    // MAX_PARSE_DEPTH, but composed at evaluation time they exceed
    // MAX_EVAL_DEPTH (256).
    let mut call_arena = ExprArena::new();
    let arg0 = call_arena.alloc(Expr::Literal(Value::Decimal(1)));
    let (args_start, args_len) = call_arena.push_args(&[arg0]).unwrap();
    let mut call_site = Expr::FunctionCall {
        name: func_sym,
        args_start,
        args_len,
    };
    for _ in 0..20 {
        let left = call_arena.alloc(call_site);
        let right = call_arena.alloc(Expr::Literal(Value::Decimal(0)));
        call_site = Expr::BinaryOp {
            left,
            op: BinOp::Add,
            right,
        };
    }

    store.evaluator.instruction_count = 0;
    store.evaluator.eval_depth = 0;

    let result = store
        .evaluator
        .evaluate(&call_site, 0, &functions, &store.interner, &call_arena);

    match result {
        Err(MizuError::ExecutionError(msg)) if msg.contains("nesting too deep") => {
            println!("{ok_marker}");
        }
        // Also acceptable: the instruction budget could in principle be
        // exhausted first depending on constant tuning: still a clean,
        // bounded error, not a crash.
        Err(MizuError::Timeout) => {
            println!("{ok_marker}");
        }
        other => {
            println!("UNEXPECTED_RESULT: {other:?}");
        }
    }
}

/// Measures the real native stack depth required to run a `evaluate()`
/// chain deep enough to trip `MAX_EVAL_DEPTH` (256), in whichever profile
/// the test binary was built under (debug or `--release`).
///
/// The comment on `eval_depth_guard` above only established that debug
/// frames are "several KB" each; it never quantified the release-mode
/// case, where `evaluate`/`evaluate_impl` frames are dramatically
/// smaller after inlining and optimization. Production's `LogicWorker`
/// (`parser::logic_worker::LogicWorker::spawn`) always runs in whatever
/// profile the binary was built under, so a release-only guess is not
/// good enough either — this test probes a fixed ladder of candidate
/// stack sizes and, for each, re-execs this same test binary (a real
/// native stack overflow aborts the process and cannot be caught with
/// `catch_unwind`, so it must be observed from a parent process) to run
/// the same 300-level chain used by `cross_function_composition_depth_guard`
/// on a thread built with exactly that `stack_size`. The smallest
/// candidate that survives is the empirical per-profile floor.
///
/// This is a manual measurement tool, not a correctness gate — it is
/// `#[ignore]`d so normal `cargo test` runs stay fast. Run it directly to
/// reproduce the numbers documented next to `LogicWorker::spawn` and in
/// `walkthrough.md`:
///   `cargo test --release --lib core::types::tests::eval::measure_stack_usage_at_max_eval_depth -- --ignored --nocapture`
///   `cargo test          --lib core::types::tests::eval::measure_stack_usage_at_max_eval_depth -- --ignored --nocapture`
#[test]
#[ignore]
fn measure_stack_usage_at_max_eval_depth() {
    const STACK_ENV: &str = "MIZU_STACK_MEASURE_BYTES";
    const OK_MARKER: &str = "STACK_MEASURE_OK";

    if let Some(bytes) = std::env::var_os(STACK_ENV) {
        let stack_size: usize = bytes
            .to_str()
            .expect("env var must be UTF-8")
            .parse()
            .expect("env var must be a valid usize");
        run_stack_measurement_child(stack_size, OK_MARKER);
        return;
    }

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    let exe = std::env::current_exe().expect("current_exe");
    // Doubling ladder from 16 KiB up to 4 MiB covers everywhere a
    // per-frame estimate in the tens-of-KB-to-single-KB range could land,
    // for both debug and release.
    let candidates: &[usize] = &[
        16 * 1024,
        32 * 1024,
        64 * 1024,
        128 * 1024,
        256 * 1024,
        512 * 1024,
        1024 * 1024,
        2 * 1024 * 1024,
        4 * 1024 * 1024,
    ];

    let mut smallest_safe: Option<usize> = None;
    for &size in candidates {
        let output = std::process::Command::new(&exe)
            .arg("core::types::tests::eval::measure_stack_usage_at_max_eval_depth")
            .arg("--exact")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .arg("--ignored")
            .env(STACK_ENV, size.to_string())
            .output()
            .expect("failed to spawn measurement child process");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let safe = output.status.success() && stdout.contains(OK_MARKER);
        println!(
            "[{profile}] stack_size={size} bytes ({:.1} KiB) -> {}",
            size as f64 / 1024.0,
            if safe { "survived" } else { "CRASHED" }
        );
        if safe && smallest_safe.is_none() {
            smallest_safe = Some(size);
        }
    }

    println!(
        "[{profile}] RESULT: smallest tested stack_size that survives a \
             300-level eval_depth chain (exceeds MAX_EVAL_DEPTH=256) = {:?}",
        smallest_safe
    );
}

/// Runs the actual 300-level `evaluate()` chain — identical in shape to
/// `eval_depth_guard` and `cross_function_composition_depth_guard` — on a
/// thread built with exactly `stack_size` bytes, and prints `ok_marker`
/// iff it completes without a native stack overflow (regardless of
/// whether the result is the depth-guard error or a timeout — both are
/// controlled, non-crashing outcomes).
fn run_stack_measurement_child(stack_size: usize, ok_marker: &str) {
    use crate::parser::logic::{BinOp, Expr, ExprArena};
    use rustc_hash::FxHashMap;

    let handle = std::thread::Builder::new()
        .stack_size(stack_size)
        .spawn(|| {
            let mut arena = ExprArena::new();
            let mut ast = Expr::Literal(Value::Decimal(0));
            for _ in 0..300 {
                let left = arena.alloc(ast);
                let right = arena.alloc(Expr::Literal(Value::Decimal(0)));
                ast = Expr::BinaryOp {
                    left,
                    op: BinOp::Add,
                    right,
                };
            }

            let mut store = VariableStore::new();
            let mut store = store.freeze();
            store.evaluator.instruction_count = 0;
            store.evaluator.eval_depth = 0;
            let fns = FxHashMap::default();

            let _ = store
                .evaluator
                .evaluate(&ast, 0, &fns, &store.interner, &arena);
        })
        .expect("thread spawn must succeed");

    handle.join().expect("measurement thread must not panic");
    println!("{ok_marker}");
}

#[test]
fn compare_lists_equal_content() {
    use std::cmp::Ordering;
    let a = Value::List(Arc::new(vec![Value::Decimal(1), Value::Decimal(2)]));
    let b = Value::List(Arc::new(vec![Value::Decimal(1), Value::Decimal(2)]));
    assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Equal);
}

#[test]
fn compare_lists_lexicographic() {
    use std::cmp::Ordering;
    // [1, 3] > [1, 2]
    let a = Value::List(Arc::new(vec![Value::Decimal(1), Value::Decimal(3)]));
    let b = Value::List(Arc::new(vec![Value::Decimal(1), Value::Decimal(2)]));
    assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Greater);
    assert_eq!(compare_values(Some(&b), Some(&a)), Ordering::Less);
}

#[test]
fn compare_lists_shorter_less_than_longer() {
    use std::cmp::Ordering;
    // [1] < [1, 2] (prefix match, shorter is Less)
    let shorter = Value::List(Arc::new(vec![Value::Decimal(1)]));
    let longer = Value::List(Arc::new(vec![Value::Decimal(1), Value::Decimal(2)]));
    assert_eq!(
        compare_values(Some(&shorter), Some(&longer)),
        Ordering::Less
    );
    assert_eq!(
        compare_values(Some(&longer), Some(&shorter)),
        Ordering::Greater
    );
}

#[test]
fn compare_empty_lists_equal() {
    use std::cmp::Ordering;
    let a = Value::List(Arc::new(vec![]));
    let b = Value::List(Arc::new(vec![]));
    assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Equal);
}

#[test]
fn sort_list_of_lists_is_deterministic() {
    // Sorting [[3], [1,2], [1], []] must produce a stable lexicographic order.
    let mut lists = vec![
        Value::List(Arc::new(vec![Value::Decimal(3)])),
        Value::List(Arc::new(vec![Value::Decimal(1), Value::Decimal(2)])),
        Value::List(Arc::new(vec![Value::Decimal(1)])),
        Value::List(Arc::new(vec![])),
    ];
    lists.sort_by(|a, b| compare_values(Some(a), Some(b)));
    // Expected: [] < [1] < [1,2] < [3]
    let lengths: Vec<usize> = lists
        .iter()
        .map(|v| {
            if let Value::List(v) = v {
                v.len()
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(lengths, vec![0, 1, 2, 1]);
    // Verify the last element is [3].
    if let Value::List(last) = lists.last().unwrap() {
        assert_eq!(last.as_slice(), &[Value::Decimal(3)]);
    } else {
        panic!("last element must be a List");
    }
}

#[test]
fn compare_records_equal_content() {
    use std::cmp::Ordering;
    let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
    ma.push((Arc::from("x"), Value::Decimal(1)));
    let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
    mb.push((Arc::from("x"), Value::Decimal(1)));
    let a = Value::record_from_unsorted(ma);
    let b = Value::record_from_unsorted(mb);
    assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Equal);
}

#[test]
fn compare_records_same_keys() {
    use std::cmp::Ordering;
    // { x: 1 } < { x: 2 }
    let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
    ma.push((Arc::from("x"), Value::Decimal(1)));
    let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
    mb.push((Arc::from("x"), Value::Decimal(2)));
    let a = Value::record_from_unsorted(ma);
    let b = Value::record_from_unsorted(mb);
    assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Less);
    assert_eq!(compare_values(Some(&b), Some(&a)), Ordering::Greater);
}

#[test]
fn compare_records_by_key_name() {
    use std::cmp::Ordering;
    // { a: 1 } < { b: 1 } because "a" < "b"
    let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
    ma.push((Arc::from("a"), Value::Decimal(1)));
    let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
    mb.push((Arc::from("b"), Value::Decimal(1)));
    let a = Value::record_from_unsorted(ma);
    let b = Value::record_from_unsorted(mb);
    assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Less);
}

#[test]
fn compare_records_shorter_less_than_longer() {
    use std::cmp::Ordering;
    // { x: 1 } < { x: 1, y: 2 } (same keys up to len, shorter is Less)
    let mut ma: Vec<(Arc<str>, Value)> = Vec::new();
    ma.push((Arc::from("x"), Value::Decimal(1)));
    let mut mb: Vec<(Arc<str>, Value)> = Vec::new();
    mb.push((Arc::from("x"), Value::Decimal(1)));
    mb.push((Arc::from("y"), Value::Decimal(2)));
    let a = Value::record_from_unsorted(ma);
    let b = Value::record_from_unsorted(mb);
    assert_eq!(compare_values(Some(&a), Some(&b)), Ordering::Less);
    assert_eq!(compare_values(Some(&b), Some(&a)), Ordering::Greater);
}

#[test]
fn sort_records_by_single_field_via_compare_values() {
    // Before the fix, sorting a list whose items are themselves Record values
    // (not comparing a field *inside* a Record, but the Record *itself*) would
    // collapse to all-Equal and produce undefined order.
    let mut records: Vec<Value> = (0..4_i64)
        .rev()
        .map(|i| {
            let mut m: Vec<(Arc<str>, Value)> = Vec::new();
            m.push((Arc::from("v"), Value::Decimal(i)));
            Value::record_from_unsorted(m)
        })
        .collect();
    // compare_values on two Records now compares keys then values.
    records.sort_by(|a, b| compare_values(Some(a), Some(b)));
    let vals: Vec<i64> = records
        .iter()
        .map(|r| {
            if let Some(&Value::Decimal(n)) = r.get_field(crate::core::types::hash_field("v"), "v")
            {
                n
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(
        vals,
        vec![0, 1, 2, 3],
        "records must sort by their 'v' field"
    );
}

// ------------------------------------------------------------------
// Task 1 regression: BTreeMap-based Record sort — zero allocation,
// strict weak ordering, correct result on deeply mixed inputs
// ------------------------------------------------------------------

/// Verifies that sorting a list of multi-key records via `compare_values`
/// produces the correct lexicographic order and does not panic.
///
/// With the BTreeMap representation, `compare_values` iterates the two maps
/// in parallel using `Iterator::zip` — no `Vec` allocation, no `sort_unstable`
/// call.  The correctness guarantee is structural: BTreeMap always yields keys
/// in ascending order, so the zip is guaranteed to visit corresponding keys.
#[test]
fn compare_records_btreemap_zero_alloc_sort() {
    use std::cmp::Ordering;

    // Three records with two keys each, in descending insertion order,
    // to verify that BTreeMap's sorted iterator is key-order, not
    // insertion-order.
    let make = |a: i64, b: i64| {
        let mut m: Vec<(Arc<str>, Value)> = Vec::new();
        // Insert in reverse alphabetical order — BTreeMap must still iterate "alpha" first.
        m.push((Arc::from("zeta"), Value::Decimal(b)));
        m.push((Arc::from("alpha"), Value::Decimal(a)));
        m.sort_by(|x, y| x.0.cmp(&y.0));
        Value::record_from_unsorted(m)
    };

    let r1 = make(1, 10); // { alpha:1, zeta:10 }
    let r2 = make(2, 5); // { alpha:2, zeta:5  }
    let r3 = make(1, 20); // { alpha:1, zeta:20 }

    // r1 vs r3: alpha equal, zeta 10 < 20 → r1 < r3
    assert_eq!(compare_values(Some(&r1), Some(&r3)), Ordering::Less);
    // r3 vs r2: alpha 1 < 2 → r3 < r2
    assert_eq!(compare_values(Some(&r3), Some(&r2)), Ordering::Less);
    // Transitivity: r1 < r3 < r2 → sort must yield [r1, r3, r2]
    let mut records = vec![r2.clone(), r1.clone(), r3.clone()];
    records.sort_by(|a, b| compare_values(Some(a), Some(b)));

    // Expected ascending order: r1 { alpha:1, zeta:10 }, r3 { alpha:1, zeta:20 }, r2 { alpha:2, zeta:5 }
    let alpha_vals: Vec<i64> = records
        .iter()
        .map(|r| {
            if let Some(&Value::Decimal(n)) =
                r.get_field(crate::core::types::hash_field("alpha"), "alpha")
            {
                n
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(
        alpha_vals,
        vec![1, 1, 2],
        "BTreeMap record sort must respect key order regardless of insertion order"
    );
}
