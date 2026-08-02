//! Tests for `parse.rs`: `parse_logic`, `parse_root_timers`,
//! `parse_expr_standalone`, `parse_action`, `parse_action_with_urls`.

use super::*;

// ────────────────────────────────────────────────────────────────────────
// Lexer / parser — happy paths
// ────────────────────────────────────────────────────────────────────────

#[test]
fn parse_inline_function_no_args() {
    let (fns, interner) = single_fn("    pi() : 3.14159\n").unwrap();
    let pi_sym = interner.get("pi").unwrap();
    assert!(fns.contains_key(&pi_sym));
    let f = &fns[&pi_sym];
    assert!(f.params.is_empty());
    // 3.14159 * DECIMAL_SCALE, rounded.
    assert_eq!(*f.body.root(), Expr::Literal(Value::Decimal(314_159_000)));
}

#[test]
fn parse_inline_function_single_num_param() {
    let (fns, interner) = single_fn("    vat(p: num) : p * 1.22\n").unwrap();
    let vat_sym = interner.get("vat").unwrap();
    let f = &fns[&vat_sym];
    let p_sym = interner.get("p").unwrap();
    assert_eq!(f.params, vec![(p_sym, ValueType::Num)]);
    // Body should be BinaryOp(Variable(p_sym), Mul, Literal(1.22))
    assert!(matches!(
        f.body.root(),
        Expr::BinaryOp { op: BinOp::Mul, .. }
    ));
}

#[test]
fn parse_inline_function_two_params() {
    let (fns, interner) = single_fn("    add(a: num, b: num) : a + b\n").unwrap();
    let add_sym = interner.get("add").unwrap();
    let f = &fns[&add_sym];
    assert_eq!(f.params.len(), 2);
    let a_sym = interner.get("a").unwrap();
    let b_sym = interner.get("b").unwrap();
    assert_eq!(f.params[0], (a_sym, ValueType::Num));
    assert_eq!(f.params[1], (b_sym, ValueType::Num));
}

#[test]
fn parse_inline_string_param() {
    let (fns, interner) = single_fn("    greet(name: string) : name\n").unwrap();
    let greet_sym = interner.get("greet").unwrap();
    let f = &fns[&greet_sym];
    let name_sym = interner.get("name").unwrap();
    assert_eq!(f.params[0], (name_sym, ValueType::Str));
}

#[test]
fn parse_inline_bool_param() {
    let (fns, interner) = single_fn("    id_bool(b: bool) : b\n").unwrap();
    let sym = interner.get("id_bool").unwrap();
    let f = &fns[&sym];
    let b_sym = interner.get("b").unwrap();
    assert_eq!(f.params[0], (b_sym, ValueType::Bool));
}

#[test]
fn parse_inline_list_param() {
    let (fns, interner) = single_fn("    first(items: list<num>) : items\n").unwrap();
    let sym = interner.get("first").unwrap();
    let f = &fns[&sym];
    let items_sym = interner.get("items").unwrap();
    assert_eq!(
        f.params[0],
        (items_sym, ValueType::List(Box::new(ValueType::Num)))
    );
}

#[test]
fn parse_multiple_functions() {
    let src = r"
    double(x: num) : x * 2
    triple(x: num) : x * 3
";
    let (fns, interner) = single_fn(src).unwrap();
    assert_eq!(fns.len(), 2);
    assert!(
        interner
            .get("double")
            .map_or(false, |s| fns.contains_key(&s))
    );
    assert!(
        interner
            .get("triple")
            .map_or(false, |s| fns.contains_key(&s))
    );
}

#[test]
fn parse_multiline_function_with_binding() {
    let src = r"
    total(price: num, qty: num)
        netto = price * qty
        netto * 1.22
";
    let (fns, interner) = single_fn(src).unwrap();
    let total_sym = interner.get("total").unwrap();
    let f = &fns[&total_sym];
    let netto_sym = interner.get("netto").unwrap();
    // Body should be Let { name: netto_sym, value: price * qty, body: netto * 1.22 }
    assert!(matches!(f.body.root(), Expr::Let { name, .. } if *name == netto_sym));
}

#[test]
fn parse_function_calling_another() {
    let src = r"
    vat(p: num) : p * 1.22
    total(p: num, q: num) : vat(p * q)
";
    let (fns, interner) = single_fn(src).unwrap();
    assert_eq!(fns.len(), 2);
    let total_sym = interner.get("total").unwrap();
    let vat_sym = interner.get("vat").unwrap();
    let body = fns[&total_sym].body.root();
    assert!(matches!(body, Expr::FunctionCall { name, .. } if *name == vat_sym));
}

#[test]
fn parse_empty_logic_block() {
    let fns = parse_logic("", &mut StringInterner::new()).unwrap();
    assert!(fns.is_empty());
}

