//! # `sandbox::macos` — App Sandbox confinement via `sandbox_init`
//!
//! Applies a deny-by-default Sandbox Profile Language (SBPL) profile to the
//! calling process using the private (but ABI-stable since Mac OS X 10.5)
//! `libsystem_sandbox` entry point `sandbox_init`. This is the same
//! mechanism `sandbox-exec(1)` uses under the hood; calling it in-process
//! avoids spawning a wrapper process and lets the worker apply its own
//! confinement immediately before entering its message loop, exactly as on
//! Linux and Windows.
//!
//! ## Profile
//!
//! ```text
//! (version 1)
//! (deny default)
//! (deny network*)
//! (deny file-read* file-write*)
//! (allow file-read-metadata)
//! (allow signal (target self))
//! ```
//!
//! `(deny default)` denies every operation class not explicitly allowed —
//! including process creation (`process-fork`/`process-exec`), Mach IPC
//! (`mach-lookup`), IOKit device access, and of course `network*` and
//! `file-read*`/`file-write*`, which are also denied explicitly for defense
//! in depth (a future SBPL default change should not silently re-open them).
//! `file-read-metadata` (stat, not read) and `signal (target self)` are the
//! only allowances, both required for a stock Rust binary to keep running
//! normally (the runtime stats its own binary path on some code paths; the
//! process must be able to signal itself, e.g. for `SIGABRT` on panic).
//!
//! The IPC socket's `read`/`write` are unaffected: `sandbox_init` operates
//! on *new* resource acquisition (opening files, connecting sockets), not on
//! already-open file descriptors, so the worker's existing IPC connection
//! keeps working after this profile is applied.
//!
//! ## ⚠️ Unverified on real hardware
//!
//! This module type-checks against the `x86_64-apple-darwin` target but has
//! not been exercised on macOS — this repository is developed and CI'd from
//! non-macOS hosts. Before relying on it, confirm on real macOS that (a)
//! `sandbox_init` returns `0`, and (b) the worker can still service the IPC
//! loop after the profile is applied (Rust's panic path in particular should
//! be exercised, since it touches `file-read-metadata`-adjacent paths on
//! some OS versions).
//!
//! ## Safety
//!
//! `sandbox_init` is FFI into `libsystem_sandbox.dylib` (not exposed by the
//! `libc` crate, so declared here directly). The profile string is a
//! `'static` byte literal (never attacker-influenced), and the `errorbuf`
//! out-parameter — allocated by the OS — is freed with `sandbox_free_error`
//! exactly once, only when `sandbox_init` reports failure.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, CStr, CString};

use crate::error::IpcError;

const PROFILE: &str = "\
(version 1)\n\
(deny default)\n\
(deny network*)\n\
(deny file-read* file-write*)\n\
(allow file-read-metadata)\n\
(allow signal (target self))\n\
";

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    /// `int sandbox_init(const char *profile, uint64_t flags, char **errorbuf);`
    ///
    /// Returns `0` on success. On failure returns a negative value and, if
    /// `errorbuf` is non-null, writes a `sandbox_free_error`-owned
    /// diagnostic string to `*errorbuf`.
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;

    /// Frees a diagnostic string produced by `sandbox_init`.
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// `SANDBOX_NAMED`: unset, since we pass a literal SBPL profile string
/// rather than the name of a built-in profile.
const SANDBOX_PROFILE_IS_LITERAL: u64 = 0;

/// Installs the deny-by-default profile documented in the module docs on
/// the calling process.
///
/// # Errors
///
/// Returns [`IpcError::Sandbox`] if `sandbox_init` reports failure (e.g. the
/// profile fails to parse, or the calling process lacks the entitlement
/// required to self-sandbox on some macOS versions). This must be treated
/// as fatal by the caller.
pub fn apply_sandbox_profile() -> Result<(), IpcError> {
    let profile =
        CString::new(PROFILE).expect("static profile string contains no interior NUL");

    let mut errorbuf: *mut c_char = std::ptr::null_mut();
    // SAFETY: `profile` is a valid, NUL-terminated, `'static`-derived
    // C string kept alive for the duration of the call; `errorbuf` is a
    // valid out-pointer to a stack local, initialized to null.
    let rc = unsafe { sandbox_init(profile.as_ptr(), SANDBOX_PROFILE_IS_LITERAL, &mut errorbuf) };

    if rc == 0 {
        return Ok(());
    }

    let message = if errorbuf.is_null() {
        format!("sandbox_init failed with code {rc} (no diagnostic message)")
    } else {
        // SAFETY: `errorbuf` was set to a valid, NUL-terminated string by
        // `sandbox_init` (checked non-null above); it remains valid until
        // freed by `sandbox_free_error` below.
        let msg = unsafe { CStr::from_ptr(errorbuf) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `errorbuf` is non-null and was allocated by
        // `sandbox_init`; freed exactly once, after its only use above.
        unsafe { sandbox_free_error(errorbuf) };
        msg
    };

    Err(IpcError::Sandbox(format!("sandbox_init: {message}")))
}
