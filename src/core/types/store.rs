//! `VariableStore`, the `StateMachine` + `StringInterner` wrapper.

use std::collections::HashMap;

use crate::core::errors::MizuError;

use super::eval::StateMachine;
use super::interner::{StringInterner, Symbol};
use super::value::Value;

/// A backwards compatibility layer wrapping StateMachine and StringInterner.
#[derive(Debug, Clone, Default)]
pub struct VariableStore {
    /// The underlying flat evaluator state (globals, locals, budgets, queued actions).
    pub state_machine: StateMachine,
    /// Name ↔ `Symbol` mapping shared with `state_machine`'s expressions.
    pub interner: StringInterner,
}

impl VariableStore {
    /// Creates an empty store with a fresh, unfrozen interner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state_machine: StateMachine::new(),
            interner: StringInterner::new(),
        }
    }

    /// Creates an empty store reusing an existing (typically frozen) interner.
    #[must_use]
    pub fn with_interner(interner: StringInterner) -> Self {
        Self {
            state_machine: StateMachine::new(),
            interner,
        }
    }

    /// Binds `sym` directly to `value`, bypassing name interning.
    pub fn set_symbol(&mut self, sym: Symbol, value: impl Into<Value>) {
        self.state_machine.set_global(sym, value.into());
    }

    /// Binds `name` to `value`.
    ///
    /// Calls [`StringInterner::get_or_intern`] to intern the name.  Do **not**
    /// call this method after the interner has been
    /// [`freeze`](StringInterner::freeze)d with a runtime-generated string;
    /// use [`set_runtime`](Self::set_runtime) instead.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<Value>) {
        let name_str = name.into();
        let value_val = value.into();
        let sym = self.interner.get_or_intern(&name_str);
        self.state_machine.set_global(sym, value_val);
    }

    /// Frozen-safe version of [`set`](Self::set).
    ///
    /// Uses [`StringInterner::get`] (read-only) instead of
    /// [`get_or_intern`](StringInterner::get_or_intern).  If `name` is already
    /// in the interner the value is stored normally.  If `name` is **not** in
    /// the interner (i.e. it was not declared in the parse phase), the call is
    /// a no-op and a `tracing::debug!` is emitted — the frozen symbol table is
    /// never mutated.
    ///
    /// Use this method anywhere that runs after [`StringInterner::freeze`] and
    /// may encounter strings not declared at compile time, e.g.:
    /// - `UiEvent::SubmitForm` field names
    /// - `UiEvent::UpdateVariable` variable names from network responses
    pub fn set_runtime(&mut self, name: &str, value: impl Into<Value>) {
        if let Some(sym) = self.interner.get(name) {
            self.state_machine.set_global(sym, value.into());
        } else {
            tracing::debug!(
                name,
                "set_runtime: `{}` is not in the frozen interner — declare it in \
                 the logic block to make it bindable at runtime",
                name
            );
        }
    }

    /// Looks up `name` as a local (frame 0) or non-null global.
    ///
    /// # Errors
    ///
    /// Returns [`MizuError::VariableNotFound`] if `name` is unknown or unbound.
    pub fn get(&self, name: &str) -> Result<&Value, MizuError> {
        if let Some(sym) = self.interner.get(name) {
            if let Some(val) = self.state_machine.get_local(sym, 0) {
                return Ok(val);
            }
            let val = self.state_machine.get_global(sym);
            if !matches!(val, Value::Null) {
                return Ok(val);
            }
        }
        Err(MizuError::VariableNotFound(name.to_owned()))
    }

    /// Replaces every `{name}` placeholder in `text` with the string form of
    /// the corresponding variable's value.
    ///
    /// # Errors
    ///
    /// Returns [`MizuError::BindingNotFound`] if a placeholder references an
    /// unbound name.
    pub fn interpolate(&self, text: &str) -> Result<String, MizuError> {
        let mut buf = String::with_capacity(text.len());
        self.state_machine
            .interpolate_into(text, &self.interner, &mut buf)?;
        Ok(buf)
    }

    /// Interpolates string placeholders, checking `overlay` before the global store.
    ///
    /// `overlay` is a small per-iteration binding map used by `each` loops to inject
    /// the current element value (e.g. `item → Record{…}`) without mutating the store.
    /// If `overlay` is empty, this is identical to [`Self::interpolate`].
    ///
    /// Unlike the previous implementation, this method passes `overlay` directly into
    /// the interpolation engine as an `Option<&HashMap<…>>` parameter — no clone of
    /// `StateMachine` or `StringInterner` is performed.
    pub fn interpolate_with_overlay(
        &self,
        text: &str,
        overlay: &HashMap<String, crate::core::types::Value>,
    ) -> Result<String, MizuError> {
        let mut buf = String::with_capacity(text.len());
        let overlay_opt = if overlay.is_empty() {
            None
        } else {
            Some(overlay)
        };
        self.state_machine.interpolate_into_with_overlay(
            text,
            &self.interner,
            overlay_opt,
            &mut buf,
        )?;
        Ok(buf)
    }
}
