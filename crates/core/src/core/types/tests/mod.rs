//! Test suite for `core::types`, split to mirror the source modules it
//! exercises: [`value`] (`Value` construction/Display/JSON/`FileHandle`),
//! [`store`] (`VariableStore` set/get/interpolate/overlay), [`eval`]
//! (`Evaluator` scoping, built-in functions, `compare_values`), and
//! [`interner`] (`StringInterner`/`FrozenInterner`).
//!
//! `eval` is by far the largest: every built-in function (`filter`, `sort`,
//! `count`, `length`, …) is a `FunctionCall` arm inside `Evaluator::evaluate`,
//! so testing them means testing `eval.rs`, the same as testing scoping or
//! `compare_values` does.

use super::{
    Evaluator, FileHandleData, StringInterner, Symbol, Value, VariableStore, compare_values,
    field_value, from_json_str, to_json, variant_weight,
};
use crate::core::errors::MizuError;
use crate::core::types::DECIMAL_SCALE;
use std::collections::HashMap;
use std::sync::Arc;

mod eval;
mod interner;
mod store;
mod value;
