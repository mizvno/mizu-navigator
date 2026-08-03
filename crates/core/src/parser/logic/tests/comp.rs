//! Tests for `comp.rs`: the `comp` keyword's parsing, cycle detection,
//! binding cap, and dependency-driven recomputation (including the
//! reverse-index optimization's equivalence with a naive linear scan).

use super::*;

#[test]
fn test_comp_cycle_rejected() {
    let src = "    comp a = b + 1\n    comp b = a + 1\n";
    let mut interner = StringInterner::new();
    let result = super::super::parse_computed(src, &mut interner);
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
    let too_many = crate::core::config::CONFIG.max_comp_bindings + 1;
    let mut src = String::new();
    for i in 0..too_many {
        src.push_str(&format!("    comp c{i} = {i}\n"));
    }
    let mut interner = StringInterner::new();
    let result = super::super::parse_computed(&src, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("MAX_COMP_BINDINGS")),
        "expected a ParseError naming MAX_COMP_BINDINGS, got: {result:?}"
    );
}

#[test]
fn test_comp_binding_cap_allows_exactly_the_limit() {
    // The cap must reject documents *above* the limit without rejecting
    // documents that declare exactly MAX_COMP_BINDINGS comps.
    let at_limit = crate::core::config::CONFIG.max_comp_bindings;
    let mut src = String::new();
    for i in 0..at_limit {
        src.push_str(&format!("    comp c{i} = {i}\n"));
    }
    let mut interner = StringInterner::new();
    let result = super::super::parse_computed(&src, &mut interner);
    assert!(
        result.is_ok(),
        "expected Ok at exactly the cap, got: {result:?}"
    );
    assert_eq!(result.unwrap().len(), at_limit);
}

#[test]
fn test_comp_assignment_rejected() {
    let src = "    comp derived = 42\n";
    let mut interner = StringInterner::new();
    let computed = super::super::parse_computed(src, &mut interner).unwrap();
    assert_eq!(computed.len(), 1);

    let mut store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    };
    let derived_sym = store.interner.get_or_intern("derived");
    let mut store = store.freeze();
    store.evaluator.computed_var_syms.insert(derived_sym);

    let fns = FxHashMap::default();
    let mut arena = ExprArena::new();
    let root = arena.alloc(Expr::Literal(Value::Decimal(99)));
    let action = Action::Assign {
        target: "derived".to_string(),
        expr: crate::parser::logic::ExprTree { arena, root },
    };
    let result = super::super::execute_action(&action, &mut store, &fns);
    assert!(
        matches!(result, Err(MizuError::ExecutionError(ref msg)) if msg.contains("computed variable")),
        "expected ExecutionError for comp assignment, got: {result:?}"
    );
}

