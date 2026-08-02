//! Tests for `interner.rs`: `StringInterner`/`FrozenInterner`.
//!
//! Freezing is a type-state transition (`StringInterner::freeze(self) ->
//! FrozenInterner`), not a runtime flag, so there is no "does the flag
//! survive Clone" behavior left to test — a frozen table simply cannot
//! intern further, at compile time. What remains worth pinning is the
//! property that transition exists to guarantee: a frozen table and every
//! clone of it agree on every `Symbol` ID (M1 — the UI thread and the logic
//! worker must never read different variables through the same expression
//! tree).

use super::*;

#[test]
fn frozen_interner_existing_symbols_unchanged() {
    let mut interner = StringInterner::new();
    let sym_a = interner.get_or_intern("alpha");
    let sym_b = interner.get_or_intern("beta");

    let interner = interner.freeze();

    // Existing symbols must still resolve to the same ID post-freeze.
    assert_eq!(interner.get("alpha"), Some(sym_a));
    assert_eq!(interner.get("beta"), Some(sym_b));
    assert_eq!(interner.resolve(sym_a), Some("alpha"));
}

/// M1: the logic worker's copy of the symbol table must agree with the UI
/// thread's on every `Symbol(u32)`, or the two threads read different
/// variables through the same expression tree.
#[test]
fn frozen_clone_resolves_every_symbol_identically() {
    let mut interner = StringInterner::new();
    let sym_x = interner.get_or_intern("x");
    let sym_y = interner.get_or_intern("y");
    let interner = interner.freeze();

    let cloned = interner.clone();

    assert_eq!(cloned.get("x"), Some(sym_x));
    assert_eq!(cloned.get("y"), Some(sym_y));
    assert_eq!(cloned.resolve(sym_x), Some("x"));
    assert_eq!(cloned.vec.len(), interner.vec.len());
}

/// Symbols are identical in the original and its frozen clone.
#[test]
fn interner_clone_symbols_are_identical() {
    let mut interner = StringInterner::new();
    let s_alpha = interner.get_or_intern("alpha");
    let s_beta = interner.get_or_intern("beta");
    let interner = interner.freeze();

    let clone = interner.clone();
    assert_eq!(clone.get("alpha"), Some(s_alpha));
    assert_eq!(clone.get("beta"), Some(s_beta));
    assert_eq!(clone.vec.len(), interner.vec.len());
}

/// Demonstrates the M1 fix end-to-end: after freeze, a clone used by the
/// worker thread cannot add symbols that would diverge from the UI thread.
/// Before the fix, the worker's clone was unfrozen and adding "runtime_var"
/// would produce Symbol(N) on the worker but a DIFFERENT Symbol(M) if the
/// UI thread independently interned the same name later.
#[test]
fn frozen_clone_cannot_diverge_symbol_ids() {
    let mut ui_interner = StringInterner::new();
    let sym_a = ui_interner.get_or_intern("declared_a");
    let sym_b = ui_interner.get_or_intern("declared_b");
    let ui_interner = ui_interner.freeze();

    let worker_interner = ui_interner.clone();

    // The worker resolves known symbols identically.
    assert_eq!(worker_interner.get("declared_a"), Some(sym_a));
    assert_eq!(worker_interner.get("declared_b"), Some(sym_b));

    // Worker-side VariableStore with the frozen clone.
    let mut worker_store = VariableStore::with_interner(worker_interner);

    // set_runtime does NOT intern "runtime_var".
    worker_store.set_runtime("runtime_var", Value::Decimal(7));
    assert!(worker_store.get("runtime_var").is_err());

    // Symbol table size on both sides is still identical.
    assert_eq!(
        worker_store.interner.vec.len(),
        ui_interner.vec.len(),
        "worker must not add symbols after freeze"
    );
}
