//! End-to-end test of the Phase 4 process lifecycle: spawn → handshake →
//! request/response → EOF shutdown.
//!
//! Rather than shipping a separate fixture binary, the test re-executes its
//! own test binary. `spawn_worker` sets `MIZU_IPC_CHANNEL` in the child's
//! environment, so the child detects worker mode on entry and never reaches
//! the assertions the parent runs.

use mizu_ipc::process::{CHANNEL_NAME_ENV, connect_to_broker, spawn_worker};
use mizu_ipc::wire::{WireUiEvent, WireWorkerEnvelope, WireWorkerResponse};

/// Runs in the child: complete the handshake, answer events until the
/// broker's end of the channel closes, then exit.
fn run_as_worker() -> ! {
    let mut channel = match connect_to_broker() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("worker failed to connect: {e}");
            std::process::exit(2);
        }
    };
    // NOTE: the real `mizu-worker` applies `confine_current_process` here.
    // This fixture deliberately does not: the test asserts the *lifecycle*,
    // and dropping to Untrusted integrity mid-test-harness would break the
    // child's ability to report failures on stderr.

    loop {
        match channel.rx.recv() {
            Ok(WireUiEvent::Click { node_id }) => {
                // Echo the node id back through the gesture flag + a response
                // so the parent can prove round-trip delivery.
                let resp = WireWorkerResponse {
                    mutated_syms: vec![node_id],
                    mutated_values: vec![],
                    runtime_actions: vec![],
                    gesture: true,
                };
                if channel.tx.send(&WireWorkerEnvelope::Ok(resp)).is_err() {
                    std::process::exit(0);
                }
            }
            Ok(_) => {}
            // EOF: the broker dropped the channel. This is the normal
            // shutdown path and must exit 0, not be treated as an error.
            Err(_) => std::process::exit(0),
        }
    }
}

#[test]
fn worker_spawns_handshakes_answers_and_exits_on_eof() {
    if std::env::var_os(CHANNEL_NAME_ENV).is_some() {
        run_as_worker();
    }

    let exe = std::env::current_exe().expect("current_exe");
    // Name exactly one test in the child. Without a filter the child would
    // run every test in this binary, and each one that sees the broker
    // environment would race to claim the single-instance channel — one wins,
    // the rest fail, and the suite becomes flaky.
    let mut worker = spawn_worker(
        &exe,
        &["--exact", "worker_spawns_handshakes_answers_and_exits_on_eof"],
    )
    .expect("spawn + handshake must succeed");
    assert!(worker.id() > 0, "worker should have a real pid");

    // Round-trip an event to prove the channel carries real traffic in both
    // directions after the handshake consumed the first frame.
    worker
        .tx
        .send(&WireUiEvent::Click { node_id: 4242 })
        .expect("send must succeed");

    match worker.rx.recv().expect("worker must answer") {
        WireWorkerEnvelope::Ok(resp) => {
            assert_eq!(resp.mutated_syms, vec![4242], "payload must round-trip");
            assert!(resp.gesture);
        }
        other => panic!("expected Ok response, got {other:?}"),
    }

    // Closing the channel is the shutdown request; the worker must notice
    // EOF and exit on its own, without being killed.
    let status = worker
        .shutdown(std::time::Duration::from_secs(10))
        .expect("shutdown must reap the child");
    assert!(
        status.success(),
        "worker must exit cleanly on EOF, got {status:?}"
    );
}

#[test]
fn a_worker_that_cannot_prove_the_token_is_rejected() {
    // `connect_to_broker` refuses to run without the broker's environment,
    // which is the same check that stops a stray `mizu-worker` launched by
    // hand from doing anything at all.
    if std::env::var_os(CHANNEL_NAME_ENV).is_some() {
        run_as_worker();
    }
    let err = match connect_to_broker() {
        Err(e) => e,
        Ok(_) => panic!("connect_to_broker must refuse without the broker environment"),
    };
    assert!(
        err.to_string().contains("MIZU_IPC_CHANNEL"),
        "error should name the missing variable, got: {err}"
    );
}
