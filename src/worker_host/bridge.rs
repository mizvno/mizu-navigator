//! # `bridge` — the asynchronous adapter between the UI thread and workers
//!
//! [`WorkerHost::dispatch_event`](super::WorkerHost::dispatch_event) is
//! synchronous, which is right for tests and wrong for a browser: blocking
//! the UI thread on a cross-process round trip would freeze the window on
//! every click. This module restores the asynchrony the window manager
//! already expects, without changing a single one of its twelve call sites.
//!
//! ## Shape preservation is the whole trick
//!
//! The manager posts events with `Sender<(TabId, UiEvent)>` and drains
//! replies with `Receiver<(TabId, Result<WorkerResponse, MizuError>)>`,
//! non-blocking, in the idle loop. [`spawn_router`] takes exactly those two
//! endpoints and satisfies them with worker processes instead of a worker
//! thread. From the manager's side nothing about the contract changes — which
//! is why the swap touches only the instantiation site.
//!
//! ```text
//!  UI thread                router thread            reader thread(s)
//!  ─────────                ─────────────            ────────────────
//!  logic_tx.send ──────────▶ recv (TabId, UiEvent)
//!                            push agency to FIFO
//!                            IpcSender::send ─────────────▶ [worker process]
//!
//!  logic_rx.try_recv ◀───────────────────────────────── IpcReceiver::recv
//!    (unchanged drain)                                  pop FIFO, authorize
//! ```
//!
//! ## Why authorization happens on the reader thread
//!
//! The reply must be authorized before the idle loop executes its actions,
//! and the idle loop's drain is code we are deliberately not touching. So
//! the reader thread does it, and hands over a `WorkerResponse` whose
//! `runtime_actions` are *already* the approved set — a `NetworkCall` has
//! become a broker-resolved `ResolvedCall`, and anything refused is gone.
//! Downstream `execute_capability_action` then runs exactly what the broker
//! sanctioned, with no further changes.
//!
//! The policy state it needs (this document's `UrlRegistry` and domain) is
//! shared under one mutex with the FIFO, and updated by the router when it
//! forwards a `Reload` — the same event that changes the document.
//!
//! ## Correlating replies with events
//!
//! Gate G1 needs to know which event produced an action. Since the Phase 6
//! protocol guarantees exactly one reply per event, in order, a FIFO of
//! [`EventAgency`] values is enough: reply *N* belongs to event *N*. That
//! guarantee is what [`mizu_ipc::wire::WireWorkerEnvelope::NoOp`] exists to
//! provide — without it an unbound click would send nothing, every later
//! reply would pair with the wrong event, and a timer's action could inherit
//! a click's agency. The strictness of the protocol *is* the security
//! property here.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use mizu_core::core::errors::MizuError;
use mizu_core::messages::{TabId, UiEvent, WorkerResponse};
use mizu_core::parser::UrlRegistry;
use mizu_ipc::process::spawn_worker;
use mizu_ipc::wire::rehydrate::rehydrate_worker_response;
use mizu_ipc::wire::{WireUiEvent, WireWorkerEnvelope};

use crate::render::security::broker::{ActionOrigin, EventAgency, authorize_action};

use super::TabCrash;

/// Per-tab state shared between the router thread (which writes) and that
/// tab's reader thread (which reads).
///
/// One mutex covers both the FIFO and the policy because they are updated
/// together and read together: a `Reload` simultaneously enqueues an agency
/// and replaces the registry, and a reply needs both to be authorized.
/// Splitting them would admit a window where a reply is judged against the
/// previous document's registry.
struct TabPolicy {
    /// Agency of each event sent but not yet answered, oldest first.
    pending: VecDeque<EventAgency>,
    /// The broker's own copy of the current document's endpoint table.
    url_registry: UrlRegistry,
    /// The current document's domain.
    document_domain: String,
}

