//! Document logic evaluation, split from the transport that drives it.
//!
//! * [`session`] — [`TabSession`], one tab's document state plus the pure,
//!   synchronous `apply_event` state machine. No channels, no threads, no
//!   knowledge of how events arrive. This is the reusable core.
//! * [`worker`] — [`LogicWorker`], the `mpsc` shell: a thread, a `TabId` →
//!   session map, the open-tab ceiling, and `CloseTab`. Everything here is
//!   about *many* tabs and *this* transport.
//! * [`types`] — spawn/tab-count constants and shared type aliases.
//! * [`helpers`] — [`resolve_endpoint_url`], shared by the session's own
//!   alias resolution and by the main process's capability broker.
//!
//! The split exists so an out-of-process worker can drive `TabSession` from
//! an IPC frame loop without duplicating a line of evaluation logic — it
//! replaces `LogicWorker`, not the session.

#![forbid(unsafe_code)]

mod helpers;
mod session;
#[cfg(test)]
mod tests;
mod types;
mod worker;

pub use helpers::resolve_endpoint_url;
pub use session::{EVALUATOR_STACK_SIZE_BYTES, TabSession};
pub use types::{MAX_WORKER_TABS, SPAWN_COUNT};
pub use worker::LogicWorker;
