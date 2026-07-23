//! # `logic` — Mizu Logic Block Parser, DAG Validator & Expression Evaluator
//!
//! This module implements Phase 4 of the Mizu compilation pipeline: it
//! transforms the raw `logic_block` string produced by [`super::splitter`]
//! into a validated, recursion-free [`HashMap`] of procedure definitions,
//! and exposes an [`evaluate`] function to reduce any [`Expr`] to a
//! concrete [`Value`].
//!
//! ## Pipeline Position
//!
//! ```text
//! logic_block: String   (from parser::splitter)
//!        │
//!        ▼
//! ┌─────────────────────────────────┐
//! │  logic::lex                     │  tokenise the source text
//! │  logic::parse_function_def      │  Pratt-parse each function
//! │  logic::check_dag               │  Kahn's algorithm cycle detection
//! │  logic::evaluate                │  expression evaluator
//! └───────────────┬─────────────────┘
//!                 │  HashMap<Symbol, MizuFunction>
//!                 ▼
//!         (Phase 5) renderer / layout binding
//! ```
//!
//! ## Parsing Strategy
//!
//! ### Tokeniser
//!
//! A hand-rolled byte scanner converts the source text into a flat
//! [`Vec<Token>`].  This avoids any regex dependency and keeps error positions
//! precise.
//!
//! ### Expression Parser — Pratt (Top-Down Operator Precedence)
//!
//! Operator precedence is encoded as a *binding power* table:
//!
//! | Operator | Left BP | Right BP |
//! |----------|---------|----------|
//! | `+`      | 10      | 11       |
//! | `-`      | 10      | 11       |
//! | `*`      | 20      | 21       |
//! | `/`      | 20      | 21       |
//!
//! Higher binding power means the operator binds its operands more tightly.
//! The right BP is one higher than the left to implement left-associativity
//! (e.g., `1 - 2 - 3` parses as `(1 - 2) - 3`).
//!
//! ### Anti-Recursion DAG — Kahn's Algorithm
//!
//! After all function bodies are parsed, a directed call-graph is constructed:
//! a directed edge `A → B` means function `A` calls function `B`.  Kahn's
//! algorithm performs a BFS topological sort:
//!
//! 1. Compute the in-degree of every node.
//! 2. Enqueue all nodes with in-degree zero.
//! 3. For each node dequeued, reduce its successors' in-degrees; enqueue newly
//!    zero-in-degree nodes.
//! 4. If the queue empties before all nodes are visited, a cycle exists →
//!    `MizuError::ParseError("Recursion and infinite loops are strictly forbidden")`.
//!
//! This guarantees compile-time rejection of both direct recursion (`f` calls
//! `f`) and mutual recursion (`f` calls `g`, `g` calls `f`).
//!
//! ## Module Layout
//!
//! * [`ast`] — the AST/type definitions (`Expr`, `Action`, `BinOp`, ...).
//! * [`lexer`] — the hand-rolled tokeniser.
//! * [`parse`] — the Pratt expression parser, block/action/timer grammar, and
//!   the anti-recursion DAG check.
//! * [`comp`] — `comp` (computed variable) parsing, dependency tracking, and
//!   incremental recomputation.
//! * [`purity`] — the P1 purity/effectful-call checker.
//! * [`eval`] — the expression evaluator and binary-op semantics.
//!
//! Every item that was previously a direct member of this module is
//! re-exported below, so `crate::parser::logic::X` paths are unaffected by
//! this split.

#![forbid(unsafe_code)]

mod ast;
mod comp;
mod eval;
mod lexer;
mod parse;
mod purity;
#[cfg(test)]
mod tests;

pub use ast::{
    Action, BinOp, ComputedBinding, Expr, MizuFunction, NetworkMethod, RootTimer, TimerInterval,
    ValueType,
};
pub use comp::{
    CompReverseIndex, build_comp_reverse_index, parse_computed, parse_computed_with_functions,
    recompute_computed_bindings,
};
#[cfg(test)]
pub(crate) use comp::recompute_computed_bindings_naive_scan;
pub use eval::{evaluate, execute_action};
pub(crate) use eval::{apply_binop, check_type, type_name};
pub use parse::{
    parse_action, parse_action_with_urls, parse_expr_standalone, parse_logic, parse_root_timers,
};
pub(crate) use parse::path_param_ok;
pub use purity::find_side_effect_call;
