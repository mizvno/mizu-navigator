//! # `mizu-worker` — the sandboxed document-logic process
//!
//! Evaluates one tab's document logic with no access to the host: no
//! filesystem, no network, no child processes. Everything it can affect
//! leaves as a declarative [`RuntimeAction`](mizu_core::messages::RuntimeAction)
//! that the broker decides whether to honour.
//!
//! ## Startup order is load-bearing
//!
//! ```text
//! 1. connect_to_broker()          ← needs socket/pipe syscalls
//! 2. spawn the evaluation thread  ← needs clone; denied once confined
//! 3. confine_current_process()    ← denies both, forever, on all threads
//! 4. loop { recv → apply → send } ← needs only read/write on the open fd
//! ```
//!
//! Step 2 cannot move earlier: connecting requires exactly the syscalls the
//! sandbox exists to deny. It cannot move later either — every byte read
//! before it is unconfined input. So the window between "channel open" and
//! "sandbox applied" is the one place this process is both connected and
//! privileged, and nothing is read from the channel inside it.
//!
//! If confinement fails, the process exits without reading a single frame.
//! A worker that cannot prove it is jailed must never see untrusted input;
//! degrading to "run anyway, unsandboxed" would silently convert a hardened
//! browser into an ordinary one.
//!
//! ## Every frame is untrusted
//!
//! Frames are decoded through [`mizu_ipc::wire::rehydrate`], never by
//! trusting the archive's contents. `bytecheck` proves the bytes are
//! well-formed Rust; rehydration proves they are a well-formed *document*.
//!
//! ## Exit codes
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | Broker closed the channel (EOF). Normal shutdown. |
//! | 2 | Rendezvous/handshake failed — usually run outside a browser. |
//! | 3 | Sandbox refused to install. Fatal by design. |
//! | 4 | Transport failed mid-session (not EOF). |

#![forbid(unsafe_code)]

use mizu_core::parser::logic_worker::{EVALUATOR_STACK_SIZE_BYTES, TabSession};
use mizu_ipc::process::{WorkerChannel, connect_to_broker};
use mizu_ipc::wire::rehydrate::rehydrate_ui_event;
use mizu_ipc::wire::response::{WireWorkerError, WireWorkerResponse};
use mizu_ipc::wire::{WireUiEvent, WireWorkerEnvelope};

/// Broker closed the channel; this is the expected way to stop.
const EXIT_CLEAN: i32 = 0;
const EXIT_NO_BROKER: i32 = 2;
const EXIT_SANDBOX_FAILED: i32 = 3;
const EXIT_TRANSPORT: i32 = 4;

fn main() {
    // stderr is the only output this process has: the seccomp filter permits
    // writing to fd 2 precisely so a jailed worker can still explain itself.


    let channel = match connect_to_broker() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mizu-worker: {e}");
            std::process::exit(EXIT_NO_BROKER);
        }
    };

    // ── Evaluation runs on a thread with a guaranteed stack ─────────────
    // `evaluate` recurses up to `MAX_EVAL_DEPTH`, and the platform default
    // main-thread stack (~1 MiB on Windows) is not enough: the process would
    // overflow natively *before* the depth guard could intervene. The main
    // thread's stack size cannot be changed after start, so the message loop
    // moves to a thread that can ask for one.
    //
    // This thread is spawned *before* the sandbox engages, deliberately.
    // `clone`/`CreateThread` is not on the seccomp allowlist — a confined
    // worker must not be able to spawn threads — so the one thread it needs
    // has to exist first. Confinement is then applied from inside it, and on
    // Linux uses TSYNC so the main thread (parked in `join`) is covered too.
    let handle = std::thread::Builder::new()
        .name("mizu-worker-eval".to_owned())
        .stack_size(EVALUATOR_STACK_SIZE_BYTES)
        .spawn(move || confine_and_run(channel));

    let code = match handle {
        Ok(h) => h.join().unwrap_or_else(|_| {
            eprintln!("{AUDIT}evaluation thread panicked");
            EXIT_TRANSPORT
        }),
        Err(e) => {
            eprintln!("{AUDIT}cannot start evaluation thread: {e}");
            EXIT_SANDBOX_FAILED
        }
    };
    std::process::exit(code);
}

/// Engages the sandbox, then runs the message loop. Never returns unconfined.
fn confine_and_run(channel: WorkerChannel) -> i32 {
    audit_sandbox_boundary();

    #[cfg(unix)]
    let confine = mizu_ipc::confine_current_process(&channel.handles);
    #[cfg(not(unix))]
    let confine = mizu_ipc::confine_current_process(&mizu_ipc::WorkerIpcHandles {});

    if let Err(e) = confine {
        eprintln!("{AUDIT}sandbox FAILED to install: {e}");
        eprintln!("{AUDIT}refusing to process untrusted input unconfined; exiting");
        return EXIT_SANDBOX_FAILED;
    }
    eprintln!("{AUDIT}sandbox ENGAGED; entering message loop");

    run(channel)
}