#[test]
fn parse_logic_blank_only() {
    let fns = parse_logic("   \n  \n", &mut StringInterner::new()).unwrap();
    assert!(fns.is_empty());
}

// ────────────────────────────────────────────────────────────────────────
// Operator precedence (Pratt parser correctness)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn pratt_mul_before_add() {
    // `2 + 3 * 4` should parse as `2 + (3 * 4)`, not `(2 + 3) * 4`.
    let (fns, interner) = single_fn("    f() : 2 + 3 * 4\n").unwrap();
    let f_sym = interner.get("f").unwrap();
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(
        fns[&f_sym].body.root(),
        &fns[&f_sym].body.arena,
        &store,
        &fns,
    )
    .unwrap();
    // 2 + 12 = 14
    assert_eq!(
        result,
        Value::Decimal(14 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn pratt_parentheses_override_precedence() {
    // `(2 + 3) * 4` should be 20.
    let (fns, interner) = single_fn("    f() : (2 + 3) * 4\n").unwrap();
    let f_sym = interner.get("f").unwrap();
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(
        fns[&f_sym].body.root(),
        &fns[&f_sym].body.arena,
        &store,
        &fns,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Decimal(20 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn pratt_left_associativity_subtraction() {
    // `10 - 3 - 2` should be `(10 - 3) - 2 = 5`, NOT `10 - (3 - 2) = 9`.
    let (fns, interner) = single_fn("    f() : 10 - 3 - 2\n").unwrap();
    let f_sym = interner.get("f").unwrap();
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(
        fns[&f_sym].body.root(),
        &fns[&f_sym].body.arena,
        &store,
        &fns,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Decimal(5 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn pratt_left_associativity_division() {
    // `12 / 6 / 2` → `(12/6)/2 = 1`.
    let (fns, interner) = single_fn("    f() : 12 / 6 / 2\n").unwrap();
    let f_sym = interner.get("f").unwrap();
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(
        fns[&f_sym].body.root(),
        &fns[&f_sym].body.arena,
        &store,
        &fns,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Decimal(1 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn pratt_complex_expression() {
    // `1 + 2 * 3 + 4 / 2` = `1 + 6 + 2 = 9`
    let (fns, interner) = single_fn("    f() : 1 + 2 * 3 + 4 / 2\n").unwrap();
    let f_sym = interner.get("f").unwrap();
    let store = Rc::new(VariableStore::with_interner(interner.freeze()));
    let result = evaluate(
        fns[&f_sym].body.root(),
        &fns[&f_sym].body.arena,
        &store,
        &fns,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Decimal(9 * crate::core::types::DECIMAL_SCALE)
    );
}

// ────────────────────────────────────────────────────────────────────────
// DAG anti-recursion (security / Turing-completeness guardrail)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn error_direct_recursion_rejected() {
    // `f` calls itself → cycle A → A.
    let src = "    f(x: num) : f(x)\n";
    let result = parse_logic(src, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
        "expected cycle detection error, got: {result:?}"
    );
}

#[test]
fn error_mutual_recursion_rejected() {
    // `ping` calls `pong`, `pong` calls `ping` → cycle A → B → A.
    let src = r"
    ping(x: num) : pong(x)
    pong(x: num) : ping(x)
";
    let result = parse_logic(src, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
        "expected mutual recursion error, got: {result:?}"
    );
}

#[test]
fn dag_accepts_chain_a_calls_b() {
    // `b` is defined first (in-degree 0), `a` calls `b` — acyclic.
    let src = r"
    b(x: num) : x * 2
    a(x: num) : b(x)
";
    let fns = parse_logic(src, &mut StringInterner::new());
    assert!(fns.is_ok(), "expected Ok for acyclic DAG, got: {fns:?}");
}

#[test]
fn dag_accepts_three_level_chain() {
    let src = r"
    leaf(x: num) : x
    mid(x: num) : leaf(x) * 2
    root(x: num) : mid(x) + 1
";
    assert!(parse_logic(src, &mut StringInterner::new()).is_ok());
}

// ────────────────────────────────────────────────────────────────────────
// Parser failure paths
// ────────────────────────────────────────────────────────────────────────

#[test]
fn error_unknown_type_keyword() {
    let result = parse_logic("    f(x: integer) : x\n", &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("unknown type")),
        "expected unknown-type error, got: {result:?}"
    );
}

#[test]
fn error_function_without_body() {
    // Header with `:` but nothing after it.
    let result = parse_logic("    f(x: num) :\n", &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "expected ParseError for body-less function, got: {result:?}"
    );
}

#[test]
fn error_multiline_last_line_is_binding() {
    // The last line of a multi-line function must be a bare expression.
    let src = r"
    f(x: num)
        a = x * 2
        b = a + 1
";
    let result = parse_logic(src, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "expected ParseError when last body line is a binding, got: {result:?}"
    );
}

#[test]
fn test_case_insensitive_types_and_aliases() {
    let src = r"
    greet(name: Str) : name
    VAT(p: Number) : p * 1.22
    check(b: Boolean) : b
";
    let mut interner = StringInterner::new();
    let result = parse_logic(src, &mut interner).unwrap();
    assert!(
        interner
            .get("greet")
            .map_or(false, |s| result.contains_key(&s))
    );
    assert!(
        interner
            .get("VAT")
            .map_or(false, |s| result.contains_key(&s))
    );
    assert!(
        interner
            .get("check")
            .map_or(false, |s| result.contains_key(&s))
    );

    let greet_sym = interner.get("greet").unwrap();
    let greet_fn = &result[&greet_sym];
    assert_eq!(greet_fn.params[0].1, ValueType::Str);

    let vat_sym = interner.get("VAT").unwrap();
    let vat_fn = &result[&vat_sym];
    assert_eq!(vat_fn.params[0].1, ValueType::Num);

    let check_sym = interner.get("check").unwrap();
    let check_fn = &result[&check_sym];
    assert_eq!(check_fn.params[0].1, ValueType::Bool);
}

#[test]
fn parse_action_invalid_assignment() {
    let err = parse_action("= 5", &mut StringInterner::new()).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

// ────────────────────────────────────────────────────────────────────────
// get_system_time — argument must be a bare identifier (RM-04)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn get_system_time_bare_identifier_accepted() {
    let mut interner = StringInterner::new();
    let action = parse_action("get_system_time(my_var)", &mut interner).unwrap();
    let Action::Eval(tree) = action else {
        panic!("expected Action::Eval(FunctionCall), got: {action:?}");
    };
    let Expr::FunctionCall {
        name,
        args_start,
        args_len,
    } = tree.root()
    else {
        panic!("expected FunctionCall root, got: {:?}", tree.root());
    };
    assert_eq!(interner.resolve(*name), Some("get_system_time"));
    let my_var_sym = interner.get("my_var").unwrap();
    let arg_exprs: Vec<&Expr> = tree
        .arena
        .args(*args_start, *args_len)
        .iter()
        .map(|&id| &tree.arena[id])
        .collect();
    assert_eq!(arg_exprs, vec![&Expr::Variable(my_var_sym)]);
}

#[test]
fn get_system_time_field_access_target_rejected() {
    // The gap this closes: a target derived (even indirectly) from
    // untrusted data, e.g. `$form.evil`, must be rejected at parse time
    // — it can no longer even be expressed in a document.
    let err = parse_action("get_system_time($form.evil)", &mut StringInterner::new()).unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(_)),
        "expected ParseError for a field-access target, got: {err:?}"
    );
}

#[test]
fn get_system_time_string_literal_target_rejected() {
    // The pre-fix syntax (argument evaluated to a string used for a
    // dynamic lookup) must no longer parse at all.
    let err = parse_action(r#"get_system_time("my_var")"#, &mut StringInterner::new()).unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(_)),
        "expected ParseError for a string-literal target, got: {err:?}"
    );
}

#[test]
fn get_system_time_binop_target_rejected() {
    let err = parse_action("get_system_time(a + b)", &mut StringInterner::new()).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

#[test]
fn get_system_time_no_args_rejected() {
    let err = parse_action("get_system_time()", &mut StringInterner::new()).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

#[test]
fn get_system_time_two_args_rejected() {
    let err = parse_action("get_system_time(a, b)", &mut StringInterner::new()).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

#[test]
fn parse_variable_definition() {
    let mut interner = StringInterner::new();
    let fns = parse_logic("    count = 10\n", &mut interner).unwrap();
    let count_sym = interner.get("count").unwrap();
    assert!(fns.contains_key(&count_sym));
    let f = &fns[&count_sym];
    assert!(f.params.is_empty());
    assert_eq!(
        *f.body.root(),
        Expr::Literal(Value::Decimal(10 * crate::core::types::DECIMAL_SCALE))
    );
}

#[test]
fn error_recursive_variable_definition_rejected() {
    // count = count + 1 is a cycle
    let src = "    count = count + 1\n";
    let result = parse_logic(src, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
        "expected cycle error, got: {result:?}"
    );
}

#[test]
fn error_mutually_recursive_variables_rejected() {
    let src = r"
    a = b + 1
    b = a + 1
";
    let result = parse_logic(src, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
        "expected mutual recursion error, got: {result:?}"
    );
}

// ────────────────────────────────────────────────────────────────────────
// parse_action must not confuse `==` with assignment
// ────────────────────────────────────────────────────────────────────────

#[test]
fn parse_action_comparison_is_eval_not_assign() {
    let action = parse_action("x == 5", &mut StringInterner::new()).unwrap();
    assert!(
        matches!(action, Action::Eval(_)),
        "expected Eval for comparison expression, got: {action:?}"
    );
}

#[test]
fn parse_action_assignment_after_comparison_operators() {
    // `result = a != b` must parse as Assign{target="result", expr=Ne(a, b)}
    // (won't work without store variables, just check it parses as Assign)
    let action = parse_action("flag = true", &mut StringInterner::new()).unwrap();
    assert!(
        matches!(action, Action::Assign { ref target, .. } if target == "flag"),
        "expected Assign, got: {action:?}"
    );
}

// ────────────────────────────────────────────────────────────────────────
// Cursor-exhaustion: trailing tokens after a complete expression
// ────────────────────────────────────────────────────────────────────────

#[test]
fn parse_action_trailing_ident_after_assign_is_error() {
    // Simulates: `button click -> count = count + 1 class "btn"`
    // The expression `count + 1` is valid, but `class` is a leftover token.
    let err = parse_action("count = count + 1 class", &mut StringInterner::new()).unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
        "expected ParseError about unexpected token, got: {err:?}"
    );
}

#[test]
fn parse_action_trailing_string_after_assign_is_error() {
    // `count = count + 1 "leftover"` — trailing string literal
    let err = parse_action(
        r#"count = count + 1 "leftover""#,
        &mut StringInterner::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
        "expected ParseError about unexpected token, got: {err:?}"
    );
}

#[test]
fn parse_action_trailing_token_after_eval_is_error() {
    // `myFn() class "x"` — Eval action with trailing junk
    let err = parse_action("true class", &mut StringInterner::new()).unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
        "expected ParseError about unexpected token, got: {err:?}"
    );
}

#[test]
fn parse_action_trailing_token_after_navigate_is_error() {
    // `navigate "url" class "x"` — URL parsed, then junk
    let err = parse_action(
        r#"navigate "mizu://host/page" class "x""#,
        &mut StringInterner::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
        "expected ParseError about unexpected token, got: {err:?}"
    );
}

#[test]
fn parse_action_clean_assign_still_ok() {
    // Regression: valid action must still parse without error
    let action = parse_action("count = count + 1", &mut StringInterner::new()).unwrap();
    assert!(matches!(action, Action::Assign { ref target, .. } if target == "count"));
}

#[test]
fn parse_action_clean_navigate_still_ok() {
    let action =
        parse_action(r#"navigate "mizu://host/page""#, &mut StringInterner::new()).unwrap();
    assert!(matches!(action, Action::Navigate { .. }));
}

#[test]
fn parse_action_lowercase_get_is_error() {
    // Lowercase `get url -> var` must be rejected; only `GET(alias) -> var` is valid.
    let err = parse_action(
        r#"get "mizu://host/data" -> result"#,
        &mut StringInterner::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("get")),
        "expected ParseError about lowercase get, got: {err:?}"
    );
}

#[test]
fn parse_action_lowercase_post_is_error() {
    let err = parse_action(
        r#"post "mizu://host/submit" -> result"#,
        &mut StringInterner::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("post")),
        "expected ParseError about lowercase post, got: {err:?}"
    );
}

#[test]
fn parse_action_lowercase_put_is_error() {
    let err = parse_action(
        r#"put "mizu://host/item" -> result"#,
        &mut StringInterner::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("put")),
        "expected ParseError about lowercase put, got: {err:?}"
    );
}

#[test]
fn parse_action_lowercase_delete_is_error() {
    let err = parse_action(
        r#"delete "mizu://host/item/1" -> result"#,
        &mut StringInterner::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("delete")),
        "expected ParseError about lowercase delete, got: {err:?}"
    );
}

#[test]
fn parse_action_parenthesized_verb_case_sensitivity_bypass_is_error() {
    // MNT-01 follow-up: `get(alias) -> var`, `Get(alias) -> var`, and
    // `gEt(alias) -> var` must all be rejected — only the exact-case
    // `GET(alias) -> var` form is valid. Use a populated registry so the
    // alias itself can't be the rejection reason.
    use crate::parser::urls::{EndpointKind, UrlEndpoint, UrlRegistry};
    let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let mut interner = StringInterner::new();
    let sym = interner.get_or_intern("alias");
    registry.insert(
        sym,
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: "/api/alias".to_string(),
        },
    );

    for variant in [
        "get(alias) -> var",
        "Get(alias) -> var",
        "gEt(alias) -> var",
    ] {
        let err = parse_action_with_urls(variant, &mut interner, Some(&registry)).unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.to_ascii_lowercase().contains("get")),
            "expected ParseError about lowercase get for {variant:?}, got: {err:?}"
        );
    }

    // Exact-uppercase form must still parse successfully.
    let action =
        parse_action_with_urls("GET(alias) -> var", &mut interner, Some(&registry)).unwrap();
    assert!(matches!(
        action,
        Action::NetworkCall {
            method: NetworkMethod::Get,
            ..
        }
    ));
}

// ────────────────────────────────────────────────────────────────────────
// NetworkMethod — as_str round-trip
// ────────────────────────────────────────────────────────────────────────

#[test]
fn network_method_as_str_values() {
    assert_eq!(NetworkMethod::Get.as_str(), "GET");
    assert_eq!(NetworkMethod::Post.as_str(), "POST");
    assert_eq!(NetworkMethod::Put.as_str(), "PUT");
    assert_eq!(NetworkMethod::Delete.as_str(), "DELETE");
    assert_eq!(NetworkMethod::Query.as_str(), "QUERY");
}

// ────────────────────────────────────────────────────────────────────────
// Expanded ValueType parsing (list, dict, record, any)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn parse_params_list_becomes_list() {
    let src = "f(items: list<string>) : 1";
    let mut interner = StringInterner::new();
    let fns = parse_logic(src, &mut interner).unwrap();
    let sym = interner.get("f").unwrap();
    assert_eq!(
        fns[&sym].params[0].1,
        ValueType::List(Box::new(ValueType::Str))
    );
}

#[test]
fn parse_params_dict_annotation_is_error() {
    let src = "f(d: dict) : 1";
    let mut interner = StringInterner::new();
    let result = parse_logic(src, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("dict")),
        "expected ParseError for `dict`, got: {result:?}"
    );
}

