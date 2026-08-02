//! # `flow` — Load-time Information Flow Checker
//!
//! Enforces invariant **F1** (gated information flow) from
//! `SECURITY-INVARIANTS.md`.  Runs after `check_dag` and `comp` extraction,
//! before the document is considered ready.
//!
//! ## Algorithm
//!
//! The checker computes a tainted-symbol set by iterative propagation over the
//! DAG of functions, computed variables, and assignments.  Because the graph is
//! acyclic and finite (enforced by `check_dag`), the fixpoint converges in a
//! bounded number of iterations.
//!
//! After convergence, every sink expression (`Action::Navigate.url`) is checked:
//! a sink whose expression reads any tainted symbol without a discharging gate
//! is rejected.
//!
//! ## Soundness
//!
//! The checker is **sound** (never misses a real source→sink flow) and **may be
//! conservative** (over-approximation → spurious rejection is acceptable).
//! Any analysis uncertainty (unresolved symbol, unexpected node) is treated as
//! tainted/rejected, never as clean.
//!
//! Split by concern: [`types`] (`ActionContext`/`TaintOrigin`), [`check`]
//! (`check_information_flow`, the entry point), and [`helpers`] (the
//! expression-level taint-checking/diagnostic helpers it calls).

mod check;
mod helpers;
#[cfg(test)]
mod tests;
mod types;

pub use check::check_information_flow;
pub use types::ActionContext;
