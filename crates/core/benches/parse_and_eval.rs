//! Benchmarks for the mizu-core parse and evaluation paths.
//!
//! Added per the standing "measure, don't assume" discipline this repo
//! follows for optimization work (see `PROMPT-followups-interner-clone-
//! and-ast-arena.md`): the zero-copy lexer, the `Box<Expr>` -> `ExprArena`
//! migration, and the `FunctionCall` args-pool flattening were each
//! justified by `size_of`/allocation-count micro-measurements, but until
//! now nothing measured whether parsing or evaluating an actual `.mizu`
//! document got faster in wall-clock terms. This is that measurement.
//!
//! Uses `docs/reference/examples/showcase.mizu` (164 lines, the largest
//! shipped example) rather than a synthetically inflated fixture —
//! criterion's statistical model (many iterations, outlier rejection)
//! measures sub-millisecond operations reliably without needing an
//! artificially large input to pad the numbers.
//!
//! Run with `cargo bench -p mizu-core` from the workspace root, or
//! `cargo bench` from `crates/core`.
//!
//! ## Baseline (Windows, release build, first measurement — see git log for date)
//!
//! - `full_parse_showcase_mizu`: ~52 µs (median of 100 samples)
//! - `eval_clamp_call_showcase_mizu`: ~458 ns (median of 100 samples)
//!
//! Both are already far below any frame budget this renderer cares about
//! (16ms at 60fps) for a document at the upper end of what a real `.mizu`
//! file looks like today. Nothing here currently suggests parse/eval speed
//! is a bottleneck worth further arena/allocator work — the prior rounds of
//! that work were justified by allocation-count reduction on their own
//! terms, not by a wall-clock number, and this baseline is what the next
//! round should be measured against instead of another `size_of`-only
//! argument.

use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};

use mizu_core::core::types::{StringInterner, Value, VariableStore};
use mizu_core::parser::logic::{
    evaluate, parse_computed_with_functions, parse_expr_standalone, parse_logic,
    parse_root_timers,
};
use mizu_core::parser::{parse_layout_with_urls, parse_style_with_variants, parse_urls, split_source};

/// Relative to `crates/core` (this benchmark's own crate root, which is
/// `cargo bench`'s working directory).
const SHOWCASE_PATH: &str = "../../docs/reference/examples/showcase.mizu";

fn read_showcase() -> String {
    std::fs::read_to_string(SHOWCASE_PATH)
        .unwrap_or_else(|e| panic!("read {SHOWCASE_PATH}: {e} (run from the workspace or crates/core)"))
}

/// The full document compile pipeline `src/main.rs` runs on every initial
/// load: split -> urls -> logic -> computed -> root timers -> style ->
/// layout. Does not include the two load-time security checks
/// (`check_types`/`check_information_flow`) or window/renderer setup, which
/// depend on the GUI stack this dependency-light crate deliberately doesn't
/// pull in.
fn bench_full_parse(c: &mut Criterion) {
    let source = read_showcase();
    let current_dir = Path::new(".");

    c.bench_function("full_parse_showcase_mizu", |b| {
        b.iter(|| {
            let parsed = split_source(&source, current_dir).unwrap();
            let mut interner = StringInterner::new();
            let url_registry = parse_urls(&parsed.urls_block, &mut interner).unwrap();
            let logic_fns = parse_logic(&parsed.logic_block, &mut interner).unwrap();
            let computed =
                parse_computed_with_functions(&parsed.logic_block, &mut interner, &logic_fns).unwrap();
            let timers = parse_root_timers(&parsed.logic_block, &mut interner).unwrap();
            let (style_rules, style_variants) = parse_style_with_variants(&parsed.style_block).unwrap();
            let dom = parse_layout_with_urls(
                &parsed.layout_block,
                &mut interner,
                Some(&url_registry),
                true,
                &logic_fns,
            )
            .unwrap();
            std::hint::black_box((dom, style_rules, style_variants, computed, timers));
        });
    });
}

/// Evaluates a call to `clamp(x, lo, hi)` — showcase.mizu's representative
/// multi-line user function (an intermediate `limited = ...` binding, an
/// `if/then/else`, three parameters) — via the same `evaluate()` entry
/// point every `comp`/timer/action recomputation in the running app goes
/// through (`core::types::eval::StateMachine::evaluate`, via the
/// `parser::logic::evaluate` wrapper).
fn bench_function_call_eval(c: &mut Criterion) {
    let source = read_showcase();
    let current_dir = Path::new(".");
    let parsed = split_source(&source, current_dir).unwrap();
    let mut interner = StringInterner::new();
    let logic_fns = parse_logic(&parsed.logic_block, &mut interner).unwrap();

    let call = parse_expr_standalone("clamp(counter, 0, 100)", &mut interner).unwrap();
    let mut store = VariableStore::with_interner(interner);
    store.set("counter", Value::from(42i64));

    c.bench_function("eval_clamp_call_showcase_mizu", |b| {
        b.iter(|| {
            let result = evaluate(call.root(), &call.arena, &mut store, &logic_fns, 0).unwrap();
            std::hint::black_box(result);
        });
    });
}

criterion_group!(benches, bench_full_parse, bench_function_call_eval);
criterion_main!(benches);