#[test]
fn parse_params_record_annotation_is_error() {
    let src = "f(r: record) : 1";
    let mut interner = StringInterner::new();
    let result = parse_logic(src, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("record")),
        "expected ParseError for `record`, got: {result:?}"
    );
}

#[test]
fn parse_params_any_annotation_is_error() {
    let src = "f(x: any) : 1";
    let mut interner = StringInterner::new();
    let result = parse_logic(src, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("any")),
        "expected ParseError for `any`, got: {result:?}"
    );
}

#[test]
fn parse_params_no_annotation_produces_error() {
    // f(x) — no `: type` — parameter should error
    let src = "f(x) : x";
    let mut interner = StringInterner::new();
    let err = parse_logic(src, &mut interner).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
    if let MizuError::ParseError(msg) = err {
        println!("Actual error message: {}", msg);
        assert!(msg.contains("function `f`: parameter `x` requires a type annotation"));
    }
}

#[test]
fn parse_params_partial_annotation_produces_error() {
    // f(x: num, y) — first param typed, second untyped
    let src = "f(x: num, y) : x";
    let mut interner = StringInterner::new();
    let err = parse_logic(src, &mut interner).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
    if let MizuError::ParseError(msg) = err {
        assert!(msg.contains("function `f`: parameter `y` requires a type annotation"));
    }
}

