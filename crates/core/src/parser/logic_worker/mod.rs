//! Logic worker module executing the state machine on a dedicated background thread.
//!
//! Split by concern: [`types`] (`LogicWorkerTabState`, spawn/tab-count
//! constants), [`worker`] (`LogicWorker`, the thread entry point and event
//! loop), and [`helpers`] (`send_response`/`recompute_after_mutation`/
//! `execute_and_respond`, the per-action response plumbing).

#![forbid(unsafe_code)]

mod helpers;
#[cfg(test)]
mod tests;
mod types;
mod worker;

pub use types::{LogicWorkerTabState, MAX_WORKER_TABS, SPAWN_COUNT};
pub use worker::LogicWorker;
