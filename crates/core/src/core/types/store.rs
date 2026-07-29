//! `VariableStore`, the `StateMachine` + `StringInterner` wrapper.

use std::collections::HashMap;

use crate::core::errors::MizuError;

use super::eval::StateMachine;
use super::interner::{FrozenInterner, StringInterner, Symbol};
use super::value::Value;

/// A backwards compatibility layer wrapping StateMachine and StringInterner.
#[derive(Debug, Clone, Default)]
pub struct VariableStore<I = FrozenInterner> {
    /// The underlying flat evaluator state (globals, locals, budgets, queued actions).
    pub state_machine: StateMachine,
    /// Name ↔ `Symbol` mapping shared with `state_machine`'s expressions.
    pub interner: I,
}

impl VariableStore<StringInterner> {
    /// Creates an empty store with a fresh, unfrozen interner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state_machine: StateMachine::default(),
            interner: StringInterner::new(),
        }
    }

    /// Freezes the store's interner, transitioning to a runtime-safe VariableStore.
    pub fn freeze(self) -> VariableStore<FrozenInterner> {
        VariableStore {
            state_machine: self.state_machine,
            interner: self.interner.freeze(),
        }
    }

    /// Binds `sym` directly to `value`, bypassing name interning.
    pub fn set_symbol(&mut self, sym: Symbol, value: impl Into<Value>) {
        self.state_machine.set_global(sym, value.into());
    }

    /// Binds `name` to `value`.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<Value>) {
        let name_str = name.into();
        let value_val = value.into();
        let sym = self.interner.get_or_intern(&name_str);
        self.state_machine.set_global(sym, value_val);
    }
}

impl VariableStore<FrozenInterner> {
    /// Creates an empty store reusing an existing frozen interner.
    #[must_use]
    pub fn with_interner(interner: FrozenInterner) -> Self {
        Self {
            state_machine: StateMachine::default(),
            interner,
        }
    }

    /// Binds `sym` directly to `value`, bypassing name interning.
    pub fn set_symbol(&mut self, sym: Symbol, value: impl Into<Value>) {
        self.state_machine.set_global(sym, value.into());
    }

    /// Frozen-safe version of `set`.
    ///
    /// Uses [`FrozenInterner::get`] (read-only). If `name` is already
    /// in the interner the value is stored normally. If `name` is **not** in
    /// the interner, the call is a no-op and a `tracing::debug!` is emitted.
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
    pub fn interpolate(&self, text: &str) -> Result<String, MizuError> {
        let mut buf = String::with_capacity(text.len());
        self.state_machine
            .interpolate_into(text, &self.interner, &mut buf)?;
        Ok(buf)
    }

    /// Interpolates string placeholders, checking `overlay` before the global store.
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
