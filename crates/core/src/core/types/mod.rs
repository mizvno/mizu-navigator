//! # `types` — Core Value Primitives and Variable Store
//!
//! This module defines the fundamental data model of the Mizu runtime.
//!
//! ## Module Layout
//!
//! * [`interner`] — `Symbol` and `StringInterner`.
//! * [`value`] — the `Value` enum and JSON (de)serialization.
//! * [`eval`] — `StateMachine`, the evaluator, and the runtime budget
//!   constants (`MAX_INSTRUCTIONS`, `MAX_COMP_BINDINGS`, `MAX_EVAL_DEPTH`).
//! * [`store`] — `VariableStore`, the `StateMachine` + `StringInterner`
//!   wrapper used throughout the rest of the crate.
//!
//! Every item that was previously a direct member of this module is
//! re-exported below, so `crate::core::types::X` paths are unaffected by
//! this split.

#![forbid(unsafe_code)]

mod eval;
mod interner;
mod store;
#[cfg(test)]
mod tests;
mod value;

pub use eval::{MAX_COMP_BINDINGS, MAX_EVAL_DEPTH, MAX_INSTRUCTIONS, StateMachine};
#[cfg(test)]
use eval::{compare_values, field_value, variant_weight};
pub use interner::{StringInterner, Symbol};
pub use store::VariableStore;
pub use value::{DECIMAL_SCALE, Value, from_json, to_json};
