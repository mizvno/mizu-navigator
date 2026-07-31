//! # `types` — Core Value Primitives and Variable Store
//!
//! This module defines the fundamental data model of the Mizu runtime.
//!
//! ## Module Layout
//!
//! * [`interner`] — `Symbol` and `StringInterner`.
//! * [`value`] — the `Value` enum and JSON (de)serialization.
//! * [`eval`] — `Evaluator`, the evaluator, and the runtime budget
//!   constants (`MAX_INSTRUCTIONS`, `MAX_COMP_BINDINGS`, `MAX_EVAL_DEPTH`).
//! * [`store`] — `VariableStore`, the `Evaluator` + `StringInterner`
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

#[cfg(test)]
use eval::field_value;
pub use eval::{Evaluator, MAX_EVAL_DEPTH};
#[cfg(test)]
use eval::{compare_values, variant_weight};
pub use interner::{FrozenInterner, StringInterner, Symbol};
pub use store::VariableStore;
pub use value::{
    DECIMAL_SCALE, FileHandleData, RecordField, Value, from_json_slice, from_json_str, hash_field,
    to_json,
};
