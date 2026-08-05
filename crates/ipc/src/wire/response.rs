//! # `wire::response` — `WireWorkerResponse`, `WireWorkerError`, `WireWorkerEnvelope`
//!
//! These types flow **worker → main process** as the top-level framed message
//! after every event the worker processes.
//!
//! ## Envelope pattern
//!
//! Every worker response is wrapped in `WireWorkerEnvelope`:
//!
//! ```text
//! WireWorkerEnvelope::Ok(WireWorkerResponse)   — successful cycle
//! WireWorkerEnvelope::Err(WireWorkerError)      — worker-side failure
//! ```
//!
//! The broker matches on the outer variant before attempting to extract a
//! `WireWorkerResponse`, so a worker failure is handled independently of
//! capability dispatch.
//!
//! ## Security: `gesture` flag
//!
//! `WireWorkerResponse::gesture` mirrors the `WorkerResponse::gesture` field
//! and is subject to the same G1 invariant documented in `SECURITY-INVARIANTS.md`:
//! the flag is derived from the *event variant* inside the worker (not from
//! any ambient state) and travels with the response that the event produced.
//! The broker reads this flag when dispatching `Navigate` and `CopyToClipboard`
//! actions.

#![forbid(unsafe_code)]

use rkyv::{Archive, Deserialize, Serialize};

use crate::wire::actions::WireRuntimeAction;
use crate::wire::value::WireValue;

/// Wire-format mirror of [`mizu_core::messages::WorkerResponse`].
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct WireWorkerResponse {
    /// Symbols (raw u32) of mutated variables, parallel to `mutated_values`.
    pub mutated_syms:   Vec<u32>,
    /// New values for mutated variables, parallel to `mutated_syms`.
    pub mutated_values: Vec<WireValue>,
    /// Capability actions the broker must validate and execute.
    pub runtime_actions: Vec<WireRuntimeAction>,
    /// `true` iff the triggering `UiEvent` was a user gesture (Click /
    /// SubmitForm).  Carried from the worker to the broker to preserve the
    /// G1 security invariant.
    pub gesture: bool,
}

/// Wire-encoded error propagated from the worker to the broker.
///
/// `MizuError` itself is not serialized across the boundary because it
/// contains non-`Archive` types (`std::io::Error`, `hickory_resolver`
/// types).  The worker maps errors to these simplified variants before
/// sending.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireWorkerError {
    /// The worker's instruction budget was exceeded.
    Timeout,
    /// A security-policy violation detected inside the worker.
    SecurityViolation(String),
    /// A runtime execution error.
    ExecutionError(String),
    /// Unexpected internal failure (catch-all).
    Internal(String),
}

/// Top-level envelope framed over the IPC socket (worker → broker).
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub enum WireWorkerEnvelope {
    /// Handshake frame: always the **first** message on a fresh channel.
    ///
    /// Carries the secret the broker passed to the process it spawned, so
    /// the broker can tell its own child apart from a same-user process that
    /// guessed the rendezvous name and connected first. A peer that cannot
    /// produce the token is disconnected before any other frame is read.
    ///
    /// Rejecting this variant everywhere *except* the handshake is what
    /// stops a compromised worker from replaying it later to confuse the
    /// broker's state machine.
    Hello {
        /// The token echoed back from `MIZU_IPC_TOKEN`.
        token: String,
    },
    /// Successful event cycle.
    Ok(WireWorkerResponse),
    /// Worker-side failure.
    Err(WireWorkerError),
    /// The event addressed nothing in this document — an unbound click, a
    /// timer index past the end — so there is no state update to report.
    ///
    /// # Why this exists rather than sending nothing
    ///
    /// The in-process worker simply stays silent in this case, because an
    /// `mpsc` channel has no notion of a reply owed. Over IPC that would be
    /// a deadlock: only the worker holds the document's action tables, so
    /// only the worker knows whether a given click is bound. A broker that
    /// tried to predict it would block forever on the first unbound click,
    /// and a broker that stopped waiting would read the *next* event's reply
    /// and stay one frame out of step from then on.
    ///
    /// Making every event owe exactly one reply removes the guess entirely:
    /// the stream is strictly one-to-one, and "nothing happened" is a thing
    /// the worker says rather than something the broker infers from silence.
    NoOp,
}

// ── Conversions ──────────────────────────────────────────────────────────────

impl From<&mizu_core::messages::WorkerResponse> for WireWorkerResponse {
    fn from(r: &mizu_core::messages::WorkerResponse) -> Self {
        let (mutated_syms, mutated_values) = r
            .state_update
            .mutated_variables
            .iter()
            .map(|(sym, val)| (sym.0, WireValue::from(val)))
            .unzip();

        let runtime_actions = r.runtime_actions.iter().map(WireRuntimeAction::from).collect();

        WireWorkerResponse {
            mutated_syms,
            mutated_values,
            runtime_actions,
            gesture: r.gesture,
        }
    }
}

impl From<&mizu_core::core::errors::MizuError> for WireWorkerError {
    fn from(e: &mizu_core::core::errors::MizuError) -> Self {
        use mizu_core::core::errors::MizuError;
        match e {
            MizuError::Timeout => WireWorkerError::Timeout,
            MizuError::SecurityViolation(msg) => {
                WireWorkerError::SecurityViolation(msg.clone())
            }
            MizuError::ExecutionError(msg) => {
                WireWorkerError::ExecutionError(msg.clone())
            }
            other => WireWorkerError::Internal(other.to_string()),
        }
    }
}
