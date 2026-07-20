//! # RM-15 — storage rehydration taint: end-to-end regression test
//!
//! See `walkthrough.md`, section "RM-15 — storage rehydration taint", for the
//! full investigation. Summary: a document's local storage (`store_local`,
//! `src/core/storage.rs`) is genuinely write-only from the document's
//! perspective (invariant **S1**, `SECURITY-INVARIANTS.md`). This is not
//! merely a convention — it is a code-boundary fact, verified here three
//! ways against the real production primitives:
//!
//! 1. `StorageEngine`/`read_storage` really do persist and really can
//!    rehydrate a value across a simulated session boundary (the storage
//!    layer itself is not the mitigation — it works exactly as a "read your
//!    own writes back" primitive would).
//! 2. Nothing in the Mizu language surface can invoke that rehydration: no
//!    `read_local`/`load_local` builtin is recognised by the evaluator, so a
//!    document has no expression that could ever name a rehydrated value.
//! 3. Because of (2), the load-time flow checker (`src/parser/flow.rs`) is
//!    never asked to classify a rehydrated value — its source set is (and
//!    only needs to be) `$form` fields and `NetworkCall` targets. This test
//!    also pins down *what would go wrong* if a `read_local` builtin were
//!    ever added without also teaching `flow.rs` to treat it as a source
//!    (exactly the follow-up `SECURITY-INVARIANTS.md` §3 already flags),
//!    using the real checker rather than a hypothetical description.

use std::collections::HashMap;

use mizu::core::storage::{ValidatedDomain, StorageEngine, read_storage};
use mizu::core::types::{StringInterner, Symbol, Value, StateMachine};
use mizu::parser::flow::check_information_flow;
use mizu::parser::layout::parse_layout_with_urls;
use mizu::parser::logic::{parse_computed_with_functions, parse_logic, parse_root_timers, Expr};
use mizu::parser::splitter::split_source_with_origin;
use mizu::parser::urls::parse_urls;
use mizu::parser::Origin;

