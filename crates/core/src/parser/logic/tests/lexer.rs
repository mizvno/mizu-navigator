//! Tests for `lexer.rs`: tokenization edge cases not covered by exercising
//! the parser's happy paths directly.

use super::*;

// ────────────────────────────────────────────────────────────────────────
// $form magic variable — lexed as Ident("$form")
// ────────────────────────────────────────────────────────────────────────

#[test]
fn dollar_form_variable_is_valid_assign_target() {
    // `$form = 1` must parse as Assign with target "$form"
    let action = parse_action("$form = 1", &mut StringInterner::new()).unwrap();
    assert!(
        matches!(action, Action::Assign { ref target, .. } if target == "$form"),
        "expected Assign with target $form, got: {action:?}"
    );
}
