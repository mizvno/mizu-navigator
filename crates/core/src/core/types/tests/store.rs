//! Tests for `store.rs`: `VariableStore` set/get, string interpolation
//! (including its overlay variant), and `set_runtime`'s frozen-interner
//! discipline.

use super::*;

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
fn store_interpolate_string() {
    let mut store = VariableStore::new();
    store.set("count", Value::Decimal(42 * DECIMAL_SCALE));
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
    use crate::core::types::eval::MAX_INTERPOLATED_BYTES;

    let mut store = VariableStore::new();
    store.set("small", "x".repeat(16));
    store.set("huge", "A".repeat(MAX_INTERPOLATED_BYTES + 1));
    let store = store.freeze();

    assert_eq!(
        store.interpolate("v={small}").unwrap(),
        format!("v={}", "x".repeat(16))
    );

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
fn interpolate_dot_access() {
    let mut store = VariableStore::new();
    let mut map: Vec<(Arc<str>, Value)> = Vec::new();
    map.push((Arc::from("age"), Value::Decimal(3 * DECIMAL_SCALE)));
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

/// `set_runtime` updates a pre-declared (interned) variable normally.
#[test]
fn set_runtime_updates_known_variable() {
    let mut store = VariableStore::new();
    store.set("price", Value::Decimal(10));
    let mut store = store.freeze();

    store.set_runtime("price", Value::Decimal(99));
    assert_eq!(*store.get("price").unwrap(), Value::Decimal(99));
}

/// Dot-path interpolation must fall through to the global store when the
/// overlay contains the root key but the full nested path is absent.
///
/// Pre-fix behaviour: `{user.email}` resolved to the literal `{user.email}`
/// because `handled` was set to `true` as soon as the overlay contained any
/// `user` key, even though `resolve_dot_path` returned `None`.
///
/// Post-fix behaviour: `handled` is `false` when `resolve_dot_path` returns
/// `None`, so the global store is consulted and the correct email is
/// returned. (Originally lived in `network::worker::tests` — moved here
/// because it exercises `interpolate_with_overlay` directly and touches no
/// networking code at all.)
#[test]
fn dot_path_cascades_to_global_store_when_overlay_lacks_leaf() {
    // Global store: user record that has both `name` and `email`.
    let mut store = VariableStore::new();
    let mut global_user = Vec::<(Arc<str>, Value)>::new();
    global_user.push((Arc::from("name"), Value::from("Alice")));
    global_user.push((Arc::from("email"), Value::from("alice@example.com")));
    store.set("user", Value::record_from_unsorted(global_user));

    // Overlay: user record that only has `name` — no `email` field.
    let mut overlay_user = Vec::<(Arc<str>, Value)>::new();
    overlay_user.push((Arc::from("name"), Value::from("Bob")));
    let mut overlay: HashMap<String, Value> = HashMap::new();
    overlay.insert(
        "user".to_string(),
        Value::record_from_unsorted(overlay_user),
    );

    // Interpolating `{user.name}` should resolve from the overlay (Bob).
    let store = store.freeze();
    let name_result = store
        .interpolate_with_overlay("{user.name}", &overlay)
        .expect("interpolation must not error");
    assert_eq!(
        name_result, "Bob",
        "overlay must win for a path it fully resolves ({{user.name}})"
    );

    // Interpolating `{user.email}` must cascade to the global store because
    // the overlay's user record lacks the `email` field.
    let email_result = store
        .interpolate_with_overlay("{user.email}", &overlay)
        .expect("interpolation must not error");
    assert_eq!(
        email_result, "alice@example.com",
        "global store must be consulted when overlay root exists but path is incomplete"
    );

    // A path absent from both overlay AND global store still renders the
    // raw placeholder.
    let missing_result = store
        .interpolate_with_overlay("{user.phone}", &overlay)
        .expect("interpolation must not error");
    assert_eq!(
        missing_result, "{user.phone}",
        "path absent from both overlay and global store must render as raw placeholder"
    );
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
