//! Tests for `purity.rs`: `find_side_effect_call`, the allowlist that keeps
//! conditional-class conditions (re-evaluated every frame as pure
//! "observations") from invoking anything effectful.

use super::*;

#[test]
fn find_side_effect_call_detects_get_system_time() {
    // get_system_time was missing from SIDE_EFFECT_BUILTINS, meaning a
    // conditional-class condition (a pure "observation" context,
    // re-evaluated every frame) could invoke it undetected.
    let mut interner = StringInterner::new();
    let expr = super::super::parse_expr_standalone("get_system_time(x)", &mut interner).unwrap();
    assert_eq!(
        super::super::find_side_effect_call(
            expr.root(),
            &expr.arena,
            &interner,
            &Default::default()
        ),
        Some("get_system_time".to_string())
    );
}

// ── P1 allowlist direction (SECURITY-INVARIANTS.md) ──────────────────

#[test]
fn find_side_effect_call_rejects_unknown_name_by_default() {
    // The defining property of an allowlist: a name that is neither a
    // known-pure builtin nor a user-defined function is rejected, even
    // though it doesn't (yet) exist in the evaluator's dispatch at all.
    // A denylist would have let this through silently; this is the test
    // that would have caught that direction being wrong.
    let mut interner = StringInterner::new();
    let expr =
        super::super::parse_expr_standalone("some_future_effectful_builtin(x)", &mut interner)
            .unwrap();
    assert_eq!(
        super::super::find_side_effect_call(
            expr.root(),
            &expr.arena,
            &interner,
            &Default::default()
        ),
        Some("some_future_effectful_builtin".to_string()),
        "an unrecognised name must be rejected fail-secure by default, not passed through as pure"
    );
}

#[test]
fn find_side_effect_call_allows_known_pure_builtins() {
    let mut interner = StringInterner::new();
    for src in [
        "filter(list, x, x)",
        "count(list, x, x)",
        "sort(list, x, asc)",
    ] {
        let expr = super::super::parse_expr_standalone(src, &mut interner).unwrap();
        assert_eq!(
            super::super::find_side_effect_call(
                expr.root(),
                &expr.arena,
                &interner,
                &Default::default()
            ),
            None,
            "{src} should be treated as pure"
        );
    }
}

#[test]
fn find_side_effect_call_allows_a_call_to_a_user_defined_function() {
    // A call to a name the document itself declares as a `logic`
    // function must be treated as pure by construction (Mizu function
    // bodies are plain expressions — no side-effecting construct can be
    // the return value of a direct call), the same way it already was
    // under the old denylist (anything not explicitly effectful passed).
    let mut interner = StringInterner::new();
    let functions = parse_logic("    is_valid(x: num) : x\n", &mut interner).unwrap();
    let expr = super::super::parse_expr_standalone("is_valid(1)", &mut interner).unwrap();
    assert_eq!(
        super::super::find_side_effect_call(expr.root(), &expr.arena, &interner, &functions),
        None,
        "a call to a user-defined function must not be rejected as an unknown name"
    );
}
