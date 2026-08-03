//! Tests for the flow module.

use super::*;
use crate::core::errors::MizuError;
use crate::core::types::StringInterner;
use crate::parser::layout::parse_layout_with_urls;
use crate::parser::logic::{parse_computed_with_functions, parse_logic, parse_root_timers};
use crate::parser::splitter::split_source_with_origin;
use crate::parser::urls::parse_urls;

fn check_flow_doc(src: &str) -> Result<(usize, usize, usize), MizuError> {
    let current_dir = std::env::current_dir().unwrap_or_default();
    let blocks =
        split_source_with_origin(src, &current_dir, crate::parser::Origin::Network).unwrap();
    let mut interner = StringInterner::new();
    let urls = parse_urls(&blocks.urls_block, &mut interner).unwrap_or_default();
    let functions = parse_logic(&blocks.logic_block, &mut interner).unwrap_or_default();
    let mut comps = parse_computed_with_functions(
        &blocks.logic_block,
        &mut interner,
        &functions,
        crate::core::config::CONFIG.max_comp_bindings,
    )
    .unwrap_or_default();
    let timers = parse_root_timers(&blocks.logic_block, &mut interner).unwrap_or_default();
    let dom = parse_layout_with_urls(
        &blocks.layout_block,
        &mut interner,
        Some(&urls),
        true,
        &functions,
    )
    .unwrap();

    check_information_flow(&dom, &timers, &functions, &mut comps, &urls, &interner)
}

// ── Core flow violation tests ───────────────────────────────────────────

#[test]
fn network_var_into_navigate_rejected() {
    let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    timer 2s -> navigate data
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(res.is_err(), "Expected flow violation");
    let msg = res.unwrap_err().to_string();
    assert!(
        msg.contains("navigate"),
        "error should mention navigate: {msg}"
    );
    assert!(
        msg.contains("data"),
        "error should mention the tainted var: {msg}"
    );
}

#[test]
fn clean_constant_into_navigate_allowed() {
    let doc = r#"
logic
    timer 1s -> navigate "mizu://safe.com/"
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(res.is_ok(), "Expected flow allowed");
}

#[test]
fn form_field_into_navigate_rejected() {
    let doc = r#"
logic
    timer 1s -> navigate $form.dest
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(res.is_err(), "Expected flow violation from form field");
    let msg = res.unwrap_err().to_string();
    assert!(msg.contains("$form"), "error should mention $form: {msg}");
}

#[test]
fn gated_gesture_navigation_allowed() {
    let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
layout
    doc
        button
            click -> navigate data
        "#;
    let res = check_flow_doc(doc);
    assert!(res.is_ok(), "Expected flow allowed for gesture");
}

// ── path_param is gated by construction ─────────────────────────────────

#[test]
fn network_var_into_path_param_allowed_by_construction() {
    // path_param is NOT a taint sink — it is gated by runtime A1+A2
    // validation.  This test verifies the design change from the previous
    // validate_path-based gate to the by-construction gate.
    let doc = r#"
urls
    api: mizu://api.example.com/user/{id}
logic
    timer 1s -> GET(api) -> data
    timer 2s -> GET(api, data) -> profile
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_ok(),
        "path_param should be allowed (gated by construction)"
    );
}

// ── Taint propagation tests ─────────────────────────────────────────────

#[test]
fn taint_propagates_through_binop_let_ifelse_fieldaccess() {
    // `data` is tainted (from GET), navigating `data.url` should be
    // rejected since FieldAccess propagates taint.
    let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    timer 2s -> navigate data.url
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_err(),
        "FieldAccess on tainted var should propagate taint"
    );
}

#[test]
fn pure_literal_flow_untainted() {
    // A constant string should never be tainted
    let doc = r#"
logic
    timer 1s -> navigate "mizu://pure.example.com/"
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(res.is_ok(), "Pure literal should not be tainted");
}

#[test]
fn taint_through_comp_chain_rejected() {
    // source → comp → sink: `data` (from GET) → `comp derived = data` →
    // navigate `derived` without gesture should be rejected.
    let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    comp derived = data
    timer 2s -> navigate derived
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(res.is_err(), "Taint through comp chain should be rejected");
}

#[test]
fn taint_propagates_through_function_return() {
    // A user function that returns a tainted global should taint the result
    let doc = r#"
urls
    api: mizu://api.example.com/
logic
    passthrough(x) : x
    timer 1s -> GET(api) -> data
    timer 2s -> navigate passthrough(data)
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_err(),
        "Function returning tainted arg should propagate taint"
    );
}

#[test]
fn taint_propagates_through_transitive_global() {
    // A function reads a tainted global transitively
    let doc = r#"
urls
    api: mizu://api.example.com/
logic
    read_data() : data
    timer 1s -> GET(api) -> data
    timer 2s -> navigate read_data()
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_err(),
        "Function reading tainted global should propagate taint"
    );
}

// ── Precision / over-approximation test ─────────────────────────────────

#[test]
fn over_approximation_may_reject_but_never_misses() {
    // Documented case: `if true then "safe" else data` — the checker
    // conservatively marks this as tainted because the else branch reads
    // `data`, even though at runtime the else is never taken.
    // This is acceptable: sound over complete.
    let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    timer 2s -> navigate if true then "mizu://safe.com/" else data
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    // This SHOULD be rejected by the conservative checker (over-approximation)
    assert!(
        res.is_err(),
        "Conservative checker should reject: dead branch still reads tainted var"
    );
}

// ── get_system_time: static write-target (RM-04) ────────────────────────

#[test]
fn get_system_time_targeting_comp_variable_rejected() {
    // Load-time equivalent of `execute_action`'s "cannot assign to
    // computed variable" guard, extended to get_system_time now that its
    // target is statically visible to the checker.
    let doc = r#"
logic
    comp derived = 1 + 1
    timer 1s -> get_system_time(derived)
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_err(),
        "Expected rejection: get_system_time cannot target a comp variable"
    );
    let msg = res.unwrap_err().to_string();
    assert!(
        msg.contains("derived"),
        "error should name the comp var: {msg}"
    );
    assert!(
        msg.contains("computed") || msg.contains("comp"),
        "error should explain why: {msg}"
    );
}

#[test]
fn get_system_time_targeting_plain_variable_allowed() {
    let doc = r#"
logic
    timer 1s -> get_system_time(elapsed)
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_ok(),
        "get_system_time targeting an ordinary variable should be allowed: {res:?}"
    );
}

#[test]
fn get_system_time_nested_in_assign_targeting_comp_rejected() {
    // The target-collecting walk must reach into every action's
    // expression, not just bare `Action::Eval` — here it's nested as
    // the RHS of an Assign.
    let doc = r#"
logic
    comp derived = 1 + 1
    timer 1s -> result = get_system_time(derived)
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_err(),
        "get_system_time nested in an Assign's RHS must still be caught"
    );
}

// ── Diagnostics (F3) ────────────────────────────────────────────────────

#[test]
fn diagnostic_includes_source_and_sink() {
    let doc = r#"
urls
    feed: mizu://api.example.com/feed
logic
    timer 1s -> GET(feed) -> next
    timer 2s -> navigate next
layout
    doc
        "#;
    let res = check_flow_doc(doc);
    assert!(res.is_err());
    let msg = res.unwrap_err().to_string();
    // F3: error message should mention the tainted variable and its source
    assert!(
        msg.contains("next"),
        "diagnostic should name the tainted var: {msg}"
    );
    assert!(
        msg.contains("GET") || msg.contains("feed") || msg.contains("navigate"),
        "diagnostic should mention the source or sink: {msg}"
    );
}
