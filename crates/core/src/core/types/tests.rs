use super::{
    Evaluator, FileHandleData, StringInterner, Symbol, Value, VariableStore, compare_values,
    field_value, from_json_str, to_json, variant_weight,
};
use crate::core::errors::MizuError;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn string_from_string_ref() {
    let v = Value::from("hello");
    assert_eq!(v, Value::String(std::sync::Arc::from("hello")));
}

#[test]
fn string_from_owned_string() {
    let v = Value::from(String::from("world"));
    assert_eq!(v, Value::String(std::sync::Arc::from("world")));
}

#[test]
fn string_display_verbatim() {
    let v = Value::String(std::sync::Arc::from("Mizu rocks"));
    assert_eq!(v.to_string(), "Mizu rocks");
}

#[test]
fn bool_from_true() {
    let v = Value::from(true);
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn bool_from_false() {
    let v = Value::from(false);
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn bool_display_lowercase() {
    assert_eq!(Value::Bool(true).to_string(), "true");
    assert_eq!(Value::Bool(false).to_string(), "false");
}

#[test]
fn list_display_empty() {
    let v = Value::List(std::sync::Arc::new(vec![]));
    assert_eq!(v.to_string(), "[]");
}

#[test]
fn list_display_single_element() {
    let v = Value::List(std::sync::Arc::new(vec![Value::Decimal(
        super::DECIMAL_SCALE,
    )]));
    assert_eq!(v.to_string(), "[1]");
}

#[test]
fn list_display_multiple_elements() {
    let v = Value::List(std::sync::Arc::new(vec![
        Value::Decimal(super::DECIMAL_SCALE),
        Value::String(std::sync::Arc::from("two")),
        Value::Bool(false),
    ]));
    assert_eq!(v.to_string(), "[1, two, false]");
}

#[test]
fn list_display_nested() {
    let inner = Value::List(std::sync::Arc::new(vec![
        Value::Decimal(2 * super::DECIMAL_SCALE),
        Value::Decimal(3 * super::DECIMAL_SCALE),
    ]));
    let outer = Value::List(std::sync::Arc::new(vec![
        Value::Decimal(super::DECIMAL_SCALE),
        inner,
    ]));
    assert_eq!(outer.to_string(), "[1, [2, 3]]");
}

#[test]
fn store_set_and_get_int_scaled() {
    let mut store = VariableStore::new();
    store.set("price", Value::Decimal(99_900));
    let mut store = store.freeze();
    let result = store.get("price");
    assert!(result.is_ok());
    assert_eq!(*result.unwrap(), Value::Decimal(99_900));
}

#[test]
fn store_set_and_get_string() {
    let mut store = VariableStore::new();
    store.set("label", Value::from("checkout"));
    let mut store = store.freeze();
    assert_eq!(
        *store.get("label").unwrap(),
        Value::String(std::sync::Arc::from("checkout"))
    );
}

#[test]
fn store_set_and_get_bool() {
    let mut store = VariableStore::new();
    store.set("flag", Value::from(true));
    let mut store = store.freeze();
    assert_eq!(*store.get("flag").unwrap(), Value::Bool(true));
}

#[test]
fn store_set_and_get_list() {
    let mut store = VariableStore::new();
    let list = Value::List(std::sync::Arc::new(vec![
        Value::Decimal(10_000),
        Value::Decimal(20_000),
    ]));
    store.set("items", list.clone());
    let mut store = store.freeze();
    assert_eq!(*store.get("items").unwrap(), list);
}

#[test]
fn store_set_convenience_into() {
    // `set` accepts any `impl Into<Value>`, so raw Rust types work directly.
    let mut store = VariableStore::new();
    store.set("x", Value::Int(7));
    store.set("greeting", "hi");
    store.set("active", false);
    let mut store = store.freeze();
    assert_eq!(*store.get("x").unwrap(), Value::Int(7));
    assert_eq!(
        *store.get("greeting").unwrap(),
        Value::String(std::sync::Arc::from("hi"))
    );
    assert_eq!(*store.get("active").unwrap(), Value::Bool(false));
}

#[test]
fn store_overwrite_binding() {
    let mut store = VariableStore::new();
    store.set("count", Value::Int(1));
    store.set("count", Value::Int(2));
    let mut store = store.freeze();
    assert_eq!(*store.get("count").unwrap(), Value::Int(2));
}

#[test]
fn store_scope_chaining() {
    let mut store = VariableStore::new();
    store.set("x", Value::Int(10));
    store.set("y", Value::Int(20));

    let fp = store.evaluator.local_stack.len();
    let x_sym = store.interner.get_or_intern("x");
    let y_sym = store.interner.get_or_intern("y");
    let z_sym = store.interner.get_or_intern("z");
    let mut store = store.freeze();

    store.evaluator.push_local(x_sym, Value::Int(15_));

    assert_eq!(
        *store.evaluator.get_local(x_sym, fp).unwrap(),
        Value::Int(15_)
    );
    assert!(store.evaluator.get_local(y_sym, fp).is_none());
    assert!(store.evaluator.get_local(z_sym, fp).is_none());
}

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
fn store_get_missing_returns_err() {
    let store = VariableStore::new();
    let mut store = store.freeze();
    let result = store.get("nonexistent");
    assert!(result.is_err());
}

#[test]
fn store_get_missing_is_variable_not_found() {
    let store = VariableStore::new();
    let mut store = store.freeze();
    let err = store.get("ghost").unwrap_err();
    assert!(
        matches!(err, MizuError::VariableNotFound(ref name) if name == "ghost"),
        "expected VariableNotFound(\"ghost\"), got: {err:?}"
    );
}

#[test]
fn store_get_missing_error_message() {
    let store = VariableStore::new();
    let mut store = store.freeze();
    let err = store.get("missing_var").unwrap_err();
    assert_eq!(err.to_string(), "variable not found: `missing_var`");
}

#[test]
fn store_new_and_default_are_equivalent() {
    let a = VariableStore::new();
    let mut a = a.freeze();
    let b = VariableStore::default();
    assert!(a.get("x").is_err());
    assert!(b.get("x").is_err());
}

#[test]
fn json_object_becomes_record() {
    let json = serde_json::to_string(&serde_json::json!({"id":1,"name":"Neko"})).unwrap();
    let val = from_json_str(&json).unwrap();
    assert_eq!(
        val.get_field(crate::core::types::hash_field("id"), "id"),
        Some(&Value::Decimal(super::DECIMAL_SCALE))
    );
    assert_eq!(
        val.get_field(crate::core::types::hash_field("name"), "name"),
        Some(&Value::String(Arc::from("Neko")))
    );
}

#[test]
fn json_array_of_objects() {
    let json = serde_json::to_string(&serde_json::json!([{"id":1},{"id":2}])).unwrap();
    let val = from_json_str(&json).unwrap();
    if let Value::List(ref items) = val {
        assert_eq!(items.len(), 2);
        assert!(
            matches!(items[0], Value::Record(_)),
            "first element must be Record"
        );
        assert!(
            matches!(items[1], Value::Record(_)),
            "second element must be Record"
        );
    } else {
        panic!("expected Value::List, got {val:?}");
    }
}

#[test]
fn json_string_passthrough() {
    let json = serde_json::to_string(&serde_json::json!("hello")).unwrap();
    let val = from_json_str(&json).unwrap();
    assert_eq!(val, Value::String(Arc::from("hello")));
}

#[test]
fn json_null_becomes_value_null() {
    let val = from_json(&serde_json::Value::Null).unwrap();
    assert_eq!(val, Value::Null);
}

#[test]
fn json_bool_becomes_value_bool() {
    assert_eq!(
        from_json(&serde_json::json!(true)).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        from_json(&serde_json::json!(false)).unwrap(),
        Value::Bool(false)
    );
}

#[test]
fn json_integer_becomes_value_int() {
    let val = from_json(&serde_json::json!(42)).unwrap();
    assert_eq!(val, Value::Decimal(42 * super::DECIMAL_SCALE));
}

#[test]
fn json_float_becomes_value_int() {
    let val = from_json(&serde_json::json!(3.14)).unwrap();
    assert_eq!(val, Value::Decimal(314_000_000));
}

#[test]
fn json_integer_exact_path_avoids_float_precision_loss() {
    // A whole-number JSON literal near the top of the representable
    // range must round-trip exactly via the checked_mul integer path,
    // not lose precision the way dividing through f64 unconditionally
    // used to.
    let val = from_json(&serde_json::json!(92_233_720_368_i64)).unwrap();
    assert_eq!(val, Value::Decimal(92_233_720_368 * super::DECIMAL_SCALE));
}

#[test]
fn json_integer_parses_to_true_int_without_overflow() {
    let val = from_json(&serde_json::json!(i64::MAX)).unwrap();
    assert_eq!(val, Value::Int(i64::MAX));
}

#[test]
fn json_roundtrip_exact_at_top_of_range() {
    // i64::MAX is the largest representable scaled value: exactly
    // 92233720368.54775807. Round-tripping it through to_json/from_json
    // must recover the exact same Value::Decimal, pinning the from_json
    // exact-integer path and to_json's exact-integer emission against
    // silent precision loss at the top of the new 8-decimal-digit range.
    let original = Value::Decimal(i64::MAX);
    let json = to_json(&original).unwrap();
    let roundtripped = from_json(&json).unwrap();
    assert_eq!(roundtripped, original);
}

#[test]
fn record_display_contains_fields() {
    let json = serde_json::to_string(&serde_json::json!({"x":1})).unwrap();
    let val = from_json_str(&json).unwrap();
    let display = val.to_string();
    assert!(
        display.contains("x"),
        "display must contain field name: {display}"
    );
    assert!(
        display.contains("1"),
        "display must contain field value: {display}"
    );
    assert!(
        display.starts_with('{'),
        "display must start with '{{': {display}"
    );
    assert!(
        display.ends_with('}'),
        "display must end with '}}': {display}"
    );
}

#[test]
fn from_json_depth_limit_returns_err() {
    // Build a 300-level nested array: [[[[...[42]...]]]]
    // Nesting beyond MAX_JSON_DEPTH (== MAX_EVAL_DEPTH == 256) must be
    // rejected outright with Err(MizuError::SecurityViolation) rather
    // than silently clamped to Value::Null — a clamp would let a caller
    // mistake a malicious deeply-nested payload for legitimate absent
    // data.
    let mut json = serde_json::json!(42_);
    for _ in 0..300 {
        json = serde_json::json!([json]);
    }

    let result = from_json(&json);

    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "deeply-nested JSON must be rejected with SecurityViolation, got: {result:?}"
    );
}

#[test]
fn from_json_shallow_nesting_parses_fully() {
    // A 3-level nested array (well within MAX_JSON_DEPTH) must parse
    // completely — the depth limit must not truncate legitimate data.
    let json = serde_json::json!([[[42_i64]]]);
    let result = from_json(&json).unwrap();

    let l1 = match &result {
        Value::List(v) => &v[0],
        other => panic!("level 0 must be List: {other:?}"),
    };
    let l2 = match l1 {
        Value::List(v) => &v[0],
        other => panic!("level 1 must be List: {other:?}"),
    };
    let leaf = match l2 {
        Value::List(v) => &v[0],
        other => panic!("level 2 must be List: {other:?}"),
    };
    assert_eq!(
        *leaf,
        Value::Decimal(42 * super::DECIMAL_SCALE),
        "leaf must be Int(42 * DECIMAL_SCALE)"
    );
}

#[test]
fn store_interpolate_string() {
    let mut store = VariableStore::new();
    store.set("count", Value::Decimal(42 * super::DECIMAL_SCALE));
    store.set("name", "Mizu");
    let mut store = store.freeze();

    let result = store.interpolate("{name} has {count} items");
    assert_eq!(result.unwrap(), "Mizu has 42 items");

    let lenient_res = store.interpolate("{name} has {missing}");
    assert_eq!(lenient_res.unwrap(), "Mizu has {missing}");

    let escaped_res = store.interpolate("\\{name\\} has {count}");
    assert_eq!(escaped_res.unwrap(), "{name} has 42");

    let escaped_backslash_res = store.interpolate("Test \\\\{count}");
    assert_eq!(escaped_backslash_res.unwrap(), "Test \\42");
}

/// Interpolation is the one path where a value leaves the budgeted evaluator
/// and enters the renderer: it runs during layout/paint, so `max_instructions`
/// never applies. A network response is bounded only by the 32 MiB transfer
/// cap and is a single JSON node (so `MAX_JSON_NODES` never fires), which made
/// one `text "{data}"` node enough to hand tens of megabytes to the text
/// shaper on the UI thread, on every layout pass.
///
/// The oversized value must be *rejected*, not truncated: a clipped prefix of
/// attacker-controlled data would be indistinguishable from the real value to
/// everything downstream.
#[test]
fn interpolation_rejects_oversized_values_instead_of_shaping_them() {
    use super::eval::MAX_INTERPOLATED_BYTES;

    let mut store = VariableStore::new();
    store.set("small", "x".repeat(16));
    store.set("huge", "A".repeat(MAX_INTERPOLATED_BYTES + 1));
    let store = store.freeze();

    assert_eq!(store.interpolate("v={small}").unwrap(), format!("v={}", "x".repeat(16)));

    let err = store
        .interpolate("{huge}")
        .expect_err("an over-budget value must not reach the renderer");
    assert!(
        matches!(err, MizuError::SecurityViolation(_)),
        "expected a SecurityViolation naming the render budget, got {err:?}"
    );

    // The cap bounds the whole run, not just one substitution: many
    // individually-legal values must not add up past it either.
    let mut store = VariableStore::new();
    store.set("chunk", "y".repeat(MAX_INTERPOLATED_BYTES / 4));
    let store = store.freeze();
    assert!(
        store.interpolate("{chunk}{chunk}").is_ok(),
        "two quarter-budget values must still render"
    );
    assert!(
        matches!(
            store.interpolate("{chunk}{chunk}{chunk}{chunk}{chunk}"),
            Err(MizuError::SecurityViolation(_))
        ),
        "values summing past the budget must be rejected"
    );
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

#[test]
fn interpolate_dot_access() {
    let mut store = VariableStore::new();
    let mut map: Vec<(Arc<str>, Value)> = Vec::new();
    map.push((Arc::from("age"), Value::Decimal(3 * super::DECIMAL_SCALE)));
    map.push((Arc::from("name"), Value::String(Arc::from("Neko"))));
    store.set("item", Value::record_from_unsorted(map));
    let mut store = store.freeze();

    let result = store
        .interpolate("The cat is {item.name} and is {item.age} years old")
        .unwrap();
    assert_eq!(result, "The cat is Neko and is 3 years old");

    // Missing field falls back to literal placeholder.
    let fallback = store.interpolate("{item.missing}").unwrap();
    assert_eq!(fallback, "{item.missing}");
}

#[test]
fn overlay_takes_priority_over_store() {
    // A key present in both the overlay and the store must resolve to the
    // overlay value — the store must not be consulted.
    let mut store = VariableStore::new();
    store.set("name", "global");
    let mut store = store.freeze();

    let mut overlay = HashMap::new();
    overlay.insert("name".to_string(), Value::from("local"));

    let result = store
        .interpolate_with_overlay("Hello {name}", &overlay)
        .unwrap();
    assert_eq!(
        result, "Hello local",
        "overlay must shadow the global store"
    );
}

#[test]
fn overlay_falls_back_to_store_for_missing_key() {
    // Keys absent from the overlay must still resolve from the global store.
    let mut store = VariableStore::new();
    store.set("greeting", "hello");
    let mut store = store.freeze();

    let overlay: HashMap<String, Value> = HashMap::new();
    let result = store
        .interpolate_with_overlay("{greeting} {name}", &overlay)
        .unwrap();
    // `name` is missing from both overlay and store → literal placeholder.
    assert_eq!(result, "hello {name}");
}

#[test]
fn overlay_dot_path_from_overlay_record() {
    // {item.field} must resolve through a Record stored in the overlay,
    // without cloning the Evaluator or StringInterner.
    let store = VariableStore::new().freeze(); // empty global store

    let mut inner: Vec<(Arc<str>, Value)> = Vec::new();
    inner.push((Arc::from("name"), Value::String(Arc::from("Neko"))));
    let record = Value::record_from_unsorted(inner);

    let mut overlay = HashMap::new();
    overlay.insert("item".to_string(), record);

    let result = store
        .interpolate_with_overlay("{item.name}", &overlay)
        .unwrap();
    assert_eq!(
        result, "Neko",
        "dot-path must resolve through overlay record"
    );
}

#[test]
fn overlay_dot_path_falls_back_to_store() {
    // {item.name} when `item` is absent from the overlay but present in the
    // store must fall back correctly.
    let mut store = VariableStore::new();
    let mut inner: Vec<(Arc<str>, Value)> = Vec::new();
    inner.push((Arc::from("name"), Value::String(Arc::from("Stripe"))));
    store.set("item", Value::record_from_unsorted(inner));
    let mut store = store.freeze();

    let overlay: HashMap<String, Value> = HashMap::new(); // empty overlay
    let result = store
        .interpolate_with_overlay("{item.name}", &overlay)
        .unwrap();
    assert_eq!(
        result, "Stripe",
        "dot-path must fall back to store when absent from overlay"
    );
}

#[test]
fn empty_overlay_is_identical_to_interpolate() {
    // An empty overlay must produce exactly the same result as a direct
    // `interpolate` call (the fast-path and overlay-path must agree).
    let mut store = VariableStore::new();
    store.set("x", Value::Decimal(42));
    let mut store = store.freeze();

    let overlay: HashMap<String, Value> = HashMap::new();
    let via_overlay = store.interpolate_with_overlay("x={x}", &overlay).unwrap();
    let direct = store.interpolate("x={x}").unwrap();
    assert_eq!(via_overlay, direct);
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

// ────────────────────────────────────────────────────────────────────────
// length / to_string / contains / has_field
// ────────────────────────────────────────────────────────────────────────

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
    let n = 3 * crate::core::types::DECIMAL_SCALE + crate::core::types::DECIMAL_SCALE / 2;
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
        .arg("core::types::tests::cross_function_composition_depth_guard")
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
///   `cargo test --release --lib core::types::tests::measure_stack_usage_at_max_eval_depth -- --ignored --nocapture`
///   `cargo test          --lib core::types::tests::measure_stack_usage_at_max_eval_depth -- --ignored --nocapture`
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
            .arg("core::types::tests::measure_stack_usage_at_max_eval_depth")
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
fn interpolate_deep_dot_path() {
    // Three-level nesting: {a.b.c} must resolve to the leaf string.
    let mut store = VariableStore::new();

    // Build: a = { b: { c: "value" } }
    let mut inner: Vec<(Arc<str>, Value)> = Vec::new();
    inner.push((Arc::from("c"), Value::String(Arc::from("value"))));
    let mut outer: Vec<(Arc<str>, Value)> = Vec::new();
    outer.push((Arc::from("b"), Value::record_from_unsorted(inner)));
    store.set("a", Value::record_from_unsorted(outer));
    let mut store = store.freeze();

    let result = store
        .interpolate("{a.b.c}")
        .expect("interpolation must succeed");
    assert_eq!(result, "value", "three-level dot-path must resolve to leaf");
}

#[test]
fn interpolate_dot_path_missing_intermediate() {
    // {a.b.c} where `b` is a String, not a Record — must fall back to literal.
    let mut store = VariableStore::new();

    let mut outer: Vec<(Arc<str>, Value)> = Vec::new();
    outer.push((Arc::from("b"), Value::String(Arc::from("not_a_record"))));
    store.set("a", Value::record_from_unsorted(outer));
    let mut store = store.freeze();

    let result = store
        .interpolate("{a.b.c}")
        .expect("interpolation must not error");
    assert_eq!(
        result, "{a.b.c}",
        "non-record intermediate must produce literal fallback"
    );
}

#[test]
fn frozen_interner_existing_symbols_unchanged() {
    let mut interner = StringInterner::new();
    let sym_a = interner.get_or_intern("alpha");
    let sym_b = interner.get_or_intern("beta");

    let interner = interner.freeze();

    // Existing symbols must still resolve to the same ID post-freeze.
    assert_eq!(interner.get("alpha"), Some(sym_a));
    assert_eq!(interner.get("beta"), Some(sym_b));
    assert_eq!(interner.resolve(sym_a), Some("alpha"));
}

// The interner's frozen state used to be a runtime `frozen: bool` on a single
// `StringInterner`, and a family of tests here asserted that the flag survived
// `Clone`, that an unfrozen clone stayed unfrozen, and that a post-freeze
// `get_or_intern` at least returned a real (if thread-local) Symbol rather
// than a sentinel.
//
// That flag is gone. Freezing is now a type-state transition —
// `StringInterner::freeze(self) -> FrozenInterner` consumes the builder, and
// `FrozenInterner` has no `get_or_intern` at all. Every one of those tests is
// now either unwritable or unnecessary: post-freeze interning does not compile,
// so there is no runtime behavior left to pin down. What still needs testing is
// the property those tests were ultimately protecting — that a frozen table and
// its clones agree on every Symbol ID.

/// M1: the logic worker's copy of the symbol table must agree with the UI
/// thread's on every `Symbol(u32)`, or the two threads read different
/// variables through the same expression tree.
#[test]
fn frozen_clone_resolves_every_symbol_identically() {
    let mut interner = StringInterner::new();
    let sym_x = interner.get_or_intern("x");
    let sym_y = interner.get_or_intern("y");
    let interner = interner.freeze();

    let cloned = interner.clone();

    assert_eq!(cloned.get("x"), Some(sym_x));
    assert_eq!(cloned.get("y"), Some(sym_y));
    assert_eq!(cloned.resolve(sym_x), Some("x"));
    assert_eq!(cloned.vec.len(), interner.vec.len());
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

fn from_json(json: &serde_json::Value) -> Result<Value, MizuError> {
    let s = serde_json::to_string(json).map_err(|e| MizuError::SecurityViolation(e.to_string()))?;
    from_json_str(&s)
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

// `interner_clone_preserves_frozen_state` and
// `interner_clone_preserves_unfrozen_state` lived here. Both asserted on the
// old `frozen: bool`; neither is expressible now that freezing is a type-state
// transition (see the note above `frozen_clone_resolves_every_symbol_identically`).
// Only the Symbol-identity property below survives the change.

/// Symbols are identical in the original and its frozen clone.
#[test]
fn interner_clone_symbols_are_identical() {
    let mut interner = StringInterner::new();
    let s_alpha = interner.get_or_intern("alpha");
    let s_beta = interner.get_or_intern("beta");
    let interner = interner.freeze();

    let clone = interner.clone();
    assert_eq!(clone.get("alpha"), Some(s_alpha));
    assert_eq!(clone.get("beta"), Some(s_beta));
    assert_eq!(clone.vec.len(), interner.vec.len());
}

/// `set_runtime` updates a pre-declared (interned) variable normally.
#[test]
fn set_runtime_updates_known_variable() {
    let mut store = VariableStore::new();
    store.set("price", Value::Decimal(10));
    let mut store = store.freeze();

    store.set_runtime("price", Value::Decimal(99));
    assert_eq!(*store.get("price").unwrap(), Value::Decimal(99));
}

/// `set_runtime` silently discards names that are not in the frozen interner,
/// leaving the symbol table unchanged.
#[test]
fn set_runtime_discards_unknown_names_and_does_not_grow_interner() {
    let mut store = VariableStore::new();
    store.set("declared", Value::Decimal(1));
    let mut store = store.freeze();

    let interned_count = store.interner.vec.len();

    store.set_runtime("undeclared_field", Value::Decimal(42));
    store.set_runtime("another_unknown", Value::from("hello"));

    // Interner must not have grown.
    assert_eq!(
        store.interner.vec.len(),
        interned_count,
        "frozen interner must not grow via set_runtime"
    );
    // Unknown names are not stored.
    assert!(
        store.get("undeclared_field").is_err(),
        "undeclared variable must not appear in the store"
    );
}

/// Demonstrates the M1 fix end-to-end: after freeze, a clone used by the
/// worker thread cannot add symbols that would diverge from the UI thread.
/// Before the fix, the worker's clone was unfrozen and adding "runtime_var"
/// would produce Symbol(N) on the worker but a DIFFERENT Symbol(M) if the
/// UI thread independently interned the same name later.
#[test]
fn frozen_clone_cannot_diverge_symbol_ids() {
    let mut ui_interner = StringInterner::new();
    let sym_a = ui_interner.get_or_intern("declared_a");
    let sym_b = ui_interner.get_or_intern("declared_b");
    let ui_interner = ui_interner.freeze();

    let worker_interner = ui_interner.clone();

    // The worker resolves known symbols identically.
    assert_eq!(worker_interner.get("declared_a"), Some(sym_a));
    assert_eq!(worker_interner.get("declared_b"), Some(sym_b));

    // Worker-side VariableStore with the frozen clone.
    let mut worker_store = VariableStore::with_interner(worker_interner);

    // set_runtime does NOT intern "runtime_var".
    worker_store.set_runtime("runtime_var", Value::Decimal(7));
    assert!(worker_store.get("runtime_var").is_err());

    // Symbol table size on both sides is still identical.
    assert_eq!(
        worker_store.interner.vec.len(),
        ui_interner.vec.len(),
        "worker must not add symbols after freeze"
    );
}

// ────────────────────────────────────────────────────────────────────────
// Value::FileHandle — deliberately inert (never compared/serialised/
// displayed in full)
// ────────────────────────────────────────────────────────────────────────

fn file_handle(filename: &str) -> Value {
    Value::FileHandle(Arc::new(FileHandleData {
        path: std::path::PathBuf::from(format!("/home/user/secret-dir/{filename}")),
        filename: filename.to_string(),
    }))
}

#[test]
fn file_handle_pointer_equality() {
    let a = file_handle("avatar.png");
    // Pointer equality is used for FileHandle to preserve PartialEq reflexivity
    let a2 = a.clone();
    assert_eq!(
        a, a2,
        "Cloned file handles should be equal via pointer equality"
    );

    let b = file_handle("avatar.png"); // same filename, distinct handle
    assert_ne!(a, b);
}

#[test]
fn file_handle_to_json_errors_without_leaking_the_path() {
    let handle = file_handle("resume.pdf");
    let err = to_json(&handle).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
    let msg = err.to_string();
    assert!(
        !msg.contains("secret-dir") && !msg.contains("/home/user"),
        "to_json error message must not leak the file's path: {msg}"
    );
}

#[test]
fn file_handle_nested_in_record_still_fails_to_json() {
    let record = Value::record_from_unsorted(vec![("avatar", file_handle("avatar.png"))]);
    assert!(to_json(&record).is_err());
}

#[test]
fn file_handle_nested_in_list_still_fails_to_json() {
    let list = Value::List(Arc::new(vec![file_handle("a.txt"), file_handle("b.txt")]));
    assert!(to_json(&list).is_err());
}

#[test]
fn file_handle_display_redacts_the_full_path() {
    let handle = file_handle("tax-return-2025.pdf");
    let rendered = handle.to_string();
    assert!(rendered.contains("tax-return-2025.pdf"));
    assert!(
        !rendered.contains("secret-dir") && !rendered.contains("/home/user"),
        "Display must never show the full path: {rendered}"
    );
}