// ────────────────────────────────────────────────────────────────────────
// parse_action_with_urls — HTTP verb without registry (registry = None)
// ────────────────────────────────────────────────────────────────────────

#[test]
fn parse_action_with_urls_get_no_registry_produces_network_call() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls("GET(users) -> result", &mut interner, None).unwrap();
    assert!(matches!(action, Action::NetworkCall {
            method: NetworkMethod::Get,
            ref target_var,
            ..
        } if target_var == "result"));
}

#[test]
fn parse_action_with_urls_get_with_path_param_no_registry() {
    // GET(alias, path_param) — second slot is path_param, no payload
    let mut interner = StringInterner::new();
    let action =
        parse_action_with_urls("GET(users, user_id) -> data", &mut interner, None).unwrap();
    if let Action::NetworkCall {
        method,
        payload,
        path_param,
        ..
    } = action
    {
        assert_eq!(method, NetworkMethod::Get);
        assert!(payload.is_none(), "GET must never have a payload");
        assert!(path_param.is_some(), "GET second arg should be path_param");
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn parse_action_with_urls_post_with_payload_no_registry() {
    // POST(alias, payload) — second slot is payload
    let mut interner = StringInterner::new();
    let action =
        parse_action_with_urls(r#"POST(orders, $form) -> resp"#, &mut interner, None).unwrap();
    if let Action::NetworkCall {
        method,
        payload,
        path_param,
        ..
    } = action
    {
        assert_eq!(method, NetworkMethod::Post);
        assert!(payload.is_some(), "POST second arg should be payload");
        assert!(path_param.is_none());
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn parse_action_with_urls_post_with_payload_and_path_param_no_registry() {
    // POST(alias, payload, path_param) — all three slots
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form, order_id) -> resp"#,
        &mut interner,
        None,
    )
    .unwrap();
    if let Action::NetworkCall {
        method,
        payload,
        path_param,
        ..
    } = action
    {
        assert_eq!(method, NetworkMethod::Post);
        assert!(payload.is_some());
        assert!(path_param.is_some());
    } else {
        panic!("expected NetworkCall");
    }
}

// ────────────────────────────────────────────────────────────────────────
// NetworkCall — `as <keyword>` payload format clause
// ────────────────────────────────────────────────────────────────────────

#[test]
fn network_call_with_no_as_clause_defaults_to_json() {
    let mut interner = StringInterner::new();
    let action =
        parse_action_with_urls(r#"POST(orders, $form) -> resp"#, &mut interner, None).unwrap();
    if let Action::NetworkCall {
        format, target_var, ..
    } = action
    {
        assert_eq!(format, PayloadFormat::Json);
        assert_eq!(target_var, "resp");
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn network_call_as_form_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp as form"#,
        &mut interner,
        None,
    )
    .unwrap();
    if let Action::NetworkCall {
        format, target_var, ..
    } = action
    {
        assert_eq!(format, PayloadFormat::Form);
        assert_eq!(target_var, "resp");
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn network_call_as_text_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp as text"#,
        &mut interner,
        None,
    )
    .unwrap();
    assert!(matches!(
        action,
        Action::NetworkCall {
            format: PayloadFormat::Text,
            ..
        }
    ));
}

#[test]
fn network_call_as_yaml_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp as yaml"#,
        &mut interner,
        None,
    )
    .unwrap();
    assert!(matches!(
        action,
        Action::NetworkCall {
            format: PayloadFormat::Yaml,
            ..
        }
    ));
}

#[test]
fn network_call_as_multipart_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp as multipart"#,
        &mut interner,
        None,
    )
    .unwrap();
    assert!(matches!(
        action,
        Action::NetworkCall {
            format: PayloadFormat::Multipart,
            ..
        }
    ));
}

