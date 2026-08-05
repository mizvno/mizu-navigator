//! # `worker_host` — the live capability broker, main-process side
//!
//! Owns one sandboxed `mizu-worker` process and mediates everything it asks
//! for. This is where the three earlier phases meet: the Phase 4 spawner
//! starts the process, [`mizu_ipc::wire::rehydrate`] decodes what it sends,
//! and the Phase 3 [`broker::authorize_action`] gate decides what actually
//! happens.
//!
//! ## One worker per tab
//!
//! The in-process [`LogicWorker`](mizu_core::parser::LogicWorker) multiplexes
//! every tab onto one thread and routes by `TabId`. A `WorkerHost` does not
//! route at all: it holds exactly one process serving exactly one document,
//! and the OS boundary *is* the isolation between tabs. That is the whole
//! point — two documents sharing an address space share an attack surface,
//! however carefully the `TabId` routing is written.
//!
//! ## Nothing the worker says is taken at face value
//!
//! A response arriving here has crossed a trust boundary, so it is treated
//! as hostile input twice over:
//!
//! 1. **Structurally**, by `rehydrate_worker_response` — parallel vectors
//!    must agree in length, values must not nest unboundedly, symbols must
//!    be in range.
//! 2. **By policy**, by [`broker::authorize_action`] — every action is
//!    re-derived against state the worker cannot reach (this document's own
//!    `UrlRegistry`, and the `UiEvent` the broker itself dispatched).
//!
//! The `gesture` flag on the response is deliberately *ignored* by the
//! authorization path. It is the worker's claim about agency, and a
//! compromised worker would simply set it. The broker instead remembers
//! which event it sent and derives agency from that — see
//! [`WorkerHost::dispatch_event`].
//!
//! # Not yet wired into the window manager, and why
//!
//! [`WorkerHost::dispatch_event`] is **synchronous**: it sends an event and
//! blocks until the reply arrives. The window manager is **asynchronous**
//! and cannot use it as-is. Two independent reasons, both structural:
//!
//! 1. **It would freeze the UI.** Today a `UiEvent` is posted with
//!    `logic_tx.send(..)` and the replies are drained later, non-blocking,
//!    via `logic_rx.try_recv()` in the idle loop. A slow evaluation
//!    therefore only delays *subsequent* events; the window keeps painting
//!    and scrolling. Calling `dispatch_event` from an input handler would
//!    block the UI thread on a cross-process round trip for every click and
//!    keystroke, so one slow document would hang the whole browser — a worse
//!    failure than the head-of-line blocking the shared worker thread has
//!    today.
//! 2. **The call sites cannot consume a reply.** All twelve of them are
//!    `let _ = logic_tx.send(..)` inside functions returning `bool` or
//!    nothing (`input::dispatch_click_gesture`, the mouse/keyboard/a11y
//!    handlers). There is nowhere for an `AuthorizedResponse` to go without
//!    restructuring each one.
//!
//! ## The bridge this needs
//!
//! Not a rewrite of the window manager — an adapter that restores the
//! asynchrony the manager already expects:
//!
//! * One reader thread per worker, owning the [`mizu_ipc::IpcReceiver`],
//!   doing the blocking `recv` off the UI thread and forwarding each
//!   envelope into the existing
//!   `Sender<(TabId, Result<WorkerResponse, MizuError>)>`. The idle loop's
//!   drain then works unchanged.
//! * A per-tab FIFO of in-flight `UiEvent`s on the main thread. Correlating
//!   reply *N* with event *N* is sound precisely because the protocol is
//!   strictly one-to-one and ordered — which is what
//!   [`mizu_ipc::wire::WireWorkerEnvelope::NoOp`] exists to guarantee. The
//!   drain pops the matching event and passes it to
//!   [`authorize_action`], so gate G1 keeps deriving agency from the event
//!   the broker actually sent.
//! * `logic_tx` becomes a router that picks the right worker by `TabId`
//!   instead of a single shared channel.
//!
//! Until that exists, the `mpsc` [`LogicWorker`](mizu_core::parser::LogicWorker)
//! remains the production path and must not be deleted: it is what the
//! browser currently runs on.
pub mod bridge;


use mizu_core::core::errors::MizuError;
use mizu_core::messages::{RuntimeAction, UiEvent, WorkerResponse};
use mizu_core::parser::UrlRegistry;
use mizu_ipc::process::{WorkerProcess, spawn_worker};
use mizu_ipc::wire::rehydrate::rehydrate_worker_response;
use mizu_ipc::wire::{WireUiEvent, WireWorkerEnvelope};

use crate::render::security::broker::{ActionOrigin, EventAgency, authorize_action};

