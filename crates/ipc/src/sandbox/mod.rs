//! # `sandbox` — Phase 2: OS-level confinement of the `mizu-worker` process
//!
//! Once the worker has the two IPC endpoints it needs (its read/write stream
//! to the broker), it calls [`confine_current_process`] exactly once, right
//! before entering the message loop. After that call returns `Ok(())`, the
//! process can no longer open files, open sockets, spawn children, or exec —
//! its entire remaining lifetime is `read`/`write` on the pre-opened IPC
//! handle plus whatever the platform's synchronization primitives require.
//!
//! ## Platform coverage
//!
//! | Platform | Mechanism | Module |
//! |---|---|---|
//! | Linux    | `seccomp-bpf` syscall allowlist, `SECCOMP_RET_KILL_PROCESS` default | [`linux`] |
//! | Windows  | Job Object (no child processes) + Untrusted integrity level token | [`windows`] |
//! | macOS    | `sandbox_init` with a deny-by-default profile | [`macos`] |
//!
//! ## Why this lives in `mizu-ipc`, not `mizu-worker`
//!
//! The confinement policy is intimately tied to the IPC transport (it must
//! whitelist exactly the fds/handles the transport uses), so it is defined
//! next to the transport code. The `mizu-worker` binary calls
//! [`confine_current_process`] with the handles its `IpcSender`/`IpcReceiver`
//! were constructed from.
//!
//! ## Safety boundary
//!
//! Every raw syscall / Win32 FFI call needed to install these sandboxes
//! lives inside the platform submodule ([`linux`], [`windows`], [`macos`]).
//! This module and its public entry point are safe.

use crate::error::IpcError;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// The IPC endpoints the worker must retain access to after sandboxing.
///
/// On Linux these are the raw file descriptors of the socket (or its split
/// halves); on Windows/macOS the mechanism doesn't need to enumerate handles
/// individually (Job Objects and App Sandbox profiles work at a coarser
/// granularity), but the fields are still accepted for API symmetry and
/// future tightening.
#[derive(Debug, Clone, Copy)]
pub struct WorkerIpcHandles {
    /// Raw fd/handle the worker reads events from.
    #[cfg(unix)]
    pub read_fd: std::os::unix::io::RawFd,
    /// Raw fd/handle the worker writes responses to.
    #[cfg(unix)]
    pub write_fd: std::os::unix::io::RawFd,
}

/// Install the platform's worker sandbox on the *calling* process/thread.
///
/// Must be called after all IPC handles are open and before any untrusted
/// document logic is evaluated. Irreversible: there is no API to lift the
/// sandbox once applied, by design.
///
/// # Errors
///
/// Returns [`IpcError::Sandbox`] if the underlying OS mechanism could not be
/// installed. The caller (the worker's `main`) must treat this as fatal and
/// exit without processing any events — a worker that cannot confirm its own
/// confinement must never see untrusted input.
pub fn confine_current_process(handles: &WorkerIpcHandles) -> Result<(), IpcError> {
    #[cfg(target_os = "linux")]
    {
        return linux::apply_seccomp_filter(handles.read_fd, handles.write_fd);
    }

    #[cfg(target_os = "windows")]
    {
        let _ = handles;
        return windows::confine_current_process();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = handles;
        return macos::apply_sandbox_profile();
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = handles;
        Err(IpcError::Sandbox(
            "no worker sandbox implementation for this platform".to_string(),
        ))
    }
}
