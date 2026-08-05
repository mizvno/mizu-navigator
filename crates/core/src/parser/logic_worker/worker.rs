//! [`LogicWorker`]: wraps the `Evaluator` and its per-tab state map, spawns
//! the dedicated background thread, and dispatches each `UiEvent` to the
//! owning tab's state.

use std::sync::mpsc::{Receiver, Sender};

use rustc_hash::FxHashMap;

use crate::core::errors::MizuError;
use crate::messages::{TabId, UiEvent, WorkerResponse};

use super::session::TabSession;
use super::types::{MAX_WORKER_TABS, SPAWN_COUNT};

/// LogicWorker thread: the `mpsc` transport shell around [`TabSession`].
///
/// One thread serves every tab: opening a tab allocates a map entry, never a
/// thread. The consequence is head-of-line blocking — a slow evaluation in a
/// background tab delays the foreground tab's next event — which is bounded by
/// the per-event instruction budget in [`crate::core::types::Evaluator`].
///
/// # What lives here versus in [`TabSession`]
///
/// This type owns everything that is about *many* tabs and *this particular*
/// transport: the `TabId` → session map, the open-tab ceiling, `CloseTab`
/// (which destroys a session, something a session cannot do to itself), and
/// the `mpsc` endpoints. All document evaluation lives in `TabSession` and is
/// reached through the single call in [`run_loop`](Self::run_loop).
///
/// The separation is what lets an out-of-process worker reuse the evaluation
/// logic verbatim: it swaps this shell for an IPC frame loop and keeps the
/// session untouched.
pub struct LogicWorker {
    /// Per-document state, keyed by the tab that owns it.
    ///
    /// Every `Symbol` in a message for a given key is meaningful only against
    /// *that* entry's frozen interner; routing by `TabId` (never reused) is
    /// what keeps that sound with several documents resident.
    tabs: FxHashMap<TabId, TabSession>,
    /// Receiving channel for UI events, tagged with their destination tab.
    rx: Receiver<(TabId, UiEvent)>,
    /// Sending channel for state updates, capability actions, or timeout
    /// errors, tagged with the tab that produced them.
    tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
}

impl LogicWorker {
    /// Stack size for the dedicated evaluator thread.
    ///
    /// Alias for [`super::session::EVALUATOR_STACK_SIZE_BYTES`], which owns
    /// the measurement, the rationale, and the compile-time assertion tying
    /// it to `MAX_EVAL_DEPTH`. Kept as an associated constant because tests
    /// and docs refer to it by this name; the value itself belongs to
    /// evaluation, not to this transport, and outlives it.
    pub const STACK_SIZE_BYTES: usize = super::session::EVALUATOR_STACK_SIZE_BYTES;

    /// Spawns a permanent native thread executing the LogicWorker.
    ///
    /// Fails only if the OS refuses the thread/stack allocation (real
    /// resource exhaustion) — propagated as [`MizuError::IoError`] instead of
    /// panicking, so the caller can surface a real error instead of aborting
    /// the process on an opaque message.
    pub fn spawn(
        rx: Receiver<(TabId, UiEvent)>,
        tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    ) -> Result<std::thread::JoinHandle<()>, MizuError> {
        SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let handle = std::thread::Builder::new()
            .name("logic-worker".to_owned())
            .stack_size(Self::STACK_SIZE_BYTES)
            .spawn(move || {
                let mut worker = Self {
                    tabs: FxHashMap::default(),
                    rx,
                    tx,
                };
                worker.run_loop();
            })?;
        Ok(handle)
    }

    fn run_loop(&mut self) {
        while let Ok((tab_id, event)) = self.rx.recv() {
            // `Reload` creates a tab's state; `CloseTab` destroys it. Every
            // other event addresses state that must already exist — if it does
            // not, the tab was closed while the event was in flight and the
            // event is dropped. Never fall back to "some other tab": that is
            // exactly the cross-document write this routing exists to prevent.
            if matches!(event, UiEvent::CloseTab) {
                self.tabs.remove(&tab_id);
                continue;
            }
            if matches!(event, UiEvent::Reload(_))
                && !self.tabs.contains_key(&tab_id)
                && self.tabs.len() >= MAX_WORKER_TABS
            {
                tracing::warn!(
                    tab = tab_id.0,
                    open = self.tabs.len(),
                    "refusing Reload: worker tab limit reached"
                );
                continue;
            }
            let session = if matches!(event, UiEvent::Reload(_)) {
                self.tabs.entry(tab_id).or_default()
            } else {
                match self.tabs.get_mut(&tab_id) {
                    Some(t) => t,
                    None => {
                        tracing::debug!(tab = tab_id.0, "event for unknown tab; dropped");
                        continue;
                    }
                }
            };

            // The whole evaluation, in one transport-free call. `None` means
            // the event addressed nothing in this document (a click on a node
            // with no binding, a timer index past the end); that sends nothing
            // at all, rather than an empty update the UI would still process.
            let Some(response) = session.apply_event(event) else {
                continue;
            };
            if let Err(e) = self.tx.send((tab_id, response)) {
                tracing::warn!(error = %e, "UI response channel closed; update dropped");
            }
        }
    }
}