/// Locates the `mizu-worker` binary that ships beside this executable.
///
/// Deliberately not a `PATH` lookup: the worker is the thing we are about to
/// hand untrusted documents to, so resolving it through a
/// user-influenced search path would be an obvious substitution hole.
pub fn worker_executable_path() -> Result<std::path::PathBuf, MizuError> {
    let mut dir = std::env::current_exe()
        .map_err(|e| MizuError::IpcError(format!("cannot locate own executable: {e}")))?;
    dir.pop();
    let exe = dir.join(format!("mizu-worker{}", std::env::consts::EXE_SUFFIX));
    if exe.exists() {
        return Ok(exe);
    }

    // Test builds only: `cargo test` puts test binaries in `target/debug/deps/`
    // while the worker builds to `target/debug/`, one level up. Gating this on
    // `cfg(test)` keeps the production lookup strict — a shipped browser never
    // searches outside its own directory, so a binary planted in the parent
    // cannot be substituted for the real worker.
    #[cfg(test)]
    if dir.ends_with("deps") {
        let mut up = dir.clone();
        up.pop();
        let candidate = up.join(format!("mizu-worker{}", std::env::consts::EXE_SUFFIX));
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(MizuError::IpcError(format!(
        "mizu-worker not found next to the browser executable (looked in {})",
        dir.display()
    )))
}

/// Starts the router thread that backs `logic_tx` / `logic_rx` with sandboxed
/// worker processes.
///
/// Signature-compatible with `LogicWorker::spawn`, so swapping between them
/// is a one-line change at the construction site.
///
/// Workers are spawned **lazily**, on the first event addressed to a tab, and
/// dropped on `CloseTab`. That keeps tab lifecycle entirely inside this
/// module: the window manager never has to learn that a tab now owns a
/// process.
pub fn spawn_router(
    rx: Receiver<(TabId, UiEvent)>,
    tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
) -> Result<std::thread::JoinHandle<()>, MizuError> {
    spawn_router_with_exe(rx, tx, worker_executable_path()?)
}

/// [`spawn_router`] with an explicit worker binary.
///
/// Exists because [`worker_executable_path`] is deliberately strict — it only
/// looks *directly* beside the running executable, so a planted binary one
/// directory up cannot be substituted for the real worker. Cargo puts test
/// binaries in `target/debug/deps/` while the worker builds to
/// `target/debug/`, so tests have to say where it is rather than have
/// production widen its search to accommodate them.
pub fn spawn_router_with_exe(
    rx: Receiver<(TabId, UiEvent)>,
    tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    worker_exe: std::path::PathBuf,
) -> Result<std::thread::JoinHandle<()>, MizuError> {
    std::thread::Builder::new()
        .name("mizu-worker-router".to_owned())
        .spawn(move || router_loop(rx, tx, worker_exe))
        .map_err(|e| MizuError::IpcError(format!("cannot start worker router: {e}")))
}

/// One live worker: its sender, its shared policy, and its reader thread.
struct LiveWorker {
    tx: mizu_ipc::IpcSender<WireUiEvent>,
    policy: Arc<Mutex<TabPolicy>>,
    /// Kept so dropping this struct reaps the process; never read.
    _guard: mizu_ipc::process::ChildGuard,
}

fn router_loop(
    rx: Receiver<(TabId, UiEvent)>,
    tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
    worker_exe: std::path::PathBuf,
) {
    let mut workers: HashMap<TabId, LiveWorker> = HashMap::new();

    while let Ok((tab_id, event)) = rx.recv() {
        // `CloseTab` destroys the worker. Dropping `LiveWorker` closes the
        // channel, the worker sees EOF and exits 0, and `ChildGuard` reaps
        // it — the same teardown path a browser exit takes.
        if matches!(event, UiEvent::CloseTab) {
            workers.remove(&tab_id);
            continue;
        }

        // A tab's worker is created on its first event, which in practice is
        // always the `Reload` that loads its document.
        let worker = match workers.entry(tab_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                match start_worker(&worker_exe, tab_id, tx.clone()) {
                    Ok(w) => slot.insert(w),
                    Err(e) => {
                        // The tab cannot run at all. Report it once, as a
                        // crash, so the manager can paint a crash page
                        // rather than showing a silently inert document.
                        let _ = tx.send((tab_id, Err(e.into_mizu_error())));
                        continue;
                    }
                }
            }
        };

        // Record agency *before* sending: the reader thread may answer
        // before this thread runs again, and it must find the FIFO populated.
        {
            let mut policy = lock(&worker.policy);
            policy.pending.push_back(EventAgency::of(&event));
            // A reload replaces the document, and with it the registry every
            // later reply is judged against.
            if let UiEvent::Reload(payload) = &event {
                policy.url_registry = payload.url_registry.clone();
                policy.document_domain = payload.document_domain.clone();
            }
        }

        if let Err(e) = worker.tx.send(&WireUiEvent::from(&event)) {
            tracing::warn!(tab = tab_id.0, error = %e, "worker channel died on send");
            workers.remove(&tab_id);
            let _ = tx.send((
                tab_id,
                Err(TabCrash::Protocol(format!("send to worker failed: {e}")).into_mizu_error()),
            ));
        }
    }
}

/// Spawns one worker and its reader thread.
fn start_worker(
    worker_exe: &std::path::Path,
    tab_id: TabId,
    ui_tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
) -> Result<LiveWorker, TabCrash> {
    let process = spawn_worker(worker_exe, &[])
        .map_err(|e| TabCrash::Protocol(format!("failed to start worker: {e}")))?;
    let (tx, rx, guard) = process.into_parts();

    let policy = Arc::new(Mutex::new(TabPolicy {
        pending: VecDeque::new(),
        url_registry: UrlRegistry::default(),
        document_domain: String::new(),
    }));

    let reader_policy = Arc::clone(&policy);
    std::thread::Builder::new()
        .name(format!("mizu-worker-reader-{}", tab_id.0))
        .spawn(move || reader_loop(rx, tab_id, reader_policy, ui_tx))
        .map_err(|e| TabCrash::Protocol(format!("cannot start reader thread: {e}")))?;

    Ok(LiveWorker {
        tx,
        policy,
        _guard: guard,
    })
}