#[test]
fn network_call_as_json_explicit_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp as json"#,
        &mut interner,
        None,
    )
    .unwrap();
    assert!(matches!(
        action,
        Action::NetworkCall {
            format: PayloadFormat::Json,
            ..
        }
    ));
}

#[test]
fn network_call_as_unknown_keyword_is_hard_parse_error() {
    let mut interner = StringInterner::new();
    let err = parse_action_with_urls(r#"POST(orders, $form) -> resp as xml"#, &mut interner, None)
        .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("xml") && msg.contains("payload format")),
        "expected a payload-format parse error mentioning `xml`, got: {err:?}"
    );
}

#[test]
fn network_call_as_clause_works_on_get_without_payload() {
    // `as` is grammatically accepted even on body-less verbs; it is
    // simply unused since there is no payload to serialise.
    let mut interner = StringInterner::new();
    let action =
        parse_action_with_urls(r#"GET(users) -> result as text"#, &mut interner, None).unwrap();
    assert!(matches!(
        action,
        Action::NetworkCall {
            format: PayloadFormat::Text,
            payload: None,
            ..
        }
    ));
}

// ────────────────────────────────────────────────────────────────────────
// NetworkCall — `header "<name>" <expr>` custom header clause
// ────────────────────────────────────────────────────────────────────────

#[test]
fn network_call_with_single_header_clause_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp header "X-Idempotency-Key" idempotency_id"#,
        &mut interner,
        None,
    )
    .unwrap();
    if let Action::NetworkCall {
        headers,
        target_var,
        ..
    } = action
    {
        assert_eq!(target_var, "resp");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "X-Idempotency-Key");
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn network_call_with_multiple_header_clauses_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp header "X-Foo" foo_var header "X-Bar" bar_var"#,
        &mut interner,
        None,
    )
    .unwrap();
    if let Action::NetworkCall { headers, .. } = action {
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0, "X-Foo");
        assert_eq!(headers[1].0, "X-Bar");
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn network_call_with_as_and_header_clauses_together_is_parsed() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp as form header "X-Foo" foo_var"#,
        &mut interner,
        None,
    )
    .unwrap();
    if let Action::NetworkCall {
        headers, format, ..
    } = action
    {
        assert_eq!(format, PayloadFormat::Form);
        assert_eq!(headers.len(), 1);
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn network_call_header_value_accepts_arbitrary_expression() {
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls(
        r#"POST(orders, $form) -> resp header "X-Sum" (a + b)"#,
        &mut interner,
        None,
    )
    .unwrap();
    assert!(matches!(action, Action::NetworkCall { .. }));
}

#[test]
fn network_call_header_denylisted_name_is_hard_parse_error() {
    let mut interner = StringInterner::new();
    for reserved in [
        "Authorization",
        "Content-Type",
        "Host",
        "Content-Length",
        "Connection",
        "Transfer-Encoding",
        "Upgrade",
        "TE",
        "Trailer",
        "Proxy-Foo",
        "Sec-Foo",
        "Mizu-Foo",
    ] {
        let src = format!(r#"POST(orders, $form) -> resp header "{reserved}" some_var"#);
        let err = parse_action_with_urls(&src, &mut interner, None).unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("reserved")),
            "expected `{reserved}` to be rejected as reserved, got: {err:?}"
        );
    }
}

