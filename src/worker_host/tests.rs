//! Tests for the live broker dispatcher.

use super::*;

#[test]
fn a_clean_exit_does_not_warrant_a_crash_page() {
    assert!(!TabCrash::CleanExit.warrants_crash_page());
    assert!(
        TabCrash::SandboxViolation {
            status: "signal: 31".to_string()
        }
        .warrants_crash_page()
    );
    assert!(
        TabCrash::Crashed {
            status: "exit: 101".to_string()
        }
        .warrants_crash_page()
    );
    assert!(TabCrash::Protocol("bad frame".to_string()).warrants_crash_page());
}

/// A sandbox kill must surface as a `SecurityViolation`, not as generic I/O.
/// Filing it under `IpcError` would bury the one signal that says a document
/// tried to escape its jail.
#[test]
fn a_sandbox_kill_is_a_security_violation() {
    let err = TabCrash::SandboxViolation {
        status: "signal: 31 (SIGSYS)".to_string(),
    }
    .into_mizu_error();
    assert!(
        matches!(err, MizuError::SecurityViolation(_)),
        "sandbox kills must be SecurityViolation, got {err:?}"
    );
    assert!(err.to_string().contains("SIGSYS"));
}

/// Other deaths are not security events and must not be reported as such —
/// crying wolf on every worker panic would make the real signal worthless.
#[test]
fn an_ordinary_crash_is_not_a_security_violation() {
    let err = TabCrash::Crashed {
        status: "exit code: 101".to_string(),
    }
    .into_mizu_error();
    assert!(
        matches!(err, MizuError::IpcError(_)),
        "a panic is not a sandbox escape, got {err:?}"
    );
}