/// The message loop. Returns the process exit code.
fn run(mut channel: WorkerChannel) -> i32 {
    // One session per process: the broker spawns a worker per tab, so tab
    // routing (which the in-process `LogicWorker` still does) has no meaning
    // here — the OS boundary *is* the isolation between documents.
    let mut session = TabSession::new();
    // The broker refuses pre-resolved calls from a sandboxed worker, so this
    // session must emit raw `NetworkCall`/`DownloadAlias` and let the broker
    // resolve them. Without this every network request is silently dropped.
    session.defer_alias_resolution();

    loop {
        let event = match channel.rx.recv() {
            Ok(e) => e,
            Err(e) => return exit_code_for_recv_failure(&e),
        };

        // `CloseTab` means this document is done, and since a worker serves
        // exactly one document, that is the whole process.
        if matches!(event, WireUiEvent::CloseTab) {
            return EXIT_CLEAN;
        }

        let envelope = match rehydrate_ui_event(&event) {
            Ok(core_event) => match session.apply_event(core_event) {
                // The event addressed nothing (unbound click, timer index
                // past the end). Unlike the in-process worker — which can
                // simply stay silent — this must still answer, because the
                // broker cannot tell "nothing was bound" from "still
                // working" without a frame. See `WireWorkerEnvelope::NoOp`.
                None => WireWorkerEnvelope::NoOp,
                Some(Ok(response)) => {
                    WireWorkerEnvelope::Ok(WireWorkerResponse::from(&response))
                }
                Some(Err(e)) => WireWorkerEnvelope::Err(WireWorkerError::from(&e)),
            },
            // A frame that survived `bytecheck` but not Mizu's own invariants.
            // Reported rather than acted on, and the loop continues: one bad
            // frame is not a reason to drop a working channel.
            Err(e) => {
                tracing::warn!(error = %e, "rejected malformed event frame");
                WireWorkerEnvelope::Err(WireWorkerError::Internal(e.to_string()))
            }
        };

        if let Err(e) = channel.tx.send(&envelope) {
            return exit_code_for_recv_failure(&e);
        }
    }
}

/// EOF is the shutdown protocol, not a failure.
///
/// The broker closes the channel to ask this process to stop — on tab close,
/// on browser exit, and on broker crash alike. All three arrive here as
/// [`mizu_ipc::IpcError::WorkerDied`] (the framer's name for "read returned
/// zero bytes"), and all three mean the same thing: there is no one left to
/// talk to, so exit cleanly.
fn exit_code_for_recv_failure(e: &mizu_ipc::IpcError) -> i32 {
    match e {
        mizu_ipc::IpcError::WorkerDied(_) => EXIT_CLEAN,
        other => {
            eprintln!("mizu-worker: transport failure: {other}");
            EXIT_TRANSPORT
        }
    }
}

/// Prefix marking every line of the sandbox audit trail.
///
/// Grep-able on purpose: under `strace`/`dtruss` these lines interleave with
/// thousands of syscalls, and the whole point is to find them instantly.
const AUDIT: &str = "[mizu-worker/sandbox] ";

/// Emits the audit trail QA needs to debug a `SIGSYS` on real hardware.
///
/// # Why `eprintln!` and not `tracing::info!`
///
/// `tracing` macros are no-ops without an installed subscriber, and this
/// process deliberately installs none: pulling a subscriber stack into the
/// jail would widen both its dependency tree and its syscall surface for log
/// formatting alone — and *this specific message* has to survive in exactly
/// the situation where the syscall surface is the thing under suspicion.
/// `write(2)` to stderr is one syscall, explicitly allowed by the seccomp
/// filter for this reason, and unbuffered so nothing is lost when the process
/// is killed mid-line.
///
/// # Reading the trail
///
/// Three outcomes, distinguishable by which lines appear:
///
/// * `about to engage` then nothing → the sandbox install itself died. On
///   Linux that usually means `seccomp`/`PR_SET_NO_NEW_PRIVS` was refused by
///   the kernel; run under `strace -f -e trace=prctl,seccomp`.
/// * `about to engage` then `FAILED` → installation returned an error
///   cleanly; the message names the cause and the process exited 3.
/// * `ENGAGED` then a kill later → a syscall in the message loop is missing
///   from the allowlist. This is the interesting case: `strace -f` and look
///   at the **last** syscall before `SIGSYS`, then add it to
///   `mizu_ipc::sandbox::linux`'s allowlist with a justification.
///
/// The allowlist is deliberately narrow, so this third case is expected to
/// happen at least once per platform/toolchain combination. It is a tuning
/// exercise, not necessarily a bug.
fn audit_sandbox_boundary() {
    eprintln!("{AUDIT}pid={} about to engage OS confinement", std::process::id());
    eprintln!(
        "{AUDIT}platform={} arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    #[cfg(target_os = "linux")]
    eprintln!(
        "{AUDIT}mechanism=seccomp-bpf (default action: KILL_PROCESS) — a \
         missing syscall presents as SIGSYS; trace with: strace -f -p {}",
        std::process::id()
    );
    #[cfg(target_os = "windows")]
    eprintln!(
        "{AUDIT}mechanism=JobObject(ActiveProcessLimit=1) + Untrusted \
         integrity token"
    );
    #[cfg(target_os = "macos")]
    eprintln!(
        "{AUDIT}mechanism=sandbox_init (deny default) — UNVERIFIED on real \
         hardware; trace with: sudo dtruss -p {}",
        std::process::id()
    );
}