#[test]
fn network_call_header_invalid_name_syntax_is_hard_parse_error() {
    let mut interner = StringInterner::new();
    // A space is not a legal HTTP header-name token character.
    let err = parse_action_with_urls(
        r#"POST(orders, $form) -> resp header "X Invalid Name" some_var"#,
        &mut interner,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

#[test]
fn network_call_header_missing_string_name_is_hard_parse_error() {
    let mut interner = StringInterner::new();
    let err = parse_action_with_urls(
        r#"POST(orders, $form) -> resp header some_var"#,
        &mut interner,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

#[test]
fn parse_action_with_urls_delete_no_path_param_no_registry() {
    // DELETE(alias) — no path_param
    let mut interner = StringInterner::new();
    let action = parse_action_with_urls("DELETE(item) -> ok", &mut interner, None).unwrap();
    if let Action::NetworkCall {
        method,
        payload,
        path_param,
        ..
    } = action
    {
        assert_eq!(method, NetworkMethod::Delete);
        assert!(payload.is_none(), "DELETE must never have a payload");
        assert!(path_param.is_none());
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn parse_action_with_urls_delete_with_path_param_no_registry() {
    // DELETE(alias, path_param) — second slot is path_param
    let mut interner = StringInterner::new();
    let action =
        parse_action_with_urls("DELETE(items, item_id) -> ok", &mut interner, None).unwrap();
    if let Action::NetworkCall {
        method,
        payload,
        path_param,
        ..
    } = action
    {
        assert_eq!(method, NetworkMethod::Delete);
        assert!(payload.is_none(), "DELETE must never have a payload");
        assert!(path_param.is_some());
    } else {
        panic!("expected NetworkCall");
    }
}

#[test]
fn parse_action_with_urls_get_with_three_args_is_error() {
    // GET(alias, path_param, extra) — GET does not accept a body, so 3 args → error
    let mut interner = StringInterner::new();
    let err = parse_action_with_urls("GET(users, user_id, extra) -> data", &mut interner, None)
        .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("does not accept a body")),
        "expected ParseError about no body, got: {err:?}"
    );
}

#[test]
fn parse_action_with_urls_get_registry_unknown_alias_is_error() {
    use crate::parser::urls::{EndpointKind, UrlEndpoint, UrlRegistry};
    let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let mut interner = StringInterner::new();
    // Register `users` as an API endpoint so the alias *exists*
    let sym = interner.get_or_intern("users");
    registry.insert(
        sym,
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: "/api/users".to_string(),
        },
    );

    // `unknown_alias` is NOT in the registry → compile error
    let err = parse_action_with_urls(
        "GET(unknown_alias) -> result",
        &mut interner,
        Some(&registry),
    )
    .unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("not defined in the `urls` block")),
        "expected ParseError about missing alias, got: {err:?}"
    );
}

#[test]
fn parse_action_with_urls_get_registry_media_alias_is_error() {
    use crate::parser::urls::{EndpointKind, UrlEndpoint, UrlRegistry};
    let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let mut interner = StringInterner::new();
    let sym = interner.get_or_intern("logo");
    registry.insert(
        sym,
        UrlEndpoint {
            kind: EndpointKind::Media,
            raw_target: "mizu://media/logo.png".to_string(),
        },
    );

    let err =
        parse_action_with_urls("GET(logo) -> result", &mut interner, Some(&registry)).unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("media")),
        "expected ParseError about media endpoint, got: {err:?}"
    );
}