#[test]
fn test_comp_initial_value() {
    let src = "    comp derived = total + 1\n";
    let mut interner = StringInterner::new();
    let computed = super::super::parse_computed(src, &mut interner).unwrap();
    let reverse_index = super::super::build_comp_reverse_index(&computed);

    let mut store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    };
    store.set(
        "total",
        Value::Decimal(5 * crate::core::types::DECIMAL_SCALE),
    );
    let mut store = store.freeze();

    let fns = FxHashMap::default();
    let all_syms: FxHashSet<Symbol> = store.evaluator.global_store.keys().copied().collect();
    super::super::recompute_computed_bindings(
        &mut store,
        &computed,
        &fns,
        &all_syms,
        &reverse_index,
    );

    let derived_sym = store.interner.get("derived").unwrap();
    assert_eq!(
        *store.evaluator.get_global(derived_sym),
        Value::Decimal(6 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn test_comp_evaluated_on_dependency_change() {
    let src = "    comp double = x * 2\n";
    let mut interner = StringInterner::new();
    let computed = super::super::parse_computed(src, &mut interner).unwrap();
    let reverse_index = super::super::build_comp_reverse_index(&computed);

    let mut store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    };
    store.set("x", Value::Decimal(10 * crate::core::types::DECIMAL_SCALE));
    let mut store = store.freeze();
    let fns = FxHashMap::default();

    let all_syms: FxHashSet<Symbol> = store.evaluator.global_store.keys().copied().collect();
    super::super::recompute_computed_bindings(
        &mut store,
        &computed,
        &fns,
        &all_syms,
        &reverse_index,
    );
    let double_sym = store.interner.get("double").unwrap();
    assert_eq!(
        *store.evaluator.get_global(double_sym),
        Value::Decimal(20 * crate::core::types::DECIMAL_SCALE)
    );

    // Mutate x and recompute
    store.evaluator.undo_log.clear();
    let x_sym = store.interner.get("x").unwrap();
    store
        .evaluator
        .set_global(x_sym, Value::Decimal(7 * crate::core::types::DECIMAL_SCALE));
    let x_sym = store.interner.get("x").unwrap();
    let mutated: FxHashSet<Symbol> = [x_sym].into_iter().collect();
    super::super::recompute_computed_bindings(
        &mut store,
        &computed,
        &fns,
        &mutated,
        &reverse_index,
    );
    assert_eq!(
        *store.evaluator.get_global(double_sym),
        Value::Decimal(14 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn test_comp_depends_on_globals_read_inside_functions() {
    // `f` reads the global `z` internally; `comp y = f(x)` must therefore
    // recompute when `z` mutates, not only when `x` does.  Pre-regression,
    // the dependency walk stopped at the comp RHS and `y` went stale.
    let src = "    f(a: num) : a + z\n    comp y = f(x)\n";
    let mut interner = StringInterner::new();
    let fns = super::super::parse_logic(src, &mut interner).unwrap();
    let computed =
        super::super::parse_computed_with_functions(src, &mut interner, &fns, 500).unwrap();
    let reverse_index = super::super::build_comp_reverse_index(&computed);
    assert_eq!(computed.len(), 1);

    let z_sym = interner.get("z").unwrap();
    assert!(
        computed[0].depends_on.contains(&z_sym),
        "comp must transitively depend on the global `z` read inside `f`"
    );

    let mut store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    };
    store.set("x", Value::Decimal(1 * crate::core::types::DECIMAL_SCALE));
    store.set("z", Value::Decimal(10 * crate::core::types::DECIMAL_SCALE));
    let mut store = store.freeze();

    let all_syms: FxHashSet<Symbol> = store.evaluator.global_store.keys().copied().collect();
    super::super::recompute_computed_bindings(
        &mut store,
        &computed,
        &fns,
        &all_syms,
        &reverse_index,
    );
    let y_sym = store.interner.get("y").unwrap();
    assert_eq!(
        *store.evaluator.get_global(y_sym),
        Value::Decimal(11 * crate::core::types::DECIMAL_SCALE)
    );

    // Mutate ONLY z — y must recompute through the transitive dependency.
    store.evaluator.undo_log.clear();
    let z_sym_again = store.interner.get("z").unwrap();
    store.evaluator.set_global(
        z_sym_again,
        Value::Decimal(20 * crate::core::types::DECIMAL_SCALE),
    );
    let mutated: FxHashSet<Symbol> = [z_sym].into_iter().collect();
    super::super::recompute_computed_bindings(
        &mut store,
        &computed,
        &fns,
        &mutated,
        &reverse_index,
    );
    assert_eq!(
        *store.evaluator.get_global(y_sym),
        Value::Decimal(21 * crate::core::types::DECIMAL_SCALE)
    );
}

#[test]
fn test_comp_chain() {
    // comp a = x + 1; comp b = a * 2 → must be evaluated in topo order
    let src = "    comp a = x + 1\n    comp b = a * 2\n";
    let mut interner = StringInterner::new();
    let computed = super::super::parse_computed(src, &mut interner).unwrap();

    let a_pos = computed
        .iter()
        .position(|cb| interner.resolve(cb.name) == Some("a"))
        .unwrap();
    let b_pos = computed
        .iter()
        .position(|cb| interner.resolve(cb.name) == Some("b"))
        .unwrap();
    assert!(a_pos < b_pos, "a must precede b in topological order");

    let mut store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    };
    store.set("x", Value::Decimal(3 * crate::core::types::DECIMAL_SCALE));
    let mut store = store.freeze();
    let fns = FxHashMap::default();
    let reverse_index = super::super::build_comp_reverse_index(&computed);

    let all_syms: FxHashSet<Symbol> = store.evaluator.global_store.keys().copied().collect();
    super::super::recompute_computed_bindings(
        &mut store,
        &computed,
        &fns,
        &all_syms,
        &reverse_index,
    );

    let a_sym = store.interner.get("a").unwrap();
    let b_sym = store.interner.get("b").unwrap();
    assert_eq!(
        *store.evaluator.get_global(a_sym),
        Value::Decimal(4 * crate::core::types::DECIMAL_SCALE)
    );
    assert_eq!(
        *store.evaluator.get_global(b_sym),
        Value::Decimal(8 * crate::core::types::DECIMAL_SCALE)
    );
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
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
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
        let mut arena = ExprArena::new();
        let mut expr = Expr::Literal(Value::Decimal((i as i64 + 1) * 100));
        for &d in &deps {
            let left = arena.alloc(expr);
            let right = arena.alloc(Expr::Variable(d));
            expr = Expr::BinaryOp {
                left,
                op: BinOp::Add,
                right,
            };
        }
        let root = arena.alloc(expr);

        bindings.push(ComputedBinding {
            name,
            expr: crate::parser::logic::ExprTree { arena, root },
            depends_on: deps,
        });
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
        let reverse_index = super::super::build_comp_reverse_index(&bindings);

        let interner = interner.freeze();
        let mut store_old = VariableStore::with_interner(interner.clone());
        let mut store_new = VariableStore::with_interner(interner.clone());
        for (gi, &sym) in base_syms.iter().enumerate() {
            let v = Value::Decimal((gi as i64 + 1) * 10);
            store_old.evaluator.set_global(sym, v.clone());
            store_new.evaluator.set_global(sym, v);
        }

        // Initial load: every base global counts as mutated, as a real
        // document load would treat it (see `LogicWorker`'s `Reload` handler).
        let all_syms: FxHashSet<Symbol> = base_syms.iter().copied().collect();
        let changed_old = super::super::recompute_computed_bindings_naive_scan(
            &mut store_old,
            &bindings,
            &fns,
            &all_syms,
        );
        let changed_new = super::super::recompute_computed_bindings(
            &mut store_new,
            &bindings,
            &fns,
            &all_syms,
            &reverse_index,
        );
        assert_eq!(
            changed_old, changed_new,
            "seed {seed}: initial changed-set diverged"
        );
        assert_eq!(
            store_old.evaluator.global_store, store_new.evaluator.global_store,
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
                let v = Value::Decimal((rng.next_u64() % 1000) as i64);
                store_old.evaluator.set_global(sym, v.clone());
                store_new.evaluator.set_global(sym, v);
            }

            let changed_old = super::super::recompute_computed_bindings_naive_scan(
                &mut store_old,
                &bindings,
                &fns,
                &mutated,
            );
            let changed_new = super::super::recompute_computed_bindings(
                &mut store_new,
                &bindings,
                &fns,
                &mutated,
                &reverse_index,
            );
            assert_eq!(
                changed_old, changed_new,
                "seed {seed}: changed-set diverged after mutation"
            );
            assert_eq!(
                store_old.evaluator.global_store, store_new.evaluator.global_store,
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
/// [`crate::core::config::CONFIG.max_comp_bindings`] (500) caps documents parsed
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
            let mut arena = ExprArena::new();
            let left = arena.alloc(Expr::Variable(base_syms[i]));
            let right = arena.alloc(Expr::Literal(Value::Decimal(1)));
            let root = arena.alloc(Expr::BinaryOp {
                left,
                op: BinOp::Add,
                right,
            });
            ComputedBinding {
                name,
                expr: crate::parser::logic::ExprTree { arena, root },
                depends_on: vec![base_syms[i]],
            }
        })
        .collect();

    let reverse_index = super::super::build_comp_reverse_index(&bindings);
    let fns = FxHashMap::default();

    let interner = interner.freeze();
    let mut store_old = VariableStore::with_interner(interner.clone());
    let mut store_new = VariableStore::with_interner(interner.clone());
    for &sym in &base_syms {
        store_old.evaluator.set_global(sym, Value::Decimal(0));
        store_new.evaluator.set_global(sym, Value::Decimal(0));
    }

    // Every event mutates the same single variable, which affects
    // exactly one of the N_COMPS bindings — the smallest possible blast
    // radius against a document far larger than any real one can be.
    let target = base_syms[0];

    let start_old = std::time::Instant::now();
    for n in 0..N_EVENTS {
        store_old
            .evaluator
            .set_global(target, Value::Decimal(n as i64));
        let mutated: FxHashSet<Symbol> = [target].into_iter().collect();
        super::super::recompute_computed_bindings_naive_scan(
            &mut store_old,
            &bindings,
            &fns,
            &mutated,
        );
    }
    let old_elapsed = start_old.elapsed();

    let start_new = std::time::Instant::now();
    for n in 0..N_EVENTS {
        store_new
            .evaluator
            .set_global(target, Value::Decimal(n as i64));
        let mutated: FxHashSet<Symbol> = [target].into_iter().collect();
        super::super::recompute_computed_bindings(
            &mut store_new,
            &bindings,
            &fns,
            &mutated,
            &reverse_index,
        );
    }
    let new_elapsed = start_new.elapsed();

    // Both algorithms must still agree on the final result — this test
    // exists to measure speed, not to re-litigate correctness (see
    // `test_recompute_matches_naive_scan_randomized` for that).
    assert_eq!(
        store_old.evaluator.global_store,
        store_new.evaluator.global_store
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

#[test]
fn comp_cascade_shares_one_instruction_budget() {
    // The budget is per *event*, not per binding. Before this, every firing
    // comp got a fresh allowance, so the real worst case was
    // MAX_COMP_BINDINGS (500) full budgets per event — a product the document
    // controlled by declaring more derived variables, spent on the single
    // LogicWorker thread every tab shares.
    //
    // Asserted on an observable that actually separates the two designs: with
    // one shared budget a long chain is *cut short*, so the tail never
    // updates. Per-binding budgets would run the whole chain to completion,
    // because no individual link is expensive enough to trip its own quota.
    // (Asserting only on `instruction_count` does not work — with per-binding
    // resets it reads back as the last link's cost, which is tiny either way.)
    let n_comps = 40;
    let mut src = String::from(
        "    comp c0 = base + 1
",
    );
    for i in 1..n_comps {
        src.push_str(&format!(
            "    comp c{i} = c{} + 1
",
            i - 1
        ));
    }

    let mut interner = StringInterner::new();
    let base = interner.get_or_intern("base");
    let last = interner.get_or_intern(&format!("c{}", n_comps - 1));
    let bindings = super::super::parse_computed(&src, &mut interner).expect("comps parse");
    assert_eq!(bindings.len(), n_comps, "every comp should have parsed");
    let reverse_index = super::super::build_comp_reverse_index(&bindings);

    let interner = interner.freeze();
    let mut store = VariableStore::with_interner(interner);
    store.evaluator.set_global(base, Value::Decimal(0));

    // Enough for a handful of links, nowhere near all forty.
    let budget = 20;
    store.evaluator.max_instructions = budget;

    let mutated: FxHashSet<Symbol> = std::iter::once(base).collect();
    let fns = FxHashMap::default();
    let changed = super::super::recompute_computed_bindings(
        &mut store,
        &bindings,
        &fns,
        &mutated,
        &reverse_index,
    );

    assert!(
        !changed.contains(&last),
        "the whole {n_comps}-link cascade completed under a budget of {budget}, so each          binding is being granted its own allowance instead of sharing one per event"
    );
    assert!(
        changed.len() < n_comps,
        "expected the cascade to be cut short, but {} of {n_comps} bindings updated",
        changed.len().saturating_sub(1)
    );
    assert!(
        store.evaluator.instruction_count <= budget + 1,
        "spent {} instructions against a budget of {budget}",
        store.evaluator.instruction_count
    );
}
