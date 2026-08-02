//! `Evaluator`, the evaluator, and the runtime budget constants.
//!
//! Split by concern: [`types`] (the `Evaluator` struct, scope management),
//! [`interpolate`] (text interpolation), [`evaluate`] (the recursive
//! expression evaluator), and [`compare`] (value comparison/dot-path
//! resolution).

mod compare;
mod evaluate;
mod interpolate;
mod types;

pub use types::{Evaluator, MAX_EVAL_DEPTH};
// Re-exported for `core::types::tests::store`, which references it via the
// full `crate::core::types::eval::MAX_INTERPOLATED_BYTES` path — the lib
// build itself never names it directly.
#[allow(unused_imports)]
pub use types::MAX_INTERPOLATED_BYTES;

#[cfg(test)]
pub(crate) use compare::{compare_values, field_value, variant_weight};