#[test]
fn parse_action_with_urls_get_registry_valid_alias_ok() {
    use crate::parser::urls::{EndpointKind, UrlEndpoint, UrlRegistry};
    let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let mut interner = StringInterner::new();
    let sym = interner.get_or_intern("users");
    registry.insert(
        sym,
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: "/api/users".to_string(),
        },
    );

    let action =
        parse_action_with_urls("GET(users) -> data", &mut interner, Some(&registry)).unwrap();
    assert!(matches!(
        action,
        Action::NetworkCall {
            method: NetworkMethod::Get,
            ..
        }
    ));
}

#[test]
fn parse_action_with_urls_get_missing_parens_is_error() {
    let mut interner = StringInterner::new();
    let err = parse_action_with_urls("GET users -> result", &mut interner, None).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

#[test]
fn parse_action_with_urls_get_missing_arrow_is_error() {
    let mut interner = StringInterner::new();
    let err = parse_action_with_urls("GET(users)", &mut interner, None).unwrap_err();
    assert!(matches!(err, MizuError::ParseError(_)));
}

// ────────────────────────────────────────────────────────────────────────
// parse_root_timers — happy paths and error cases
// ────────────────────────────────────────────────────────────────────────

#[test]
fn parse_root_timers_milliseconds_literal() {
    let src = "timer 500ms -> count = count + 1";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].interval, TimerInterval::Millis(500));
    assert!(matches!(timers[0].action, Action::Assign { ref target, .. } if target == "count"));
}

