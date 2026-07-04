//! # Mizu — Minimalist, Secure, Turing-Incomplete UI Hypermedia Runtime
//!
//! This crate is the monorepo root for the Mizu language toolchain:
//!
//! * **Compiler** — lexer → parser → type-checker → DAG resolver
//! * **Evaluator** — expression evaluator
//! * **Renderer** — GPU-accelerated layout engine (Phase 3+)
//!
//! ## Guiding Principles
//!
//! * **Turing-incomplete by design** — no loops, no recursion; the function
//!   call graph is verified acyclic before anything runs, so every single
//!   reaction is guaranteed to terminate.  Expressions are pure (no assignment
//!   or loop nodes exist in the AST); document state changes only through
//!   declared actions fired by outside events (clicks, timers, responses).
//! * **Zero `unsafe` code** — enforced crate-wide by `#![forbid(unsafe_code)]`.
//! * **Zero `unwrap` / `expect`** — every fallible operation surfaces through
//!   the [`core::errors::MizuError`] type hierarchy.
//! * **Named procedures** — logic blocks contain named procedures with optional
//!   parameters that can perform assignments, network calls, and other actions.

// Crate-wide safety and linting attributes.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::approx_constant)
)]
#![warn(missing_docs)]

pub mod core;
/// Networking subsystem
pub mod network;
pub mod parser;
pub mod render;
