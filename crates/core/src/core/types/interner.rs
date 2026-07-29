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
/// **clone** of a `FrozenInterner` after the initial parse phase.
/// There is no shared/locked table, so there is no lock contention.
/// The two clones are guaranteed to agree on every `Symbol(u32)` ID
/// because [`Clone`] preserves the source's contents exactly.
///
/// Once frozen, a thread cannot mint symbols for new names.
/// Post-freeze code that may encounter strings not declared in the logic block
/// (form field names, network response variable names) must use
/// [`VariableStore::set_runtime`] instead of [`VariableStore::set`].
/// `set_runtime` calls [`get`](FrozenInterner::get) and silently discards
/// unknown names, so the frozen symbol table is never mutated.
#[derive(Debug, Default)]
pub struct StringInterner {
    /// Name → `Symbol` lookup, the inverse of `vec`.
    pub map: HashMap<String, Symbol>,
    /// `Symbol(i)` resolves to `vec[i]`; append-only.
    pub vec: Vec<String>,
}

impl StringInterner {
    /// Creates a new empty interner.
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            vec: Vec::new(),
        }
    }

    /// Freezes the interner, preventing further additions and returning
    /// a read-only `FrozenInterner` that can be safely cloned and shared.
    pub fn freeze(self) -> FrozenInterner {
        FrozenInterner {
            map: self.map,
            vec: self.vec,
        }
    }

    /// Interns `s` and returns its [`Symbol`], inserting it into this
    /// interner's own table if it is not already present.
    pub fn get_or_intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        let id = self.vec.len() as u32;
        let sym = Symbol(id);
        self.map.insert(s.to_string(), sym);
        self.vec.push(s.to_string());
        sym
    }

    /// Returns the [`Symbol`] for `s` if it was interned, or `None`.
    pub fn get(&self, s: &str) -> Option<Symbol> {
        self.map.get(s).copied()
    }

    /// Resolves a Symbol back to its string representation.
    pub fn resolve(&self, sym: Symbol) -> Option<&str> {
        self.vec.get(sym.0 as usize).map(|s| s.as_str())
    }
}

/// A read-only symbol table. Created by freezing a [`StringInterner`].
#[derive(Debug, Clone, Default)]
pub struct FrozenInterner {
    pub map: HashMap<String, Symbol>,
    pub vec: Vec<String>,
}

impl FrozenInterner {
    /// Returns the [`Symbol`] for `s` if it was interned, or `None`.
    pub fn get(&self, s: &str) -> Option<Symbol> {
        self.map.get(s).copied()
    }

    /// Resolves a Symbol back to its string representation.
    pub fn resolve(&self, sym: Symbol) -> Option<&str> {
        self.vec.get(sym.0 as usize).map(|s| s.as_str())
    }
}