#[test]
fn parse_root_timers_bare_number_milliseconds() {
    let src = "timer 1000 -> flag = true";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].interval, TimerInterval::Millis(1000));
}

#[test]
fn parse_root_timers_variable_interval() {
    // Use a name that does NOT end in "ms" so it isn't misidentified as a literal.
    let src = "timer tick_rate -> refresh = true";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(
        timers[0].interval,
        TimerInterval::Variable("tick_rate".to_string())
    );
}

#[test]
fn parse_root_timers_multiple_timers() {
    let src = "timer 100ms -> a = 1\ntimer 200ms -> b = 2";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 2);
    assert_eq!(timers[0].interval, TimerInterval::Millis(100));
    assert_eq!(timers[1].interval, TimerInterval::Millis(200));
}

#[test]
fn parse_root_timers_non_timer_lines_are_ignored() {
    // parse_root_timers skips non-timer lines; parse_logic handles functions
    let src = "double(x: num) : x + x\ntimer 300ms -> flag = true";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].interval, TimerInterval::Millis(300));
}

#[test]
fn parse_root_timers_at_the_limit_is_accepted() {
    let src = (0..*crate::parser::logic::MAX_ROOT_TIMERS)
        .map(|i| format!("timer 100ms -> a = {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(&src, &mut interner).expect("exactly the limit must parse");
    assert_eq!(timers.len(), *crate::parser::logic::MAX_ROOT_TIMERS);
}

#[test]
fn parse_root_timers_beyond_the_limit_is_rejected() {
    // Every timer is an independent, self-rearming event source, so an
    // unbounded count is an unbounded dispatch rate into the logic worker —
    // rejected outright rather than silently truncated, so a document never
    // appears to work while running something other than what it declares.
    let src = (0..*crate::parser::logic::MAX_ROOT_TIMERS + 1)
        .map(|i| format!("timer 16ms -> a = {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut interner = StringInterner::new();
    let err = parse_root_timers(&src, &mut interner)
        .expect_err("more timers than the limit must be rejected");
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("timer")),
        "expected a ParseError naming the timer limit, got {err:?}"
    );
}

#[test]
fn parse_root_timers_missing_arrow_is_error() {
    let src = "timer 500ms count = count + 1";
    let mut interner = StringInterner::new();
    let err = parse_root_timers(src, &mut interner).unwrap_err();
    assert!(
        matches!(err, MizuError::ParseError(ref msg) if msg.contains("->")),
        "expected ParseError about missing `->`, got: {err:?}"
    );
}

#[test]
fn parse_root_timers_empty_source_returns_empty_vec() {
    let mut interner = StringInterner::new();
    let timers = parse_root_timers("", &mut interner).unwrap();
    assert!(timers.is_empty());
}

#[test]
fn timer_interval_seconds() {
    let src = "timer 60s -> x = 1";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].interval, TimerInterval::Millis(60000));
}

#[test]
fn timer_interval_fractional_seconds() {
    let src = "timer 1.5s -> x = 1";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].interval, TimerInterval::Millis(1500));
}

#[test]
fn timer_interval_ms_unchanged() {
    let src = "timer 500ms -> x = 1";
    let mut interner = StringInterner::new();
    let timers = parse_root_timers(src, &mut interner).unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].interval, TimerInterval::Millis(500));
}

// ── Depth guard tests ────────────────────────────────────────────────────

#[test]
fn parse_deeply_nested_rejected() {
    // 300 nested parentheses — must produce a ParseError, not a stack overflow.
    let depth = 300usize;
    let src = format!("{}{}{}", "(".repeat(depth), "1", ")".repeat(depth));
    let mut interner = StringInterner::new();
    let result = super::super::parse_expr_standalone(&src, &mut interner);
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("nesting too deep"),
                "error must mention nesting depth: {msg}"
            );
        }
        other => panic!("expected ParseError for deeply nested expr, got: {other:?}"),
    }
}

#[test]
fn parse_deep_unary_chain_rejected() {
    // 300 consecutive `!` operators — must produce a ParseError, not a stack overflow.
    let src = format!("{}true", "!".repeat(300));
    let mut interner = StringInterner::new();
    let result = super::super::parse_expr_standalone(&src, &mut interner);
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("nesting too deep"),
                "error must mention nesting depth: {msg}"
            );
        }
        other => panic!("expected ParseError for deep unary chain, got: {other:?}"),
    }
}

#[test]
fn parse_normal_nesting_ok() {
    // 10 levels of nesting is well within the limit and must parse successfully.
    let depth = 10usize;
    let src = format!("{}{}{}", "(".repeat(depth), "42", ")".repeat(depth));
    let mut interner = StringInterner::new();
    let result = super::super::parse_expr_standalone(&src, &mut interner);
    assert!(
        result.is_ok(),
        "normal nesting depth must parse without error: {result:?}"
    );
}
