//! The `Value::Int` / `Value::Decimal` split, and what has to stay true
//! across it.
//!
//! Two numeric variants behind one surface type (`num`) is a design that only
//! works if the split is invisible where it should be and exact where it
//! matters. These tests pin both halves of that:
//!
//! * arithmetic and comparison agree across the two variants, so a program
//!   cannot observe which one it happens to hold;
//! * serialization preserves the variant, so a value does not change
//!   behaviour by being written to disk and read back.
//!
//! They live in an integration test so they exercise only the public surface,
//! which is the surface a document's behaviour is actually a function of.

use mizu_core::core::types::{DECIMAL_SCALE, Value, from_json_slice, from_json_str, to_json};

fn eq(a: &Value, b: &Value) -> bool {
    a.budget_eq(b, &mut 0, u64::MAX)
        .expect("a test-sized value cannot exhaust an unbounded budget")
}

fn json(v: &Value) -> String {
    serde_json::to_string(&to_json(v).expect("value must be JSON-encodable")).unwrap()
}

/// The property the whole split rests on: a value survives a save/load cycle
/// as the same *variant*, not merely the same magnitude.
///
/// Without it, `Decimal(5.0)` is written as `5`, read back as `Int(5)`, and
/// starts behaving like an integer — a value silently changing type because
/// the browser was restarted.
#[test]
fn json_round_trip_preserves_the_numeric_variant() {
    let cases = [
        Value::Int(0),
        Value::Int(5),
        Value::Int(-5),
        Value::Int(i64::MAX),
        Value::Int(i64::MIN),
        Value::Decimal(0),
        Value::Decimal(5 * DECIMAL_SCALE),
        Value::Decimal(-5 * DECIMAL_SCALE),
        Value::Decimal(150_000_000), // 1.5
        Value::Decimal(-50_000_000), // -0.5
        Value::Decimal(1),           // 0.00000001
        Value::Decimal(-1),          // -0.00000001
        Value::Decimal(i64::MAX),
        Value::Decimal(i64::MIN + 1),
    ];

    for original in cases {
        let encoded = json(&original);
        let decoded = from_json_str(&encoded).expect("re-parse must succeed");
        assert_eq!(
            std::mem::discriminant(&original),
            std::mem::discriminant(&decoded),
            "{original:?} serialized to `{encoded}` and came back as {decoded:?}"
        );
        assert!(
            eq(&original, &decoded),
            "{original:?} serialized to `{encoded}` and came back as {decoded:?}"
        );
    }
}

/// `Int` keeps the whole 64-bit range. Scaling it into fixed point would cap
/// integers at roughly ±9.2e10 for no reason a document could understand.
#[test]
fn integers_keep_their_full_range() {
    for text in ["9223372036854775807", "-9223372036854775808"] {
        let parsed = from_json_str(text).expect("64-bit integers must parse");
        assert!(matches!(parsed, Value::Int(_)), "{text} became {parsed:?}");
        assert_eq!(parsed.to_string(), text);
    }
}

/// An integer too large for `i64` is rejected, not wrapped or truncated to a
/// float. A silently altered number is worse than a refused one.
#[test]
fn integers_beyond_the_range_are_rejected() {
    assert!(from_json_str("9223372036854775808").is_err());
    assert!(from_json_str("-9223372036854775809").is_err());
}

/// A fractional literal beyond the representable range is rejected rather
/// than wrapped through the `i64` scaling.
#[test]
fn decimals_beyond_the_fixed_point_range_are_rejected() {
    assert!(from_json_str("92233720369.0").is_err());
    assert!(from_json_str("-92233720369.0").is_err());
}

/// Excess fractional digits round rather than truncate — truncation biases
/// every value toward zero — and rounding never turns a value into an error.
#[test]
fn excess_fractional_digits_round_to_the_representable_scale() {
    for (text, expected) in [
        ("0.123456789", 12_345_679),   // 9th digit is 9, rounds up
        ("0.123456781", 12_345_678),   // 9th digit is 1, rounds down
        ("-0.123456789", -12_345_679), // away from zero, symmetrically
        ("1.000000004", 100_000_000),
        ("1.000000005", 100_000_001),
    ] {
        let parsed = from_json_str(text).expect("ordinary API numbers must parse");
        assert!(
            eq(&parsed, &Value::Decimal(expected)),
            "{text} parsed as {parsed:?}, expected Decimal({expected})"
        );
    }
}

/// Display is the user-facing rendering, where the split must not show.
#[test]
fn display_hides_the_split() {
    assert_eq!(Value::Int(5).to_string(), "5");
    assert_eq!(Value::Decimal(5 * DECIMAL_SCALE).to_string(), "5");
    assert_eq!(Value::Decimal(150_000_000).to_string(), "1.5");
    assert_eq!(Value::Decimal(-50_000_000).to_string(), "-0.5");
    assert_eq!(Value::Decimal(1).to_string(), "0.00000001");
    assert_eq!(Value::Decimal(-1).to_string(), "-0.00000001");
}

