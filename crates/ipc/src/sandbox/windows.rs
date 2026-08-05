//! # `sandbox::windows` — Job Object confinement + Untrusted integrity level
//!
//! Two independent, additive mechanisms are applied to the calling process:
//!
//! 1. **Job Object with `JOB_OBJECT_LIMIT_ACTIVE_PROCESS = 1`**: the process
//!    is assigned to a Job Object whose active-process limit is exactly one
//!    (itself). Any subsequent `CreateProcess`/`fork`-equivalent call — from
//!    the worker directly, or from any code an exploit manages to reach —
//!    fails, because it would push the job's process count to 2.
//!    `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` additionally guarantees the
//!    process dies if the job handle is ever closed unexpectedly.
//! 2. **Untrusted mandatory integrity label** (`SECURITY_MANDATORY_UNTRUSTED_RID`,
//!    SID `S-1-16-0`) applied to the process's own primary token. Windows'
//!    mandatory integrity control then denies this process write (and, for
//!    most system objects, read) access to virtually every object on the
//!    system that wasn't explicitly labeled to accept Untrusted callers —
//!    this is the same mechanism Protected Mode IE / Chrome's legacy Windows
//!    sandbox used to strip filesystem access from a renderer process.
//!
//! ## Safety
//!
//! Every `unsafe` block below wraps exactly one Win32 call. Buffers passed
//! to the OS are always stack-allocated `#[repr(C)]` structs sized to match
//! the call, and every out-parameter is validated before use.

#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Security::{
    AllocateAndInitializeSid, FreeSid, SetTokenInformation, TokenIntegrityLevel,
    PSID, SECURITY_MANDATORY_LABEL_AUTHORITY, SID_AND_ATTRIBUTES,
    TOKEN_ADJUST_DEFAULT, TOKEN_MANDATORY_LABEL,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::SystemServices::{
    SECURITY_MANDATORY_UNTRUSTED_RID, SE_GROUP_INTEGRITY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::IpcError;

fn last_os_error(context: &str) -> IpcError {
    IpcError::Sandbox(format!("{context}: {}", std::io::Error::last_os_error()))
}

/// Applies both confinement mechanisms to the calling process, in order.
///
/// Job Object confinement is applied first (cheaper to reason about, no
/// dependency on token state); the integrity downgrade is applied last
/// because it is the more consequential, harder-to-undo step, and every
/// prior step should already have succeeded before the process's own
/// privileges are stripped.
pub fn confine_current_process() -> Result<(), IpcError> {
    restrict_to_single_process_job()?;
    lower_integrity_to_untrusted()?;
    Ok(())
}

/// Creates an anonymous Job Object limited to exactly one active process,
/// assigns the calling process to it, and leaks the job handle for the
/// remaining lifetime of the process (closing it would trigger
/// `KILL_ON_JOB_CLOSE`, which is the desired failure mode, not a bug — but
/// leaking means it only happens on process exit, which is a no-op).
fn restrict_to_single_process_job() -> Result<(), IpcError> {
    // SAFETY: no security attributes (default, not inheritable), no name
    // (anonymous job, cannot be opened by another process by name). The
    // returned HANDLE is checked for null before use.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(last_os_error("CreateJobObjectW"));
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    info.BasicLimitInformation.ActiveProcessLimit = 1;

    // SAFETY: `job` was just created and checked non-null. `info` is a
    // stack-local, correctly-sized, correctly-tagged struct matching
    // `JobObjectExtendedLimitInformation`'s expected layout.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let err = last_os_error("SetInformationJobObject");
        // SAFETY: job is a valid handle we own.
        unsafe { CloseHandle(job) };
        return Err(err);
    }

    // SAFETY: `job` is valid; `GetCurrentProcess()` returns a pseudo-handle
    // that is always valid for the calling process.
    let ok = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
    if ok == 0 {
        let err = last_os_error("AssignProcessToJobObject");
        // SAFETY: job is a valid handle we own.
        unsafe { CloseHandle(job) };
        return Err(err);
    }

    // Intentionally do not `CloseHandle(job)`: `HANDLE` is a plain pointer
    // with no destructor, so letting it fall out of scope here simply keeps
    // the OS-side handle open (and the process a member of the job) for the
    // remaining lifetime of the process — which is exactly what
    // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` requires to be a meaningful
    // guarantee rather than a no-op.
    Ok(())
}

/// Relabels the calling process's own primary token with the Untrusted
/// mandatory integrity SID (`S-1-16-0`).
///
/// Lowering your own integrity level requires only `TOKEN_ADJUST_DEFAULT`
/// access — no special privilege — but the effect is one-way: nothing short
/// of a new logon can raise it back. That asymmetry is exactly the property
/// a worker sandbox wants.
fn lower_integrity_to_untrusted() -> Result<(), IpcError> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess()` is always a valid pseudo-handle; `token`
    // is a valid out-pointer to a stack local.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_DEFAULT, &mut token) };
    if ok == 0 {
        return Err(last_os_error("OpenProcessToken"));
    }

    let mut untrusted_sid: PSID = std::ptr::null_mut();
    // SAFETY: `SECURITY_MANDATORY_LABEL_AUTHORITY` is a valid static
    // 6-byte authority; one sub-authority is requested
    // (`SECURITY_MANDATORY_UNTRUSTED_RID`); `untrusted_sid` is a valid
    // out-pointer. The SID must be freed with `FreeSid` once no longer
    // needed (done below, after `SetTokenInformation`).
    let ok = unsafe {
        AllocateAndInitializeSid(
            &SECURITY_MANDATORY_LABEL_AUTHORITY,
            1,
            SECURITY_MANDATORY_UNTRUSTED_RID as u32,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut untrusted_sid,
        )
    };
    if ok == 0 {
        let err = last_os_error("AllocateAndInitializeSid");
        // SAFETY: token is a valid handle we own.
        unsafe { CloseHandle(token) };
        return Err(err);
    }

    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: untrusted_sid,
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };

    // SAFETY: `token` was opened with `TOKEN_ADJUST_DEFAULT` above; `label`
    // is a correctly-sized, correctly-tagged struct whose `Sid` pointer
    // (`untrusted_sid`) remains valid for the duration of this call.
    let ok = unsafe {
        SetTokenInformation(
            token,
            TokenIntegrityLevel,
            &label as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
        )
    };
    let result = if ok == 0 {
        Err(last_os_error("SetTokenInformation(TokenIntegrityLevel)"))
    } else {
        Ok(())
    };

    // SAFETY: `untrusted_sid` was allocated by `AllocateAndInitializeSid`
    // above and is freed exactly once, after its last use.
    unsafe { FreeSid(untrusted_sid) };
    // SAFETY: token is a valid handle we own and no longer need.
    unsafe { CloseHandle(token) };

    result
}
