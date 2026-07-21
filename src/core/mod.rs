//! # `core` — Foundational Compiler & Runtime Primitives
//!
//! This module re-exports the two core sub-modules that form the backbone of
//! the Mizu compiler and runtime:
//!
//! * [`errors`] — the unified [`MizuError`] taxonomy.
//! * [`types`]  — the [`Value`] primitive and the [`VariableStore`] binding
//!   store.
//!
//! All other Mizu subsystems (parser, evaluator, renderer) depend on the types
//! declared here and access them via `crate::core::{errors, types}`.

#![forbid(unsafe_code)]

pub(crate) mod config;
pub mod errors;
pub mod storage;
pub mod types;
