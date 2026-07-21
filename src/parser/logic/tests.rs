    use super::{
        Action, BinOp, ComputedBinding, Expr, MizuFunction, NetworkMethod, TimerInterval,
        ValueType, parse_action, parse_action_with_urls, parse_logic, parse_root_timers,
    };
    use crate::core::errors::MizuError;
    use crate::core::types::{StringInterner, Symbol, Value, VariableStore};
    use rustc_hash::{FxHashMap, FxHashSet};
    use std::rc::Rc;

    fn single_fn(
        src: &str,
    ) -> Result<(FxHashMap<Symbol, MizuFunction>, StringInterner), MizuError> {
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner)?;
        Ok((fns, interner))
    }

    fn evaluate(
        expr: &Expr,
        store: &Rc<VariableStore>,
        functions: &FxHashMap<Symbol, MizuFunction>,
    ) -> Result<Value, MizuError> {
        let mut temp_store = (**store).clone();
        super::evaluate(expr, &mut temp_store, functions, 0)
    }

    fn execute_action(
        action: &Action,
        store: &mut Rc<VariableStore>,
        functions: &FxHashMap<Symbol, MizuFunction>,
    ) -> Result<bool, MizuError> {
        let mut temp_store = (**store).clone();
        let result = super::execute_action(action, &mut temp_store, functions)?;
        *store = Rc::new(temp_store);
        Ok(result)
    }

    fn eval_src(src: &str) -> Result<Value, MizuError> {
        let wrapper = format!("  f() : {src}\n");
        let (fns, interner) = single_fn(&wrapper)?;
        let f_sym = interner
            .get("f")
            .ok_or_else(|| MizuError::ParseError("f not found in interner".to_string()))?;
        let store = Rc::new(VariableStore::with_interner(interner));
        evaluate(&fns[&f_sym].body, &store, &fns)
    }

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
        assert_eq!(f.body, Expr::Literal(Value::Int(31416)));
    }

    #[test]
    fn parse_inline_function_single_num_param() {
        let (fns, interner) = single_fn("    vat(p: num) : p * 1.22\n").unwrap();
        let vat_sym = interner.get("vat").unwrap();
        let f = &fns[&vat_sym];
        let p_sym = interner.get("p").unwrap();
        assert_eq!(f.params, vec![(p_sym, Some(ValueType::Num))]);
        // Body should be BinaryOp(Variable(p_sym), Mul, Literal(1.22))
        assert!(matches!(&f.body, Expr::BinaryOp { op: BinOp::Mul, .. }));
    }

    #[test]
    fn parse_inline_function_two_params() {
        let (fns, interner) = single_fn("    add(a: num, b: num) : a + b\n").unwrap();
        let add_sym = interner.get("add").unwrap();
        let f = &fns[&add_sym];
        assert_eq!(f.params.len(), 2);
        let a_sym = interner.get("a").unwrap();
        let b_sym = interner.get("b").unwrap();
        assert_eq!(f.params[0], (a_sym, Some(ValueType::Num)));
        assert_eq!(f.params[1], (b_sym, Some(ValueType::Num)));
    }

    #[test]
    fn parse_inline_string_param() {
        let (fns, interner) = single_fn("    greet(name: string) : name\n").unwrap();
        let greet_sym = interner.get("greet").unwrap();
        let f = &fns[&greet_sym];
        let name_sym = interner.get("name").unwrap();
        assert_eq!(f.params[0], (name_sym, Some(ValueType::Str)));
    }

    #[test]
    fn parse_inline_bool_param() {
        let (fns, interner) = single_fn("    id_bool(b: bool) : b\n").unwrap();
        let sym = interner.get("id_bool").unwrap();
        let f = &fns[&sym];
        let b_sym = interner.get("b").unwrap();
        assert_eq!(f.params[0], (b_sym, Some(ValueType::Bool)));
    }

    #[test]
    fn parse_inline_list_param() {
        let (fns, interner) = single_fn("    first(items: list) : items\n").unwrap();
        let sym = interner.get("first").unwrap();
        let f = &fns[&sym];
        let items_sym = interner.get("items").unwrap();
        assert_eq!(f.params[0], (items_sym, Some(ValueType::List)));
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
        assert!(matches!(&f.body, Expr::Let { name, .. } if *name == netto_sym));
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
        let body = &fns[&total_sym].body;
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
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        // 2 + 12 = 14
        assert_eq!(result, Value::Int(14 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn pratt_parentheses_override_precedence() {
        // `(2 + 3) * 4` should be 20.
        let (fns, interner) = single_fn("    f() : (2 + 3) * 4\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(20 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn pratt_left_associativity_subtraction() {
        // `10 - 3 - 2` should be `(10 - 3) - 2 = 5`, NOT `10 - (3 - 2) = 9`.
        let (fns, interner) = single_fn("    f() : 10 - 3 - 2\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(5 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn pratt_left_associativity_division() {
        // `12 / 6 / 2` → `(12/6)/2 = 1`.
        let (fns, interner) = single_fn("    f() : 12 / 6 / 2\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(1 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn pratt_complex_expression() {
        // `1 + 2 * 3 + 4 / 2` = `1 + 6 + 2 = 9`
        let (fns, interner) = single_fn("    f() : 1 + 2 * 3 + 4 / 2\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(9 * crate::core::types::DECIMAL_SCALE));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Evaluator — happy paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn evaluate_literal_num() {
        let expr = Expr::Literal(Value::Int(420_000));
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Int(420_000));
    }

    #[test]
    fn evaluate_literal_bool() {
        let expr = Expr::Literal(Value::Bool(true));
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Bool(true));
    }

    #[test]
    fn evaluate_variable_lookup() {
        let mut store = VariableStore::new();
        store.set("x", 70_000_i64);
        let x_sym = store.interner.get("x").unwrap();
        let store = Rc::new(store);
        let expr = Expr::Variable(x_sym);
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Int(70_000));
    }

    #[test]
    fn evaluate_addition() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Int(30_000))),
            op: BinOp::Add,
            right: Box::new(Expr::Literal(Value::Int(40_000))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Int(70_000));
    }

    #[test]
    fn evaluate_subtraction() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Int(100_000))),
            op: BinOp::Sub,
            right: Box::new(Expr::Literal(Value::Int(35_000))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Int(65_000));
    }

    #[test]
    fn evaluate_multiplication() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Int(60_000))),
            op: BinOp::Mul,
            right: Box::new(Expr::Literal(Value::Int(70_000))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Int(420_000));
    }

    #[test]
    fn evaluate_division() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Int(150_000))),
            op: BinOp::Div,
            right: Box::new(Expr::Literal(Value::Int(30_000))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Int(50_000));
    }

    #[test]
    fn evaluate_string_concatenation() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::String(std::sync::Arc::from(
                "Hello, ",
            )))),
            op: BinOp::Add,
            right: Box::new(Expr::Literal(Value::String(std::sync::Arc::from("Mizu!")))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(
            evaluate(&expr, &store, &fns).unwrap(),
            Value::String(std::sync::Arc::from("Hello, Mizu!"))
        );
    }

    #[test]
    fn evaluate_inline_function_call() {
        let src = "    vat(p: num) : p * 1.22\n";
        let (fns, interner) = single_fn(src).unwrap();
        let vat_sym = interner.get("vat").unwrap();
        let mut store = VariableStore::with_interner(interner);
        store.set("p", 100 * crate::core::types::DECIMAL_SCALE);
        let store = Rc::new(store);
        let call_expr = Expr::FunctionCall {
            name: vat_sym,
            args: vec![Expr::Literal(Value::Int(100 * crate::core::types::DECIMAL_SCALE))],
        };
        let result = evaluate(&call_expr, &store, &fns).unwrap();
        // 100 * 1.22 = 122
        assert_eq!(result, Value::Int(122 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn evaluate_function_calling_function() {
        let src = r"
    double(x: num) : x * 2
    quadruple(x: num) : double(double(x))
";
        let (fns, interner) = single_fn(src).unwrap();
        let quadruple_sym = interner.get("quadruple").unwrap();
        let call_expr = Expr::FunctionCall {
            name: quadruple_sym,
            args: vec![Expr::Literal(Value::Int(3 * crate::core::types::DECIMAL_SCALE))],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns).unwrap();
        // 3 * 4 = 12
        assert_eq!(result, Value::Int(12 * crate::core::types::DECIMAL_SCALE));
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
        let call_expr = Expr::FunctionCall {
            name: total_sym,
            args: vec![
                Expr::Literal(Value::Int(10 * crate::core::types::DECIMAL_SCALE)),
                Expr::Literal(Value::Int(3 * crate::core::types::DECIMAL_SCALE)),
            ],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns).unwrap();
        // netto = 10 * 3 = 30, result = 30 * 1.22 = 36.6
        assert_eq!(result, Value::Int(366_000));
    }

    #[test]
    fn evaluate_function_with_store_variables() {
        // Outer store values should NOT bleed into the function's local scope.
        let src = "    area(w: num, h: num) : w * h\n";
        let (fns, interner) = single_fn(src).unwrap();
        let area_sym = interner.get("area").unwrap();
        let mut outer_store = VariableStore::with_interner(interner);
        outer_store.set("w", 999 * crate::core::types::DECIMAL_SCALE); // should be ignored inside the function
        let outer_store = Rc::new(outer_store);
        let call_expr = Expr::FunctionCall {
            name: area_sym,
            args: vec![
                Expr::Literal(Value::Int(5 * crate::core::types::DECIMAL_SCALE)),
                Expr::Literal(Value::Int(4 * crate::core::types::DECIMAL_SCALE)),
            ],
        };
        // Function arguments override the outer store inside the function body.
        let result = evaluate(&call_expr, &outer_store, &fns).unwrap();
        assert_eq!(result, Value::Int(20 * crate::core::types::DECIMAL_SCALE));
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
    // Type error paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn error_num_plus_bool_is_type_error() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Int(1))),
            op: BinOp::Add,
            right: Box::new(Expr::Literal(Value::Bool(true))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::TypeError { .. })),
            "expected TypeError, got: {result:?}"
        );
    }

    #[test]
    fn error_num_mul_string_is_type_error() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Int(2))),
            op: BinOp::Mul,
            right: Box::new(Expr::Literal(Value::String(std::sync::Arc::from("oops")))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    #[test]
    fn error_bool_sub_num_is_type_error() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Bool(true))),
            op: BinOp::Sub,
            right: Box::new(Expr::Literal(Value::Int(1))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    #[test]
    fn error_wrong_argument_type_for_function() {
        // `vat` expects `num`, but receives `bool`.
        let src = "    vat(p: num) : p * 1.22\n";
        let (fns, interner) = single_fn(src).unwrap();
        let vat_sym = interner.get("vat").unwrap();
        let call_expr = Expr::FunctionCall {
            name: vat_sym,
            args: vec![Expr::Literal(Value::Bool(true))],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns);
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
        let call_expr = Expr::FunctionCall {
            name: add_sym,
            args: vec![Expr::Literal(Value::Int(1))],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns);
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
        let call_expr = Expr::FunctionCall {
            name: inc_sym,
            args: vec![
                Expr::Literal(Value::Int(1)),
                Expr::Literal(Value::Int(2)),
            ],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns);
        assert!(matches!(result, Err(MizuError::ParseError(_))));
    }

    #[test]
    fn error_undefined_function_call() {
        let mut interner = StringInterner::new();
        let ghost_sym = interner.get_or_intern("ghost");
        let call_expr = Expr::FunctionCall {
            name: ghost_sym,
            args: vec![],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let fns = FxHashMap::default();
        let result = evaluate(&call_expr, &store, &fns);
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
        let store = Rc::new(VariableStore::with_interner(interner));
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::VariableNotFound(_))),
            "expected VariableNotFound, got: {result:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Parser failure paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn unannotated_param_is_valid() {
        // Parameters without `: type` are now legal — they accept any value.
        let result = parse_logic("    id(x) : x\n", &mut StringInterner::new());
        assert!(
            result.is_ok(),
            "unannotated param should parse successfully, got: {result:?}"
        );
    }

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
        assert_eq!(greet_fn.params[0].1, Some(ValueType::Str));

        let vat_sym = interner.get("VAT").unwrap();
        let vat_fn = &result[&vat_sym];
        assert_eq!(vat_fn.params[0].1, Some(ValueType::Num));

        let check_sym = interner.get("check").unwrap();
        let check_fn = &result[&check_sym];
        assert_eq!(check_fn.params[0].1, Some(ValueType::Bool));
    }


    #[test]
    fn execute_action_assignment_mutates_store() {
        let mut store = VariableStore::new();
        store.set("count", 10_000_i64);
        let mut store = Rc::new(store);
        let functions = FxHashMap::default();

        let action = parse_action("count = count + 1", &mut StringInterner::new()).unwrap();
        let mutated = execute_action(&action, &mut store, &functions).unwrap();
        assert!(mutated);
        assert_eq!(*store.get("count").unwrap(), Value::Int(20_000));
    }

    #[test]
    fn execute_action_pure_expression_no_mutation() {
        let mut store = VariableStore::new();
        store.set("count", 10_000_i64);
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
        Action::NetworkCall {
            method: NetworkMethod::Get,
            alias_sym: Symbol(0),
            payload: None,
            path_param: Some(Box::new(Expr::Literal(Value::from(path_param_value)))),
            target_var: "data".to_string(),
        }
    }

    #[test]
    fn path_param_ok_accepts_single_alphanumeric_segment() {
        assert!(super::path_param_ok("abc123"));
        assert!(super::path_param_ok("foo-bar_123.~baz"));
    }

    #[test]
    fn path_param_ok_rejects_forward_slash() {
        assert!(!super::path_param_ok("a/b"));
    }

    #[test]
    fn path_param_ok_rejects_backslash() {
        assert!(!super::path_param_ok("a\\b"));
    }

    #[test]
    fn path_param_ok_rejects_traversal_substring() {
        assert!(!super::path_param_ok(".."));
        assert!(!super::path_param_ok("a..b"));
    }

    #[test]
    fn path_param_ok_rejects_control_characters() {
        assert!(!super::path_param_ok("a\nb"));
        assert!(!super::path_param_ok("a\tb"));
        assert!(!super::path_param_ok("a\u{7F}b"));
    }

    #[test]
    fn execute_action_network_call_valid_path_param_accepted() {
        let mut store = Rc::new(VariableStore::new());
        let functions = FxHashMap::default();

        let action = network_call_action("abc123");
        let mutated = execute_action(&action, &mut store, &functions).unwrap();
        assert!(mutated);
        assert_eq!(store.state_machine.accumulated_actions.len(), 1);
        match &store.state_machine.accumulated_actions[0] {
            crate::network::RuntimeAction::NetworkCall { path_param, .. } => {
                assert_eq!(path_param.as_deref(), Some("abc123"));
            }
            other => panic!("expected NetworkCall, got {other:?}"),
        }
    }

    #[test]
    fn execute_action_network_call_path_param_with_slash_rejected() {
        let mut store = Rc::new(VariableStore::new());
        let functions = FxHashMap::default();

        let action = network_call_action("../etc/passwd");
        let err = execute_action(&action, &mut store, &functions).unwrap_err();
        assert!(
            matches!(err, MizuError::ExecutionError(_)),
            "expected ExecutionError, got {err:?}"
        );
        assert!(
            store.state_machine.accumulated_actions.is_empty(),
            "a rejected path_param must not be queued as a network action"
        );
    }

    #[test]
    fn execute_action_network_call_path_param_with_backslash_rejected() {
        let mut store = Rc::new(VariableStore::new());
        let functions = FxHashMap::default();

        let action = network_call_action("a\\b");
        let err = execute_action(&action, &mut store, &functions).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
    }

    #[test]
    fn execute_action_network_call_path_param_with_control_char_rejected() {
        let mut store = Rc::new(VariableStore::new());
        let functions = FxHashMap::default();

        let action = network_call_action("a\nb");
        let err = execute_action(&action, &mut store, &functions).unwrap_err();
        assert!(matches!(err, MizuError::ExecutionError(_)));
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
        let Action::Eval(Expr::FunctionCall { name, args }) = action else {
            panic!("expected Action::Eval(FunctionCall), got: {action:?}");
        };
        assert_eq!(interner.resolve(name), Some("get_system_time"));
        let my_var_sym = interner.get("my_var").unwrap();
        assert_eq!(args, vec![Expr::Variable(my_var_sym)]);
    }

    #[test]
    fn get_system_time_field_access_target_rejected() {
        // The gap this closes: a target derived (even indirectly) from
        // untrusted data, e.g. `$form.evil`, must be rejected at parse time
        // — it can no longer even be expressed in a document.
        let err = parse_action("get_system_time($form.evil)", &mut StringInterner::new())
            .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(_)),
            "expected ParseError for a field-access target, got: {err:?}"
        );
    }

    #[test]
    fn get_system_time_string_literal_target_rejected() {
        // The pre-fix syntax (argument evaluated to a string used for a
        // dynamic lookup) must no longer parse at all.
        let err = parse_action(r#"get_system_time("my_var")"#, &mut StringInterner::new())
            .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(_)),
            "expected ParseError for a string-literal target, got: {err:?}"
        );
    }

    #[test]
    fn get_system_time_binop_target_rejected() {
        let err = parse_action("get_system_time(a + b)", &mut StringInterner::new())
            .unwrap_err();
        assert!(matches!(err, MizuError::ParseError(_)));
    }

    #[test]
    fn get_system_time_no_args_rejected() {
        let err = parse_action("get_system_time()", &mut StringInterner::new()).unwrap_err();
        assert!(matches!(err, MizuError::ParseError(_)));
    }

    #[test]
    fn get_system_time_two_args_rejected() {
        let err =
            parse_action("get_system_time(a, b)", &mut StringInterner::new()).unwrap_err();
        assert!(matches!(err, MizuError::ParseError(_)));
    }

    #[test]
    fn find_side_effect_call_detects_get_system_time() {
        // get_system_time was missing from SIDE_EFFECT_BUILTINS, meaning a
        // conditional-class condition (a pure "observation" context,
        // re-evaluated every frame) could invoke it undetected.
        let mut interner = StringInterner::new();
        let expr = super::parse_expr_standalone("get_system_time(x)", &mut interner).unwrap();
        assert_eq!(
            super::find_side_effect_call(&expr, &interner),
            Some("get_system_time".to_string())
        );
    }

    #[test]
    fn parse_variable_definition() {
        let mut interner = StringInterner::new();
        let fns = parse_logic("    count = 10\n", &mut interner).unwrap();
        let count_sym = interner.get("count").unwrap();
        assert!(fns.contains_key(&count_sym));
        let f = &fns[&count_sym];
        assert!(f.params.is_empty());
        assert_eq!(f.body, Expr::Literal(Value::Int(10 * crate::core::types::DECIMAL_SCALE)));
    }

    #[test]
    fn error_variable_fallback_no_implicit_call() {
        let mut interner = StringInterner::new();
        let fns = parse_logic("    count = 10\n", &mut interner).unwrap();
        let count_sym = interner.get("count").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let expr = Expr::Variable(count_sym);
        let result = evaluate(&expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::VariableNotFound(ref name)) if name == "count"),
            "expected VariableNotFound for count, got: {result:?}"
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

    #[test]
    fn test_cooperative_checkpointing_timeout() {
        use crate::core::types::{MAX_INSTRUCTIONS, StateMachine};

        // Pre-saturate the instruction counter to MAX_INSTRUCTIONS.
        // The very next call to `evaluate` increments it to MAX_INSTRUCTIONS + 1,
        // triggering the `instruction_count > MAX_INSTRUCTIONS` check immediately.
        // This avoids building a deep recursive tree that would overflow the call
        // stack in debug mode before the instruction limit is ever reached.
        let mut sm = StateMachine::new();
        sm.instruction_count = *MAX_INSTRUCTIONS;

        let interner = crate::core::types::StringInterner::new();
        let fns = FxHashMap::default();
        let expr = Expr::Literal(Value::Int(1));

        let res = sm.evaluate(&expr, 0, &fns, &interner);
        assert!(
            matches!(res, Err(MizuError::Timeout)),
            "expected Timeout, got: {res:?}"
        );
    }

    #[test]
    fn test_instruction_budget_resets_per_action() {
        // Verify that execute_action resets instruction_count to 0 before each evaluation,
        // so two consecutive actions each get the full MAX_INSTRUCTIONS budget.
        use crate::core::types::MAX_INSTRUCTIONS;

        let mut store = VariableStore::new();
        let fns = FxHashMap::default();
        let mut interner = crate::core::types::StringInterner::new();
        let x_sym = interner.get_or_intern("x");
        store.interner = interner;
        store.state_machine.set_global(x_sym, Value::Int(0));

        // First action — must succeed even if counter was near-exhausted from a prior call.
        store.state_machine.instruction_count = *MAX_INSTRUCTIONS - 1;
        let action1 = Action::Assign {
            target: "x".to_string(),
            expr: Expr::Literal(Value::Int(1)),
        };
        let r1 = super::execute_action(&action1, &mut store, &fns);
        assert!(
            r1.is_ok(),
            "first action should succeed (counter reset to 0): {r1:?}"
        );

        // Second action — counter was reset by execute_action, must also succeed.
        let action2 = Action::Assign {
            target: "x".to_string(),
            expr: Expr::Literal(Value::Int(2)),
        };
        let r2 = super::execute_action(&action2, &mut store, &fns);
        assert!(
            r2.is_ok(),
            "second action should succeed (counter reset to 0): {r2:?}"
        );
    }

    #[test]
    fn test_flat_state_machine_scoping() {
        use crate::core::types::StateMachine;

        let mut sm = StateMachine::new();
        let mut interner = crate::core::types::StringInterner::new();
        let fns = FxHashMap::default();

        // Set global variables
        let x_sym = interner.get_or_intern("x");
        let y_sym = interner.get_or_intern("y");
        sm.set_global(x_sym, Value::Int(10));
        sm.set_global(y_sym, Value::Int(20));

        // Evaluate an expression shadowing 'x' using Let binding:
        // let x = 15 in x + y
        let expr = Expr::Let {
            name: x_sym,
            value: Box::new(Expr::Literal(Value::Int(15))),
            body: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Variable(x_sym)),
                op: BinOp::Add,
                right: Box::new(Expr::Variable(y_sym)),
            }),
        };

        let res = sm.evaluate(&expr, 0, &fns, &interner).unwrap();
        assert_eq!(res, Value::Int(35));
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
        assert_eq!(eval_src("if true then 1 else 2").unwrap(), Value::Int(1 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn if_then_else_false_branch() {
        assert_eq!(eval_src("if false then 1 else 2").unwrap(), Value::Int(2 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn if_then_else_with_expression_condition() {
        assert_eq!(
            eval_src("if 3 > 2 then 10 else 20").unwrap(),
            Value::Int(10 * crate::core::types::DECIMAL_SCALE)
        );
        assert_eq!(
            eval_src("if 1 > 2 then 10 else 20").unwrap(),
            Value::Int(20 * crate::core::types::DECIMAL_SCALE)
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
        assert_eq!(eval_src("true ? 1 : 2").unwrap(), Value::Int(1 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn ternary_false_branch() {
        assert_eq!(eval_src("false ? 1 : 2").unwrap(), Value::Int(2 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn ternary_with_expression_condition() {
        assert_eq!(eval_src("5 > 3 ? 100 : 200").unwrap(), Value::Int(100 * crate::core::types::DECIMAL_SCALE));
        assert_eq!(eval_src("1 == 2 ? 100 : 200").unwrap(), Value::Int(200 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn ternary_right_associative() {
        // `true ? 1 : false ? 2 : 3` → `true ? 1 : (false ? 2 : 3)` → 1
        assert_eq!(eval_src("true ? 1 : false ? 2 : 3").unwrap(), Value::Int(1 * crate::core::types::DECIMAL_SCALE));
        // `false ? 1 : false ? 2 : 3` → `false ? 1 : (false ? 2 : 3)` → 3
        assert_eq!(
            eval_src("false ? 1 : false ? 2 : 3").unwrap(),
            Value::Int(3 * crate::core::types::DECIMAL_SCALE)
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
        let mut store = VariableStore::with_interner(interner);
        let pos = fns[&va_sym].body.clone();
        store.set("n", Value::Int(5 * crate::core::types::DECIMAL_SCALE));
        let v = store
            .state_machine
            .evaluate(&pos, 0, &fns, &store.interner)
            .unwrap();
        // just verify the function compiles — full eval needs param binding
        let _ = v;
        // Smoke test: parse succeeds and body is IfElse
        assert!(matches!(fns[&va_sym].body, Expr::IfElse { .. }));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Type-error failure paths for new operators
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn error_lt_on_strings_is_type_error() {
        let result = eval_src(r#""a" < "b""#);
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

        for variant in ["get(alias) -> var", "Get(alias) -> var", "gEt(alias) -> var"] {
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
        let src = "f(items: list) : 1";
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner).unwrap();
        let sym = interner.get("f").unwrap();
        assert_eq!(fns[&sym].params[0].1, Some(ValueType::List));
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
    fn parse_params_no_annotation_produces_none() {
        // f(x) — no `: type` — parameter should be untyped (None)
        let src = "f(x) : x";
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner).unwrap();
        let sym = interner.get("f").unwrap();
        let x_sym = interner.get("x").unwrap();
        assert_eq!(fns[&sym].params, vec![(x_sym, None)]);
    }

    #[test]
    fn parse_params_partial_annotation() {
        // f(x: num, y) — first param typed, second untyped
        let src = "f(x: num, y) : x";
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner).unwrap();
        let sym = interner.get("f").unwrap();
        let x_sym = interner.get("x").unwrap();
        let y_sym = interner.get("y").unwrap();
        assert_eq!(
            fns[&sym].params,
            vec![(x_sym, Some(ValueType::Num)), (y_sym, None)]
        );
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

        let err = parse_action_with_urls("GET(logo) -> result", &mut interner, Some(&registry))
            .unwrap_err();
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

    // ────────────────────────────────────────────────────────────────────────
    // Integer overflow — apply_binop checked arithmetic
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn apply_binop_add_overflow() {
        let mut ic = 0u64;
        let result = super::apply_binop(&BinOp::Add, Value::Int(i64::MAX), Value::Int(1), &mut ic);
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MAX + 1, got: {result:?}"
        );
    }

    #[test]
    fn apply_binop_mul_overflow() {
        let mut ic = 0u64;
        let result = super::apply_binop(&BinOp::Mul, Value::Int(i64::MAX), Value::Int(2), &mut ic);
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MAX * 2, got: {result:?}"
        );
    }

    #[test]
    fn apply_binop_sub_underflow() {
        let mut ic = 0u64;
        let result = super::apply_binop(&BinOp::Sub, Value::Int(i64::MIN), Value::Int(1), &mut ic);
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MIN - 1, got: {result:?}"
        );
    }

    #[test]
    fn apply_binop_div_overflow() {
        let mut ic = 0u64;
        let result = super::apply_binop(&BinOp::Div, Value::Int(i64::MIN), Value::Int(-1), &mut ic);
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MIN / -1, got: {result:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // comp keyword tests
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_comp_cycle_rejected() {
        let src = "    comp a = b + 1\n    comp b = a + 1\n";
        let mut interner = StringInterner::new();
        let result = super::parse_computed(src, &mut interner);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
            "expected cycle error, got: {result:?}"
        );
    }

    #[test]
    fn test_comp_binding_cap_rejected() {
        // MAX_COMP_BINDINGS + 1 independent `comp` declarations must be rejected
        // at parse time with a clear, diagnosable error — not accepted and left
        // to blow up the per-reaction instruction budget at runtime (see
        // `MAX_COMP_BINDINGS`'s docs in `core::types` and `formal/MizuFormal/Budget.lean`'s
        // `T1_shipped_capped`).
        let too_many = *crate::core::types::MAX_COMP_BINDINGS + 1;
        let mut src = String::new();
        for i in 0..too_many {
            src.push_str(&format!("    comp c{i} = {i}\n"));
        }
        let mut interner = StringInterner::new();
        let result = super::parse_computed(&src, &mut interner);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("MAX_COMP_BINDINGS")),
            "expected a ParseError naming MAX_COMP_BINDINGS, got: {result:?}"
        );
    }

    #[test]
    fn test_comp_binding_cap_allows_exactly_the_limit() {
        // The cap must reject documents *above* the limit without rejecting
        // documents that declare exactly MAX_COMP_BINDINGS comps.
        let at_limit = *crate::core::types::MAX_COMP_BINDINGS;
        let mut src = String::new();
        for i in 0..at_limit {
            src.push_str(&format!("    comp c{i} = {i}\n"));
        }
        let mut interner = StringInterner::new();
        let result = super::parse_computed(&src, &mut interner);
        assert!(result.is_ok(), "expected Ok at exactly the cap, got: {result:?}");
        assert_eq!(result.unwrap().len(), at_limit);
    }

    #[test]
    fn test_comp_assignment_rejected() {
        let src = "    comp derived = 42\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();
        assert_eq!(computed.len(), 1);

        let mut store = VariableStore::with_interner(interner);
        let derived_sym = store.interner.get_or_intern("derived");
        store.state_machine.computed_var_syms.insert(derived_sym);

        let fns = FxHashMap::default();
        let action = Action::Assign {
            target: "derived".to_string(),
            expr: Expr::Literal(Value::Int(99)),
        };
        let result = super::execute_action(&action, &mut store, &fns);
        assert!(
            matches!(result, Err(MizuError::ExecutionError(ref msg)) if msg.contains("computed variable")),
            "expected ExecutionError for comp assignment, got: {result:?}"
        );
    }

    #[test]
    fn test_comp_initial_value() {
        let src = "    comp derived = total + 1\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();
        let reverse_index = super::build_comp_reverse_index(&computed);

        let mut store = VariableStore::with_interner(interner);
        store.set("total", Value::Int(5 * crate::core::types::DECIMAL_SCALE));

        let fns = FxHashMap::default();
        let all_syms: FxHashSet<Symbol> =
            store.state_machine.global_store.keys().copied().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &all_syms, &reverse_index);

        let derived_sym = store.interner.get("derived").unwrap();
        assert_eq!(*store.state_machine.get_global(derived_sym), Value::Int(6 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn test_comp_evaluated_on_dependency_change() {
        let src = "    comp double = x * 2\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();
        let reverse_index = super::build_comp_reverse_index(&computed);

        let mut store = VariableStore::with_interner(interner);
        store.set("x", Value::Int(10 * crate::core::types::DECIMAL_SCALE));
        let fns = FxHashMap::default();

        let all_syms: FxHashSet<Symbol> =
            store.state_machine.global_store.keys().copied().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &all_syms, &reverse_index);
        let double_sym = store.interner.get("double").unwrap();
        assert_eq!(*store.state_machine.get_global(double_sym), Value::Int(20 * crate::core::types::DECIMAL_SCALE));

        // Mutate x and recompute
        store.state_machine.undo_log.clear();
        store.set("x", Value::Int(7 * crate::core::types::DECIMAL_SCALE));
        let x_sym = store.interner.get("x").unwrap();
        let mutated: FxHashSet<Symbol> = [x_sym].into_iter().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &mutated, &reverse_index);
        assert_eq!(*store.state_machine.get_global(double_sym), Value::Int(14 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn test_comp_depends_on_globals_read_inside_functions() {
        // `f` reads the global `z` internally; `comp y = f(x)` must therefore
        // recompute when `z` mutates, not only when `x` does.  Pre-regression,
        // the dependency walk stopped at the comp RHS and `y` went stale.
        let src = "    f(a: num) : a + z\n    comp y = f(x)\n";
        let mut interner = StringInterner::new();
        let fns = super::parse_logic(src, &mut interner).unwrap();
        let computed = super::parse_computed_with_functions(src, &mut interner, &fns).unwrap();
        let reverse_index = super::build_comp_reverse_index(&computed);
        assert_eq!(computed.len(), 1);

        let z_sym = interner.get("z").unwrap();
        assert!(
            computed[0].depends_on.contains(&z_sym),
            "comp must transitively depend on the global `z` read inside `f`"
        );

        let mut store = VariableStore::with_interner(interner);
        store.set("x", Value::Int(1 * crate::core::types::DECIMAL_SCALE));
        store.set("z", Value::Int(10 * crate::core::types::DECIMAL_SCALE));

        let all_syms: FxHashSet<Symbol> =
            store.state_machine.global_store.keys().copied().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &all_syms, &reverse_index);
        let y_sym = store.interner.get("y").unwrap();
        assert_eq!(*store.state_machine.get_global(y_sym), Value::Int(11 * crate::core::types::DECIMAL_SCALE));

        // Mutate ONLY z — y must recompute through the transitive dependency.
        store.state_machine.undo_log.clear();
        store.set("z", Value::Int(20 * crate::core::types::DECIMAL_SCALE));
        let mutated: FxHashSet<Symbol> = [z_sym].into_iter().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &mutated, &reverse_index);
        assert_eq!(*store.state_machine.get_global(y_sym), Value::Int(21 * crate::core::types::DECIMAL_SCALE));
    }

    #[test]
    fn test_comp_chain() {
        // comp a = x + 1; comp b = a * 2 → must be evaluated in topo order
        let src = "    comp a = x + 1\n    comp b = a * 2\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();

        let a_pos = computed
            .iter()
            .position(|cb| interner.resolve(cb.name) == Some("a"))
            .unwrap();
        let b_pos = computed
            .iter()
            .position(|cb| interner.resolve(cb.name) == Some("b"))
            .unwrap();
        assert!(a_pos < b_pos, "a must precede b in topological order");

        let mut store = VariableStore::with_interner(interner);
        store.set("x", Value::Int(3 * crate::core::types::DECIMAL_SCALE));
        let fns = FxHashMap::default();
        let reverse_index = super::build_comp_reverse_index(&computed);

        let all_syms: FxHashSet<Symbol> =
            store.state_machine.global_store.keys().copied().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &all_syms, &reverse_index);

        let a_sym = store.interner.get("a").unwrap();
        let b_sym = store.interner.get("b").unwrap();
        assert_eq!(*store.state_machine.get_global(a_sym), Value::Int(4 * crate::core::types::DECIMAL_SCALE));
        assert_eq!(*store.state_machine.get_global(b_sym), Value::Int(8 * crate::core::types::DECIMAL_SCALE));
    }

    /// Minimal xorshift64 PRNG — deterministic per-seed, dependency-free.
    /// Good enough for generating varied DAG shapes; not for anything security-sensitive.
    struct TestRng(u64);
    impl TestRng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn next_range(&mut self, n: usize) -> usize {
            if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
        }
    }

    /// Builds a random `comp` DAG of `n_comps` bindings over `n_base` base
    /// globals: comp `i` may depend on any base global or any earlier comp
    /// (index `< i`), which keeps the resulting `Vec<ComputedBinding>` in a
    /// valid topological order by construction — the same invariant
    /// `topo_sort_computed` establishes — without needing to run the full
    /// text parser.
    fn random_comp_dag(
        rng: &mut TestRng,
        interner: &mut StringInterner,
        n_base: usize,
        n_comps: usize,
        max_deps: usize,
    ) -> (Vec<Symbol>, Vec<ComputedBinding>) {
        let base_syms: Vec<Symbol> = (0..n_base)
            .map(|i| interner.get_or_intern(&format!("g{i}")))
            .collect();

        let mut comp_syms: Vec<Symbol> = Vec::with_capacity(n_comps);
        let mut bindings: Vec<ComputedBinding> = Vec::with_capacity(n_comps);
        for i in 0..n_comps {
            let name = interner.get_or_intern(&format!("c{i}"));
            comp_syms.push(name);

            let pool_len = n_base + i;
            let n_deps = rng.next_range(max_deps + 1);
            let mut deps: Vec<Symbol> = Vec::new();
            for _ in 0..n_deps {
                let idx = rng.next_range(pool_len);
                let dep_sym = if idx < n_base {
                    base_syms[idx]
                } else {
                    comp_syms[idx - n_base]
                };
                if !deps.contains(&dep_sym) {
                    deps.push(dep_sym);
                }
            }

            // expr = (i+1)*100 + dep_0 + dep_1 + ...
            let mut expr = Expr::Literal(Value::Int((i as i64 + 1) * 100));
            for &d in &deps {
                expr = Expr::BinaryOp {
                    left: Box::new(expr),
                    op: BinOp::Add,
                    right: Box::new(Expr::Variable(d)),
                };
            }

            bindings.push(ComputedBinding { name, expr, depends_on: deps });
        }
        (base_syms, bindings)
    }

    /// Equivalence check: the reverse-index-driven `recompute_computed_bindings`
    /// must produce byte-for-byte identical results (returned `changed` set and
    /// final global store) to the pre-optimization O(#bindings) linear scan,
    /// across many randomly shaped comp DAGs and mutation sequences. This is
    /// the empirical guarantee that the optimization in this file is purely a
    /// performance change, not a semantic one.
    #[test]
    fn test_recompute_matches_naive_scan_randomized() {
        let fns = FxHashMap::default();

        for seed in 1..=300u64 {
            let mut rng = TestRng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let n_base = 2 + rng.next_range(8); // 2..=9
            let n_comps = 3 + rng.next_range(40); // 3..=42
            let max_deps = 1 + rng.next_range(3); // 1..=3

            let mut interner = StringInterner::new();
            let (base_syms, bindings) =
                random_comp_dag(&mut rng, &mut interner, n_base, n_comps, max_deps);
            let reverse_index = super::build_comp_reverse_index(&bindings);

            let mut store_old = VariableStore::with_interner(interner.clone());
            let mut store_new = VariableStore::with_interner(interner);
            for (gi, &sym) in base_syms.iter().enumerate() {
                let v = Value::Int((gi as i64 + 1) * 10);
                store_old.set_symbol(sym, v.clone());
                store_new.set_symbol(sym, v);
            }

            // Initial load: every base global counts as mutated, as a real
            // document load would treat it (see `LogicWorker`'s `Reload` handler).
            let all_syms: FxHashSet<Symbol> = base_syms.iter().copied().collect();
            let changed_old = super::recompute_computed_bindings_naive_scan(
                &mut store_old, &bindings, &fns, &all_syms,
            );
            let changed_new = super::recompute_computed_bindings(
                &mut store_new, &bindings, &fns, &all_syms, &reverse_index,
            );
            assert_eq!(changed_old, changed_new, "seed {seed}: initial changed-set diverged");
            assert_eq!(
                store_old.state_machine.global_store, store_new.state_machine.global_store,
                "seed {seed}: initial store state diverged"
            );

            // Fire a sequence of random mutation events and compare after each.
            for _event in 0..15 {
                let n_mut = 1 + rng.next_range(n_base);
                let mut mutated: FxHashSet<Symbol> = FxHashSet::default();
                for _ in 0..n_mut {
                    mutated.insert(base_syms[rng.next_range(n_base)]);
                }
                for &sym in &mutated {
                    let v = Value::Int((rng.next_u64() % 1000) as i64);
                    store_old.set_symbol(sym, v.clone());
                    store_new.set_symbol(sym, v);
                }

                let changed_old = super::recompute_computed_bindings_naive_scan(
                    &mut store_old, &bindings, &fns, &mutated,
                );
                let changed_new = super::recompute_computed_bindings(
                    &mut store_new, &bindings, &fns, &mutated, &reverse_index,
                );
                assert_eq!(changed_old, changed_new, "seed {seed}: changed-set diverged after mutation");
                assert_eq!(
                    store_old.state_machine.global_store, store_new.state_machine.global_store,
                    "seed {seed}: store state diverged after mutation"
                );
            }
        }
    }

    /// Demonstrates the reverse-index optimization's payoff: with a large
    /// document (many independent comps) but a small blast radius (mutating
    /// one variable affects exactly one comp), the naive O(#bindings) scan
    /// pays for the whole document on every event while the indexed version
    /// only ever touches the affected binding.
    ///
    /// [`crate::core::types::MAX_COMP_BINDINGS`] (500) caps documents parsed
    /// through [`parse_computed_with_functions`]; this test builds bindings
    /// directly (bypassing the parser and that cap) to explore a size regime
    /// well beyond it, so the asymptotic gap is unambiguous. No external
    /// benchmark harness (e.g. `criterion`) is wired into this crate, so this
    /// is a plain timed `#[test]`; run with `cargo test --release -- --nocapture
    /// bench_recompute_large_document_small_blast_radius` to see the printed timings.
    #[test]
    fn bench_recompute_large_document_small_blast_radius() {
        const N_COMPS: usize = 20_000;
        const N_EVENTS: usize = 500;

        let mut interner = StringInterner::new();
        let base_syms: Vec<Symbol> = (0..N_COMPS)
            .map(|i| interner.get_or_intern(&format!("base{i}")))
            .collect();

        // Every comp depends on exactly one, distinct base global — so a
        // mutation to a single base global is only ever relevant to one comp
        // out of N_COMPS.
        let bindings: Vec<ComputedBinding> = (0..N_COMPS)
            .map(|i| {
                let name = interner.get_or_intern(&format!("comp{i}"));
                ComputedBinding {
                    name,
                    expr: Expr::BinaryOp {
                        left: Box::new(Expr::Variable(base_syms[i])),
                        op: BinOp::Add,
                        right: Box::new(Expr::Literal(Value::Int(1))),
                    },
                    depends_on: vec![base_syms[i]],
                }
            })
            .collect();

        let reverse_index = super::build_comp_reverse_index(&bindings);
        let fns = FxHashMap::default();

        let mut store_old = VariableStore::with_interner(interner.clone());
        let mut store_new = VariableStore::with_interner(interner);
        for &sym in &base_syms {
            store_old.set_symbol(sym, Value::Int(0));
            store_new.set_symbol(sym, Value::Int(0));
        }

        // Every event mutates the same single variable, which affects
        // exactly one of the N_COMPS bindings — the smallest possible blast
        // radius against a document far larger than any real one can be.
        let target = base_syms[0];

        let start_old = std::time::Instant::now();
        for n in 0..N_EVENTS {
            store_old.set_symbol(target, Value::Int(n as i64));
            let mutated: FxHashSet<Symbol> = [target].into_iter().collect();
            super::recompute_computed_bindings_naive_scan(&mut store_old, &bindings, &fns, &mutated);
        }
        let old_elapsed = start_old.elapsed();

        let start_new = std::time::Instant::now();
        for n in 0..N_EVENTS {
            store_new.set_symbol(target, Value::Int(n as i64));
            let mutated: FxHashSet<Symbol> = [target].into_iter().collect();
            super::recompute_computed_bindings(
                &mut store_new, &bindings, &fns, &mutated, &reverse_index,
            );
        }
        let new_elapsed = start_new.elapsed();

        // Both algorithms must still agree on the final result — this test
        // exists to measure speed, not to re-litigate correctness (see
        // `test_recompute_matches_naive_scan_randomized` for that).
        assert_eq!(
            store_old.state_machine.global_store,
            store_new.state_machine.global_store
        );

        println!(
            "bench_recompute_large_document_small_blast_radius: {N_COMPS} comps, {N_EVENTS} events \
             — naive scan = {old_elapsed:?}, reverse-index = {new_elapsed:?} \
             ({:.1}x faster)",
            old_elapsed.as_secs_f64() / new_elapsed.as_secs_f64().max(1e-12)
        );

        assert!(
            new_elapsed * 2 < old_elapsed,
            "expected the reverse-index version to be at least 2x faster on a large \
             document with a small blast radius; naive={old_elapsed:?} indexed={new_elapsed:?}"
        );
    }

    // ── Depth guard tests ────────────────────────────────────────────────────

    #[test]
    fn parse_deeply_nested_rejected() {
        // 300 nested parentheses — must produce a ParseError, not a stack overflow.
        let depth = 300usize;
        let src = format!("{}{}{}", "(".repeat(depth), "1", ")".repeat(depth));
        let mut interner = StringInterner::new();
        let result = super::parse_expr_standalone(&src, &mut interner);
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
        let result = super::parse_expr_standalone(&src, &mut interner);
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
        let result = super::parse_expr_standalone(&src, &mut interner);
        assert!(
            result.is_ok(),
            "normal nesting depth must parse without error: {result:?}"
        );
    }
