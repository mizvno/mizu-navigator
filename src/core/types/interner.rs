//! `Symbol` and `StringInterner`.

use std::collections::HashMap;

/// A Symbol represents a unique identifier mapped from a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Symbol(pub u32);

/// Map strings to symbols and resolve symbols back to strings.
///
/// ## Concurrency model
///
/// The UI thread and the logic worker thread each own an independent
/// **clone** of a `StringInterner` — there is no shared/locked table, so
/// there is no lock contention on this cold path (interning happens at
/// parse/reload time; see the module-level architecture note below). The two
/// clones are guaranteed to agree on every `Symbol(u32)` ID *at the moment of
/// the clone* because [`Clone`] preserves the source's contents exactly.
/// [`freeze`](Self::freeze) is the boundary that keeps them agreeing
/// afterward: once frozen, a thread must not mint symbols for names the
/// other clone has never seen, because the other clone has no way to learn
/// about them and the two tables would silently diverge (`Symbol(7)` meaning
/// one string on one thread and a different string, or nothing, on the
/// other).
///
/// After the initial parse phase the interner **must** be
/// [`freeze`](Self::freeze)d. Post-freeze code that may encounter strings
/// not declared in the logic block (form field names, network response
/// variable names) must use [`VariableStore::set_runtime`] instead of
/// [`VariableStore::set`]. `set_runtime` calls [`get`](Self::get) rather than
/// [`get_or_intern`](Self::get_or_intern) and silently discards unknown
/// names, so the frozen symbol table is never mutated by untrusted runtime
/// content — this is also the load-bearing defence against a document (or a
/// malicious network response / form) growing the symbol table unboundedly.
///
/// [`get_or_intern`](Self::get_or_intern) itself remains a plain,
/// **thread-local** insert-or-lookup: calling it post-freeze with a genuinely
/// new name does intern it (there is no dummy/sentinel `Symbol` — every
/// `Symbol` this type ever returns is real and resolvable), but only in
/// *this* thread's own copy, and it logs a `tracing::warn!` because it means
/// a caller produced a `Symbol` that has no defined meaning on the other
/// thread's clone. The architectural rule that prevents this from happening
/// in practice is: never send a freshly-`get_or_intern`-ed post-freeze
/// `Symbol` across the UI↔worker channel. Cross-thread messages instead
/// carry the resolved `String` name (see [`crate::network::UiEvent::UpdateVariable`])
/// and the receiving thread resolves it against its *own* frozen table via
/// `set_runtime`/`get` — the two clones never need to invent a shared ID for
/// the same runtime string, because neither side is trusted to mint one on
/// the other's behalf.
///
/// Cloning a frozen interner produces a **frozen** copy.
#[derive(Debug, Default)]
pub struct StringInterner {
    /// Name → `Symbol` lookup, the inverse of `vec`.
    pub map: HashMap<String, Symbol>,
    /// `Symbol(i)` resolves to `vec[i]`; append-only.
    pub vec: Vec<String>,
    /// Once `true`, further insertions via `get_or_intern` are a logged caller
    /// bug (see [`Self::freeze`]).
    pub frozen: bool,
}

impl Clone for StringInterner {
    fn clone(&self) -> Self {
        // Preserve the `frozen` flag so the logic worker's copy of the
        // interner is also frozen.  The worker must never add new symbols at
        // runtime; doing so would create Symbol(u32) IDs that diverge between
        // the UI thread (whose interner never sees those new symbols) and the
        // worker, making every cross-thread symbol lookup unreliable.
        //
        // Callers that run after freeze and might encounter runtime-generated
        // strings must use VariableStore::set_runtime, which consults
        // interner.get() (not get_or_intern()) and discards unknown names.
        Self {
            map: self.map.clone(),
            vec: self.vec.clone(),
            frozen: self.frozen,
        }
    }
}

impl StringInterner {
    /// Creates a new empty interner.
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            vec: Vec::new(),
            frozen: false,
        }
    }

    /// Marks the interner as frozen.
    ///
    /// After this call, [`get_or_intern`](Self::get_or_intern) still works
    /// correctly (it never returns a bogus/unresolvable `Symbol`) but logs a
    /// `tracing::warn!` when asked to mint a symbol for a name it hasn't
    /// seen before — that symbol is only valid on *this* thread's clone of
    /// the table, so producing one post-freeze is a caller bug (see the
    /// type-level docs for the architectural rule that avoids this).
    ///
    /// Cloning a frozen interner produces a frozen copy (see [`Clone`]).
    pub fn freeze(&mut self) {
        self.frozen = true;
    }

    /// Interns `s` and returns its [`Symbol`], inserting it into this
    /// interner's own table if it is not already present.
    ///
    /// Every `Symbol` this method returns is real and resolvable via
    /// [`resolve`](Self::resolve) — there is no sentinel/dummy value. If the
    /// interner is frozen and `s` is not yet present, the insert still
    /// happens (so the caller always gets a working `Symbol` back), but a
    /// `tracing::warn!` is emitted: a `Symbol` minted here has no defined
    /// meaning on any *other* thread's clone of this table, so this should
    /// only ever happen for names local to this thread. Cross-thread code
    /// that may encounter runtime-generated strings must use
    /// [`get`](Self::get) or [`VariableStore::set_runtime`] instead.
    pub fn get_or_intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        if self.frozen {
            tracing::warn!(
                name = s,
                "StringInterner is frozen: interning '{}' post-freeze — the resulting \
                 Symbol is only valid on this thread's copy of the table and has no \
                 defined meaning elsewhere; cross-thread code must resolve names via \
                 set_runtime/get against its own frozen table instead of minting a new \
                 Symbol to send across threads",
                s
            );
        }
        let id = self.vec.len() as u32;
        let sym = Symbol(id);
        self.map.insert(s.to_string(), sym);
        self.vec.push(s.to_string());
        sym
    }

    /// Returns the [`Symbol`] for `s` if it was interned, or `None`.
    ///
    /// Unlike [`get_or_intern`](Self::get_or_intern) this method is
    /// **read-only**: it never adds new symbols and is safe to call on a frozen
    /// interner with arbitrary runtime strings.
    pub fn get(&self, s: &str) -> Option<Symbol> {
        self.map.get(s).copied()
    }

    /// Resolves a Symbol back to its string representation.
    pub fn resolve(&self, sym: Symbol) -> Option<&str> {
        self.vec.get(sym.0 as usize).map(|s| s.as_str())
    }
}
