//! # Initial-load security gate: integration regression test
//!
//! `src/main.rs`'s initial-load path (`split_source` -> `parse_urls` ->
//! `parse_logic`/`parse_computed_with_functions`/`parse_root_timers` ->
//! `parse_style_with_variants` -> `parse_layout_with_urls` -> **Phase B
//! `check_types`** -> **F1 `check_information_flow`** -> `run_window_loop`)
//! used to skip the two load-time security checks entirely — only
//! `src/render/window/navigate.rs`'s in-app navigation path called them.
//! A `.mizu` document opened via `cargo run -- evil.mizu` (the primary
//! entry point this project's own `README.md` documents) would load and run
//! completely unchecked.
//!
//! This is deliberately an integration test that drives the *same sequence
//! of calls* `main.rs` makes, not a unit test of `check_types`/
//! `check_information_flow` in isolation — those are already covered
//! elsewhere and, by construction, cannot catch a *wiring* gap: they were
//! never wrong, they were simply never called on this path. Only a test
//! that exercises the actual pipeline can catch that class of bug.
//!
//! Confirmed failing before the `main.rs` fix landed (the F1 fixture below
//! loaded successfully with no rejection); confirmed passing after.

use std::collections::HashMap;
use std::path::Path;

use mizu::core::errors::MizuError;
use mizu::core::types::StringInterner;
use mizu::parser::flow::check_information_flow;
use mizu::parser::logic::{parse_computed_with_functions, parse_logic, parse_root_timers};
use mizu::parser::typecheck::check_types;
use mizu::parser::{parse_layout_with_urls, parse_style_with_variants, parse_urls, split_source};

/// Replicates `src/main.rs`'s `run()` parse pipeline exactly, up through the
/// two load-time security checks (everything before `run_window_loop`,
/// which needs a real window and can't run in a test process). Returns
/// `Err` if any phase — including the two security checks — rejects the
/// document, exactly as a real `cargo run -- <file>` invocation would.
fn load_document_like_main(source: &str) -> Result<(), MizuError> {
    let current_dir = Path::new(".");
    let parsed = split_source(source, current_dir)?;

    let mut interner = StringInterner::new();

    let url_registry = if !parsed.urls_block.trim().is_empty() {
        parse_urls(&parsed.urls_block, &mut interner)?
    } else {
        rustc_hash::FxHashMap::default()
    };

    let logic_fns = parse_logic(&parsed.logic_block, &mut interner)?;
    let mut computed_bindings = parse_computed_with_functions(
        &parsed.logic_block,
        &mut interner,
        &logic_fns,
        mizu_core::core::config::CONFIG.max_comp_bindings,
    )?;
    let root_timers = parse_root_timers(&parsed.logic_block, &mut interner)?;

    let (_style_rules, _style_variants) = parse_style_with_variants(&parsed.style_block)?;

    let dom_tree = parse_layout_with_urls(
        &parsed.layout_block,
        &mut interner,
        Some(&url_registry),
        true,
        &logic_fns,
    )?;

    check_types(
        &dom_tree,
        &root_timers,
        &logic_fns,
        &computed_bindings,
        &interner,
    )?;
    check_information_flow(
        &dom_tree,
        &root_timers,
        &logic_fns,
        &mut computed_bindings,
        &url_registry,
        &interner,
    )?;

    Ok(())
}

/// The exact F1 violation this test exists to catch: a `NetworkCall`
/// response (`data`, untrusted) flows straight into `Action::Navigate.url`
/// with no user-gesture gate (both timers fire without a click). Same
/// violation shape as `parser::flow`'s own unit fixture
/// (`network_var_into_navigate_rejected`), driven through the real
/// initial-load pipeline (this crate's own `load_document_like_main`, which
/// does *not* swallow `parse_urls`/`parse_logic` errors the way that unit
/// test's local helper does) instead of `check_information_flow` called
/// directly — so the `urls` block below must be syntactically valid per
/// `docs/reference/grammar.md` §3 (`api <alias> <path>`, path starting with
/// `/`), unlike that helper's fixture, whose stale `api: mizu://...` syntax
/// only "works" there because the parse error itself gets discarded.
const F1_VIOLATION_FIXTURE: &str = r#"
urls
    api endpoint /data
logic
    timer 1s -> GET(endpoint) -> data
    timer 2s -> navigate data
layout
    doc
"#;

#[test]
fn initial_load_rejects_ungated_network_data_into_navigate() {
    let result = load_document_like_main(F1_VIOLATION_FIXTURE);
    assert!(
        result.is_err(),
        "a document routing untrusted network data straight into `navigate` \
         with no user-gesture gate must be rejected on initial load, exactly \
         as it already is when reached by clicking a link (navigate.rs) — \
         got Ok(()) instead"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("navigate") && msg.contains("data"),
        "expected an information-flow error naming `navigate`/`data`, got: {msg}"
    );
}

#[test]
fn initial_load_still_accepts_every_reference_example() {
    let fixtures_dir = Path::new("docs/reference/examples");
    assert!(
        fixtures_dir.exists(),
        "fixtures directory not found; run from crate root"
    );

    let mut failures: HashMap<String, String> = HashMap::new();
    for entry in std::fs::read_dir(fixtures_dir).expect("read fixtures dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mizu") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // `err_*` fixtures are intentionally invalid at parse time (a
        // different, already-covered concern — see tests/reference_examples.rs);
        // this test is specifically about the two *new* load-time gates not
        // rejecting documents that were previously accepted.
        if name.starts_with("err_") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
        if let Err(e) = load_document_like_main(&source) {
            failures.insert(name, e.to_string());
        }
    }

    assert!(
        failures.is_empty(),
        "reference example(s) rejected by the initial-load security gate \
         (either a real bug in the example, or an overly strict checker — \
         fix the example, not the checker, unless the checker is wrong): {failures:#?}"
    );
}