/// Why a tab's worker process is no longer usable.
///
/// Distinguishing these is the difference between a routine tab close and a
/// security incident, and the only way to tell them apart is the child's
/// exit status — every one of them looks like a broken pipe from the
/// channel's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabCrash {
    /// The worker exited 0. Normal: the tab was closed, or the broker
    /// dropped the channel.
    CleanExit,
    /// The worker was killed by its own sandbox — `SIGSYS` from the seccomp
    /// filter, or terminated by the Windows Job Object.
    ///
    /// This is the loud one. A worker only reaches a denied syscall if the
    /// document drove it somewhere the allowlist forbids, which under this
    /// design means either a compromise attempt or a genuine gap in the
    /// allowlist. Both need to be seen, so it is reported as a
    /// [`MizuError::SecurityViolation`], not swallowed as a crash.
    SandboxViolation {
        /// Raw exit status as the OS reported it, for the log.
        status: String,
    },
    /// The worker died some other way (panic, OOM kill, abort).
    Crashed {
        /// Raw exit status as the OS reported it.
        status: String,
    },
    /// The channel broke while the worker is still running, or a frame was
    /// unusable. The worker is killed rather than left orphaned.
    Protocol(String),
}

impl TabCrash {
    /// Converts into the project's error hierarchy.
    ///
    /// A sandbox kill becomes a `SecurityViolation` so it surfaces alongside
    /// quota and gesture failures rather than being filed as generic I/O.
    #[must_use]
    pub fn into_mizu_error(self) -> MizuError {
        match self {
            TabCrash::CleanExit => {
                MizuError::IpcError("worker exited normally".to_string())
            }
            TabCrash::SandboxViolation { status } => MizuError::SecurityViolation(format!(
                "worker process was terminated by its sandbox ({status}); \
                 the document attempted an operation the confinement forbids"
            )),
            TabCrash::Crashed { status } => {
                MizuError::IpcError(format!("worker process died unexpectedly ({status})"))
            }
            TabCrash::Protocol(msg) => MizuError::IpcError(msg),
        }
    }

    /// Whether the user should be shown a crash page for this outcome.
    ///
    /// A clean exit is the expected end of a tab's life and must not paint
    /// one.
    #[must_use]
    pub fn warrants_crash_page(&self) -> bool {
        !matches!(self, TabCrash::CleanExit)
    }
}

/// A sandboxed worker process serving one document, plus the policy state
/// needed to judge what it asks for.
pub struct WorkerHost {
    process: WorkerProcess,
    /// The broker's own copy of the document's endpoint table. Actions
    /// naming an alias are resolved against *this*, never against anything
    /// the worker sends.
    url_registry: UrlRegistry,
    /// This document's domain, for composing `api` endpoint URLs.
    document_domain: String,
}

impl WorkerHost {
    /// Spawns a worker for a document and completes the handshake.
    ///
    /// `url_registry` and `document_domain` come from the broker's own parse
    /// of the document — the same values it puts in the `ReloadPayload`, kept
    /// here so authorization never has to ask the worker what they were.
    pub fn spawn(
        worker_exe: &std::path::Path,
        url_registry: UrlRegistry,
        document_domain: String,
    ) -> Result<Self, MizuError> {
        let process = spawn_worker(worker_exe, &[])
            .map_err(|e| MizuError::IpcError(format!("failed to start worker: {e}")))?;
        Ok(WorkerHost {
            process,
            url_registry,
            document_domain,
        })
    }