/// Equality reaches across the split, so `5` and `5.0` are the same number
/// however each side was produced.
#[test]
fn equality_crosses_the_split() {
    assert!(eq(&Value::Int(5), &Value::Decimal(5 * DECIMAL_SCALE)));
    assert!(eq(&Value::Decimal(5 * DECIMAL_SCALE), &Value::Int(5)));
    assert!(!eq(&Value::Int(5), &Value::Decimal(5 * DECIMAL_SCALE + 1)));
}

/// An integer with no fixed-point representation is simply not equal to any
/// decimal — it must not overflow the comparison.
#[test]
fn out_of_scale_integers_compare_without_overflowing() {
    assert!(!eq(&Value::Int(i64::MAX), &Value::Decimal(0)));
    assert!(!eq(&Value::Int(i64::MIN), &Value::Decimal(0)));
}

/// A shared subtree makes a value's node count exponential in its
/// construction cost — the "billion laughs" shape. `budget_eq` must answer
/// within its budget rather than running for the age of the universe, and
/// `Arc::ptr_eq` must make the identical case cheap.
#[test]
fn shared_subtrees_do_not_explode_the_comparison() {
    let mut a = Value::List(std::sync::Arc::new(vec![Value::Int(1)]));
    for _ in 0..40 {
        a = Value::List(std::sync::Arc::new(vec![a.clone(), a]));
    }
    let b = a.clone();

    // Identical `Arc`s: one node, whatever the notional size of the tree.
    let mut budget = 0;
    assert!(a.budget_eq(&b, &mut budget, 16).unwrap());
    assert!(
        budget <= 16,
        "shared roots must short-circuit, spent {budget}"
    );

    // Distinct roots over the same shared children: bounded by the budget,
    // never unbounded work.
    let distinct = Value::List(std::sync::Arc::new(vec![a.clone()]));
    let other = Value::List(std::sync::Arc::new(vec![a]));
    let mut budget = 0;
    assert!(distinct.budget_eq(&other, &mut budget, 10_000).unwrap());
}

/// Comparison walks an explicit work stack, so nesting depth costs heap rather
/// than native frames.
///
/// The depth here is four times `MAX_EVAL_DEPTH` (256), which is the deepest
/// value any document or network payload can actually produce. It is
/// deliberately not larger than that: `Value`'s own `Drop` is recursive
/// — `Arc<Vec<Value>>` frees its children on the native stack — so a value
/// deep enough to overflow a recursive comparison would overflow on release
/// too, whatever this function does. Depth is bounded at construction, and
/// that bound is load-bearing for the whole type, not just for comparison.
/// What this test pins is that comparison adds no *further* stack requirement
/// on top of it.
#[test]
fn comparison_depth_costs_heap_not_stack() {
    // Four times `MAX_EVAL_DEPTH`, the deepest a document or payload can
    // reach. Not more: see the doc comment above.
    const DEEP: usize = 1_024;

    let build = || {
        let mut v = Value::Int(1);
        for _ in 0..DEEP {
            v = Value::List(std::sync::Arc::new(vec![v]));
        }
        v
    };
    let a = build();
    let b = build();

    let mut budget = 0;
    assert!(a.budget_eq(&b, &mut budget, u64::MAX).unwrap());
    assert!(
        budget as usize >= DEEP,
        "the walk must actually have descended, spent {budget}"
    );

    // And the budget is what stops it when one is imposed.
    let mut budget = 0;
    assert!(
        a.budget_eq(&b, &mut budget, 64).is_err(),
        "an exhausted budget must report Timeout, not a verdict"
    );
}

/// Storage rehydration is trusted for the node cap and untrusted for depth.
/// Both halves matter: the first keeps a legitimately-large persisted record
/// loadable, the second keeps the parse from running off the stack.
#[test]
fn trusted_hydration_lifts_the_node_cap_but_not_the_depth_cap() {
    let wide = format!("[{}]", vec!["0"; 100_000].join(","));
    assert!(
        from_json_slice(wide.as_bytes(), false).is_err(),
        "an untrusted payload must hit the node cap"
    );
    assert!(
        from_json_slice(wide.as_bytes(), true).is_ok(),
        "a persisted record must remain loadable"
    );

    let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
    for trusted in [false, true] {
        assert!(
            from_json_slice(deep.as_bytes(), trusted).is_err(),
            "depth must be enforced regardless of trust (trusted={trusted})"
        );
    }
}

/// `MAX_JSON_DEPTH` is tied to `MAX_EVAL_DEPTH` so that anything the evaluator
/// can build stays re-readable. That guarantee is only real if no *lower*
/// limit fires first — `serde_json`'s own 128-level recursion cap used to.
#[test]
fn nesting_up_to_the_evaluator_depth_still_parses() {
    let depth = 200;
    let deep = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
    assert!(
        from_json_slice(deep.as_bytes(), true).is_ok(),
        "nesting below MAX_EVAL_DEPTH must survive a storage round trip"
    );
}