/// Points `StorageEngine::open`/`read_storage` at an isolated temp directory
/// and supplies a headless master key, so this test never touches the real
/// user storage directory or OS keyring. `std::env::set_var` is `unsafe` as
/// of the 2024 edition (thread-safety of process-wide env mutation); this
/// crate's own `#![forbid(unsafe_code)]` applies only to the `mizu` library,
/// not to this separate integration-test binary, and every write here
/// happens before any other thread in this single-threaded test process
/// could read it.
fn isolate_storage_env() -> std::path::PathBuf {
    let tmp = std::env::temp_dir().join(format!(
        "mizu_rm15_storage_rehydration_test_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create isolated storage dir");
    unsafe {
        std::env::set_var("APPDATA", &tmp);
        std::env::set_var("XDG_DATA_HOME", &tmp);
        std::env::set_var("MIZU_MASTER_KEY", "11".repeat(32));
    }
    tmp
}

/// Parses and flow-checks a full `.mizu` document, mirroring
/// `parser::flow`'s own internal `check_flow_doc` test helper (duplicated
/// here since that helper is private to `flow.rs`'s unit tests and this file
/// is a separate integration-test crate).
fn check_flow_doc(src: &str) -> Result<(usize, usize, usize), mizu::core::errors::MizuError> {
    let current_dir = std::env::current_dir().unwrap_or_default();
    let blocks = split_source_with_origin(src, &current_dir, Origin::Network)
        .expect("split_source_with_origin");
    let mut interner = StringInterner::new();
    let urls = parse_urls(&blocks.urls_block, &mut interner).unwrap_or_default();
    let functions = parse_logic(&blocks.logic_block, &mut interner).unwrap_or_default();
    let comps = parse_computed_with_functions(&blocks.logic_block, &mut interner, &functions)
        .unwrap_or_default();
    let timers = parse_root_timers(&blocks.logic_block, &mut interner).unwrap_or_default();
    let dom = parse_layout_with_urls(&blocks.layout_block, &mut interner, Some(&urls), true)
        .expect("parse_layout_with_urls");

    check_information_flow(&dom, &timers, &functions, &comps, &urls, &interner)
}

#[test]
fn storage_rehydration_taint_end_to_end() {
    let _tmp = isolate_storage_env();
    let domain = ValidatedDomain::from_raw("rm15-storage-rehydration-test.mizu");

    // ── "Session 1": a document writes a value derived from $form (i.e.
    // tainted, in flow.rs's model) to local storage via store_local. This
    // uses the same primitive `execute_capability_action`'s `StoreLocal`
    // handler (`src/render/security.rs`) ultimately calls into
    // (`StorageEngine::write_batch`, via `StoragePool::write_record`).
    let tainted_value = Value::from("payload-derived-from-$form-field");
    {
        let engine = StorageEngine::open(&domain).expect("open storage engine for session 1");
        engine
            .write_batch(std::iter::once(("saved", &tainted_value)))
            .expect("write_batch (store_local persistence)");
    }

    // ── "Session 2" (reload): reread everything previously persisted for
    // this domain, exactly as `StorageEngine::read_all`/`read_storage` do.
    // This confirms the storage layer really does rehydrate data across a
    // session boundary — the write-only guarantee is not because the data
    // layer forgets, but because nothing wires this back into the evaluator.
    let rehydrated: HashMap<String, Value> =
        read_storage(&domain).expect("read_storage (session 2 rehydration)");
    assert_eq!(
        rehydrated.get("saved"),
        Some(&tainted_value),
        "the storage layer must genuinely round-trip the tainted value \
         across the simulated session boundary — otherwise this test would \
         not be exercising the real risk at all"
    );

    // ── The security question: does anything let a document *observe*
    // `rehydrated`? Answer: no such expression exists in the language.
    // `read_local`/`load_local` is neither a built-in (see
    // `SIDE_EFFECT_BUILTINS` and the `FunctionCall` match arms in
    // `core::types::StateMachine::evaluate_impl`) nor could it resolve to a
    // user function (documents never declare one, since the parser has no
    // syntax that would bind storage to a name). Constructing the call
    // directly as an `Expr` (bypassing the text parser entirely, so this
    // isn't just "the parser doesn't have syntax for it" but "the evaluator
    // itself has no such capability") and evaluating it must fail exactly
    // like calling any other undeclared function.
    let mut interner = StringInterner::new();
    let read_local_sym: Symbol = interner.get_or_intern("read_local");
    let key_arg = Expr::Literal(Value::from("saved"));
    let call = Expr::FunctionCall {
        name: read_local_sym,
        args: vec![key_arg],
    };
    let mut machine = StateMachine::new();
    let no_functions = Default::default();
    let result = machine.evaluate(&call, 0, &no_functions, &interner);
    assert!(
        result.is_err(),
        "a document must not be able to name any function that reads storage back"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("undefined function"),
        "expected an undefined-function error (no read primitive exists at all), got: {msg}"
    );

    // ── What the flow checker would (and would not) catch if such a
    // primitive were ever added. A document declaring a plain `comp` whose
    // literal RHS stands in for a hypothetical rehydrated value is judged
    // CLEAN today and is allowed to reach `navigate` unconditionally —
    // because flow.rs's source set (see `check_information_flow`'s
    // "Initialize tainted sources" block) is exactly `$form` fields and
    // `NetworkCall` targets, nothing else. This is correct and safe *today*
    // only because (per the two checks above) nothing can ever populate such
    // a `comp` from storage. It is exactly the scenario
    // `SECURITY-INVARIANTS.md`'s S1 note anticipates: "if `read_local` is
    // ever added, it must be declared as a taint source in invariant F1 and
    // route through the load-time flow checker" — this assertion is what
    // would need to flip to `is_err()` on that day.
    let doc = r#"
logic
    comp saved = "payload-derived-from-$form-field"
    timer 1s -> navigate saved
layout
    window
        "#;
    let res = check_flow_doc(doc);
    assert!(
        res.is_ok(),
        "a plain literal-valued global is (correctly, today) untainted by \
         the checker — pinning down the precondition that would break if \
         storage rehydration were ever wired up without a matching flow.rs fix"
    );
}
