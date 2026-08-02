//! The Pratt expression parser, block/action/timer grammar, and the
//! anti-recursion DAG check.
//!
//! Split by concern: [`expr`] (the Pratt expression parser), [`functions`]
//! (function-definition grammar), [`entry`] (`parse_logic`/
//! `parse_root_timers`/`parse_expr_standalone`/`check_dag`), [`action`]
//! (`parse_action`/`parse_action_with_urls`), and [`helpers`] (small leaf
//! helpers shared across the above).

mod action;
mod entry;
mod expr;
mod functions;
mod helpers;

pub use action::{parse_action, parse_action_with_urls};
pub use entry::{MAX_ROOT_TIMERS, parse_expr_standalone, parse_logic, parse_root_timers};
pub(crate) use expr::parse_expr_tree;
pub(crate) use helpers::path_param_ok;
