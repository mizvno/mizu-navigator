//! Integration test for `mizu_ipc::sandbox::windows`.
//!
//! Confinement is irreversible and process-wide, so it cannot be applied
//! inside the shared test-binary process (it would break every other test
//! running there). Instead this test re-executes itself in a child process
//! with an environment flag; the child applies the sandbox to itself, then
//! proves confinement actually took effect by attempting to spawn a
//! subprocess (must fail — `JOB_OBJECT_LIMIT_ACTIVE_PROCESS = 1`) and exits
//! with a status code the parent asserts on.

#![cfg(windows)]

const CHILD_ENV: &str = "MIZU_IPC_SANDBOX_CHILD";

#[test]
fn confine_current_process_blocks_child_process_creation() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_as_child();
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg("confine_current_process_blocks_child_process_creation")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .status()
        .expect("failed to spawn child test process");

    assert!(
        status.success(),
        "sandboxed child did not confirm confinement (status: {status:?})"
    );
}

/// Runs inside the child process spawned above. Applies the sandbox to
/// itself, then attempts to spawn a trivial subprocess: under
/// `JOB_OBJECT_LIMIT_ACTIVE_PROCESS = 1`, that spawn must fail because it
/// would raise the job's active-process count above the limit.
///
/// Exits the process directly (rather than returning / panicking) because
/// this code path never reaches the normal `#[test]` epilogue — the parent
/// only cares about the exit code.
fn run_as_child() {
    if let Err(e) = mizu_ipc::sandbox::windows::confine_current_process() {
        eprintln!("confinement failed to install: {e}");
        std::process::exit(2);
    }

    let spawn_result = std::process::Command::new("cmd.exe")
        .args(["/C", "exit", "0"])
        .status();

    match spawn_result {
        Ok(status) => {
            eprintln!(
                "expected child process spawn to be blocked by the job object, \
                 but it ran to completion with status {status:?}"
            );
            std::process::exit(3);
        }
        Err(e) => {
            // This is the expected outcome: CreateProcess failed because the
            // job's active-process limit (1, this process) was already
            // reached.
            eprintln!("child process spawn correctly blocked: {e}");
            std::process::exit(0);
        }
    }
}