/// Blocking `recv` loop, one per worker, off the UI thread.
fn reader_loop(
    mut rx: mizu_ipc::IpcReceiver<WireWorkerEnvelope>,
    tab_id: TabId,
    policy: Arc<Mutex<TabPolicy>>,
    ui_tx: Sender<(TabId, Result<WorkerResponse, MizuError>)>,
) {
    loop {
        let envelope = match rx.recv() {
            Ok(e) => e,
            Err(e) => {
                // EOF is the normal end of a worker's life (tab closed, or
                // the router dropped the channel). Anything else is worth
                // surfacing, but neither case can distinguish a sandbox kill
                // from here — only the router holds the `ChildGuard` needed
                // to read the exit status — so this reports a transport
                // failure and lets the process teardown speak for itself.
                if !matches!(e, mizu_ipc::IpcError::WorkerDied(_)) {
                    let _ = ui_tx.send((
                        tab_id,
                        Err(TabCrash::Protocol(format!("worker channel failed: {e}"))
                            .into_mizu_error()),
                    ));
                }
                return;
            }
        };

        // Every reply consumes exactly one queued event, including `NoOp`.
        // Skipping the pop for `NoOp` would desynchronize the FIFO and let a
        // later action inherit the wrong agency.
        let (agency, registry, domain) = {
            let mut p = lock(&policy);
            let agency = p.pending.pop_front();
            (agency, p.url_registry.clone(), p.document_domain.clone())
        };
        let Some(agency) = agency else {
            // A reply with nothing queued means the worker sent an
            // unsolicited frame — a protocol violation, not a stray event.
            let _ = ui_tx.send((
                tab_id,
                Err(TabCrash::Protocol(
                    "worker sent a reply with no event outstanding".to_string(),
                )
                .into_mizu_error()),
            ));
            return;
        };

        let message = match envelope {
            // Nothing was bound; there is no state update to deliver, and
            // the manager must not be woken for it.
            WireWorkerEnvelope::NoOp => continue,
            WireWorkerEnvelope::Err(e) => {
                Err(MizuError::ExecutionError(format!("{e:?}")))
            }
            WireWorkerEnvelope::Hello { .. } => Err(TabCrash::Protocol(
                "worker replayed the Hello handshake frame mid-session".to_string(),
            )
            .into_mizu_error()),
            WireWorkerEnvelope::Ok(wire) => match rehydrate_worker_response(&wire) {
                Err(e) => Err(TabCrash::Protocol(format!("malformed worker response: {e}"))
                    .into_mizu_error()),
                Ok(mut response) => {
                    response.runtime_actions =
                        authorize_all(&response, agency, &registry, &domain, tab_id);
                    // The worker's own `gesture` claim is replaced by the
                    // broker's verdict. Downstream code (the navigation choke
                    // point) reads this field, and it must reflect what the
                    // broker decided, not what the worker asserted.
                    response.gesture = agency.is_user_gesture();
                    Ok(response)
                }
            },
        };

        if ui_tx.send((tab_id, message)).is_err() {
            // The manager is gone; nothing left to report to.
            return;
        }
    }
}

/// Runs every action through the Phase 3 gate, keeping the approved ones.
fn authorize_all(
    response: &WorkerResponse,
    agency: EventAgency,
    registry: &UrlRegistry,
    domain: &str,
    tab_id: TabId,
) -> Vec<mizu_core::messages::RuntimeAction> {
    let mut approved = Vec::with_capacity(response.runtime_actions.len());
    for action in response.runtime_actions.iter().cloned() {
        match authorize_action(
            action,
            ActionOrigin::SandboxedIpcWorker,
            domain,
            registry,
            agency,
        ) {
            Ok(a) => approved.push(a),
            // Refusals are logged rather than delivered: the existing drain
            // has no channel for "this was denied", and inventing one would
            // mean touching the idle loop this bridge exists to leave alone.
            // `error!`, not `warn!`, and deliberately so. The browser
            // installs `EnvFilter::from_default_env()`, which with no
            // `RUST_LOG` set shows `ERROR` and nothing else — so a refusal
            // logged at `warn` is invisible in a normal run. That is exactly
            // how the cutover shipped with every network request silently
            // discarded: the symptom was "requests do not work" with no
            // diagnostic anywhere. A refusal is either a security event or a
            // broker bug, and both must be visible without opting in.
            Err(e) => {
                tracing::error!(
                    tab = tab_id.0,
                    error = %e,
                    "broker refused an action from the worker; the document's \
                     request was NOT executed"
                );
            }
        }
    }
    approved
}

/// A poisoned lock here guards a `VecDeque` and two plain clones — there is
/// no invariant a panicking holder could have left half-applied — so the
/// inner value is recovered rather than propagating the panic into an
/// unrelated tab. Mirrors `StorageUsageLedger`'s handling of the same case.
fn lock(m: &Mutex<TabPolicy>) -> std::sync::MutexGuard<'_, TabPolicy> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests;