    /// The worker's OS process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Sends one event and returns the worker's authorized response.
    ///
    /// The `event` is retained across the round trip on purpose: it is the
    /// broker's own record of what it asked for, and it is what
    /// [`authorize_action`] consults to decide whether a `Navigate` had a
    /// real user gesture behind it. The worker's `gesture` flag never enters
    /// that decision.
    ///
    /// Returns `Ok(None)` when the worker had nothing to say — an unbound
    /// click, a timer index past the end.
    ///
    /// # Errors
    ///
    /// [`TabCrash`] if the worker died or the channel broke. The caller
    /// should tear down the tab and, if
    /// [`warrants_crash_page`](TabCrash::warrants_crash_page), show one.
    pub fn dispatch_event(
        &mut self,
        event: &UiEvent,
    ) -> Result<Option<AuthorizedResponse>, TabCrash> {
        self.process
            .tx
            .send(&WireUiEvent::from(event))
            .map_err(|e| self.classify_failure(&e))?;

        // `CloseTab` is the one event with no reply: it ends the worker's
        // life, so the process exits instead of answering. Every other event
        // owes exactly one frame — including `NoOp` when nothing was bound —
        // which is what makes this blocking recv safe. See
        // `WireWorkerEnvelope::NoOp` for why the broker must not try to
        // predict which events produce a response.
        if matches!(event, UiEvent::CloseTab) {
            return Ok(None);
        }

        let envelope = self
            .process
            .rx
            .recv()
            .map_err(|e| self.classify_failure(&e))?;

        let wire_response = match envelope {
            WireWorkerEnvelope::Ok(r) => r,
            // Nothing was bound to this event. The tab is healthy; there is
            // simply no state update.
            WireWorkerEnvelope::NoOp => return Ok(None),
            WireWorkerEnvelope::Err(e) => {
                // A worker-side evaluation error is a document bug, not a
                // transport failure: the channel is fine and the tab lives.
                return Ok(Some(AuthorizedResponse {
                    response: WorkerResponse {
                        state_update: mizu_core::messages::StateUpdate {
                            mutated_variables: Vec::new(),
                        },
                        runtime_actions: Vec::new(),
                        gesture: false,
                    },
                    authorized_actions: Vec::new(),
                    rejected: vec![MizuError::ExecutionError(format!("{e:?}"))],
                }));
            }
            // `Hello` is only legal as the first frame, and the handshake
            // already consumed it. Replaying it here would be an attempt to
            // confuse the broker's state machine.
            WireWorkerEnvelope::Hello { .. } => {
                return Err(TabCrash::Protocol(
                    "worker replayed the Hello handshake frame mid-session".to_string(),
                ));
            }
        };

        // Structural validation, before any of it is believed.
        let response = rehydrate_worker_response(&wire_response)
            .map_err(|e| TabCrash::Protocol(format!("malformed worker response: {e}")))?;

        // Policy validation, action by action.
        let mut authorized_actions = Vec::new();
        let mut rejected = Vec::new();
        for action in response.runtime_actions.iter().cloned() {
            match authorize_action(
                action,
                ActionOrigin::SandboxedIpcWorker,
                &self.document_domain,
                &self.url_registry,
                EventAgency::of(event),
            ) {
                Ok(approved) => authorized_actions.push(approved),
                Err(e) => {
                    tracing::warn!(error = %e, "broker rejected a worker action");
                    rejected.push(e);
                }
            }
        }

        Ok(Some(AuthorizedResponse {
            response,
            authorized_actions,
            rejected,
        }))
    }

    /// Works out why the channel broke, by asking the OS what became of the
    /// child.
    fn classify_failure(&mut self, e: &mizu_ipc::IpcError) -> TabCrash {
        match self.process.try_exit_status() {
            Ok(Some(status)) => {
                if status.success() {
                    TabCrash::CleanExit
                } else if is_sandbox_kill(&status) {
                    TabCrash::SandboxViolation {
                        status: format!("{status:?}"),
                    }
                } else {
                    TabCrash::Crashed {
                        status: format!("{status:?}"),
                    }
                }
            }
            // Still running, but unreachable: the channel itself failed.
            Ok(None) => TabCrash::Protocol(format!("worker channel failed: {e}")),
            Err(wait_err) => TabCrash::Protocol(format!(
                "worker channel failed ({e}), and its status was unreadable ({wait_err})"
            )),
        }
    }

    /// Shuts the worker down, giving it `grace` to notice EOF and exit on
    /// its own before it is killed.
    pub fn shutdown(self, grace: std::time::Duration) -> Result<(), MizuError> {
        self.process
            .shutdown(grace)
            .map(|_| ())
            .map_err(|e| MizuError::IpcError(format!("worker shutdown: {e}")))
    }
}

/// A worker response that has been through both validation layers.
#[derive(Debug)]
pub struct AuthorizedResponse {
    /// The raw response, structurally validated. Its `runtime_actions` are
    /// what the worker *asked* for and must not be executed directly — use
    /// `authorized_actions`.
    pub response: WorkerResponse,
    /// The actions the broker is willing to execute, in order. A
    /// `NetworkCall` appears here already resolved into a `ResolvedCall`
    /// against the broker's own registry.
    pub authorized_actions: Vec<RuntimeAction>,
    /// Actions the broker refused, with the reason. Surfaced rather than
    /// dropped so a rejection is visible in the inspector instead of looking
    /// like the document silently doing nothing.
    pub rejected: Vec<MizuError>,
}

/// Whether an exit status indicates the sandbox killed the process.
///
/// * **UNIX**: `SIGSYS` (31) is what `SECCOMP_RET_KILL_PROCESS` delivers.
///   `SIGKILL` is included because a Job-Object-equivalent teardown or an
///   LSM can present that way.
/// * **Windows**: a Job Object termination surfaces as a non-zero exit code
///   the process never chose. There is no dedicated code, so this errs
///   toward *not* crying wolf: only the documented job-termination code is
///   treated as a sandbox kill, and anything else is a plain crash.
#[cfg(unix)]
fn is_sandbox_kill(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    matches!(status.signal(), Some(libc_sigsys) if libc_sigsys == 31 || libc_sigsys == 9)
}

#[cfg(windows)]
fn is_sandbox_kill(status: &std::process::ExitStatus) -> bool {
    // `ERROR_NOT_ENOUGH_QUOTA` (1816) is what a Job Object active-process
    // limit produces, and `STATUS_ACCESS_DENIED` (0xC0000022) is the usual
    // shape of an integrity-level refusal turning fatal.
    matches!(status.code(), Some(1816) | Some(-1073741790))
}

#[cfg(test)]
mod tests;
