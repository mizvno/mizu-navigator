//! # `sandbox::linux` — `seccomp-bpf` allowlist for the worker process
//!
//! Installs a microscopic syscall allowlist via [`seccompiler`]:
//!
//! | Group | Syscalls | Restriction |
//! |---|---|---|
//! | IPC I/O | `read`, `readv` | argument 0 (fd) must equal the IPC read fd |
//! | IPC I/O | `write`, `writev` | argument 0 (fd) must equal the IPC write fd, or stderr |
//! | Memory | `mmap`, `munmap`, `mremap`, `mprotect`, `madvise`, `brk` | unrestricted |
//! | Signals | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `sigaltstack` | unrestricted |
//! | Sync | `futex`, `sched_yield`, `restart_syscall` | unrestricted |
//! | Entropy | `getrandom` | unrestricted |
//! | Clock | `clock_gettime` | unrestricted |
//! | Readiness | `epoll_wait`/`epoll_pwait`, `poll`/`ppoll` | unrestricted |
//! | Exit | `close`, `exit`, `exit_group` | unrestricted |
//!
//! ## Why this is wider than "read/write/futex/epoll/exit"
//!
//! The original Phase 2 specification named a five-syscall allowlist. That
//! set cannot survive contact with a real Rust process: the allocator needs
//! `mmap`/`brk` the moment rkyv deserialization allocates, `HashMap`'s
//! default hasher calls `getrandom` on first construction, and the panic
//! handler needs `sigaltstack`/`rt_sigreturn`. A worker under the literal
//! five-syscall filter is killed by `SECCOMP_RET_KILL_PROCESS` on its first
//! message, which looks exactly like a successful sandbox and is in fact a
//! crash loop.
//!
//! Every syscall added above is one a computation-only process genuinely
//! needs, and none of them can name a resource outside the process: with
//! `open`/`openat`/`socket` denied there is no fd for `mmap` to map, so it
//! is reachable only as anonymous memory.
//!
//! Every other syscall — including `open`, `openat`, `socket`, `execve`,
//! `clone`, and `fork` — hits the filter's default (mismatch) action:
//! [`SeccompAction::KillProcess`]. This is deliberately harsher than
//! `KillThread` or `Errno`: a worker that reaches for a forbidden syscall is
//! treated as compromised, not merely buggy, so the entire process is torn
//! down rather than left running with one thread silently killed.
//!
//! ## Why not `read`/`write` unconditionally
//!
//! Restricting the fd argument means a worker that has somehow acquired a
//! second fd (e.g. via a bug in a dependency that leaks one) still cannot
//! read or write through it — only the two fds wired up before the filter
//! was installed are usable at all.
//!
//! ## Safety
//!
//! [`seccompiler::apply_filter`] is itself a safe function (it encapsulates
//! the `prctl`/`seccomp` FFI internally); this module contains no `unsafe`
//! code of its own, but is still kept in `sandbox::linux` — not the shared
//! `sandbox` module — because it is the only place permitted to reach for
//! the raw `libc` syscall-number constants that drive an irreversible,
//! process-wide security decision.

use std::collections::BTreeMap;

use seccompiler::{
    apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen as ArgLen, SeccompCmpOp as Op,
    SeccompCondition as Cond, SeccompFilter, SeccompRule, TargetArch,
};

use crate::error::IpcError;

fn to_ipc_err(e: impl std::fmt::Display) -> IpcError {
    IpcError::Sandbox(e.to_string())
}

fn target_arch() -> Result<TargetArch, IpcError> {
    std::env::consts::ARCH
        .try_into()
        .map_err(|_| IpcError::Sandbox(format!("unsupported seccomp arch: {}", std::env::consts::ARCH)))
}

/// Installs the syscall allowlist described in the module docs and applies
/// Installs the syscall allowlist described in the module docs and applies
/// it to **every thread** in the process (`SECCOMP_FILTER_FLAG_TSYNC`).
///
/// TSYNC rather than the calling thread alone: the worker spawns its
/// evaluation thread before confining (it cannot after — `clone` is denied),
/// so a thread-local filter would leave the main thread unconfined.
///
/// # Errors
///
/// Returns [`IpcError::Sandbox`] if the filter cannot be constructed
/// (malformed rule — a programmer error in this module) or if the kernel
/// rejects installation (e.g. `CONFIG_SECCOMP` unavailable, or
/// `PR_SET_NO_NEW_PRIVS` denied). Both are fatal: the caller must not
/// proceed to process untrusted input without confirmed confinement.
pub fn apply_seccomp_filter(read_fd: i32, write_fd: i32) -> Result<(), IpcError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // ── fd-restricted I/O ────────────────────────────────────────────────
    // Only the two IPC endpoints are readable/writable. A leaked third fd is
    // useless to the worker because the fd number itself is the gate.
    // `writev` is included because Rust's `std::io::Write` implementations
    // reach for vectored writes on buffered flushes.
    let fd_rule = |fd: i32| -> Result<SeccompRule, IpcError> {
        SeccompRule::new(vec![
            Cond::new(0, ArgLen::Dword, Op::Eq, fd as u64).map_err(to_ipc_err)?,
        ])
        .map_err(to_ipc_err)
    };

    rules.insert(libc::SYS_read, vec![fd_rule(read_fd)?]);
    rules.insert(libc::SYS_readv, vec![fd_rule(read_fd)?]);
    // stderr is writable in addition to the IPC endpoint: without it a panic
    // in the worker is silent, and a sandbox you cannot debug is a sandbox
    // that gets turned off. stderr is inherited from the broker (our own
    // process), so this is not an exfiltration channel to anywhere new.
    rules.insert(
        libc::SYS_write,
        vec![fd_rule(write_fd)?, fd_rule(libc::STDERR_FILENO)?],
    );
    rules.insert(
        libc::SYS_writev,
        vec![fd_rule(write_fd)?, fd_rule(libc::STDERR_FILENO)?],
    );

    // ── memory management ────────────────────────────────────────────────
    // Unconditional. Every one of these is reachable from a plain `Vec::push`
    // once the allocator needs to grow its arena — rkyv deserialization
    // allocates, so denying these kills the worker on its first real message.
    // None of them can name a resource outside the process: `mmap` here is
    // reachable only as anonymous memory because `open`/`openat` are denied,
    // so there is no fd to map.
    rules.insert(libc::SYS_mmap, vec![]);
    rules.insert(libc::SYS_munmap, vec![]);
    rules.insert(libc::SYS_mremap, vec![]);
    rules.insert(libc::SYS_mprotect, vec![]);
    rules.insert(libc::SYS_madvise, vec![]);
    rules.insert(libc::SYS_brk, vec![]);

    // ── signals ──────────────────────────────────────────────────────────
    // Required for Rust's panic path (and its stack-overflow guard page
    // handler) to run at all. Without `sigaltstack`/`rt_sigreturn` a panic
    // becomes an opaque `KILL_PROCESS` indistinguishable from a sandbox
    // violation, which would make every real bug look like an attack.
    rules.insert(libc::SYS_rt_sigaction, vec![]);
    rules.insert(libc::SYS_rt_sigprocmask, vec![]);
    rules.insert(libc::SYS_rt_sigreturn, vec![]);
    rules.insert(libc::SYS_sigaltstack, vec![]);

    // ── synchronization & scheduling ─────────────────────────────────────
    rules.insert(libc::SYS_futex, vec![]);
    rules.insert(libc::SYS_sched_yield, vec![]);
    rules.insert(libc::SYS_restart_syscall, vec![]);

    // ── entropy ──────────────────────────────────────────────────────────
    // `getrandom` is not optional: `std::collections::HashMap`'s default
    // `RandomState` seeds itself from it on first construction, and the
    // rehydration path builds HashMaps. Denying it kills the worker the
    // first time it rebuilds a document.
    rules.insert(libc::SYS_getrandom, vec![]);

    // ── clocks ───────────────────────────────────────────────────────────
    // Normally serviced by the vDSO without entering the kernel, but that is
    // a performance optimization the kernel is free to skip (and does, under
    // some clocksources / seccomp+ptrace combinations), so the real syscall
    // must be permitted for the fallback path.
    rules.insert(libc::SYS_clock_gettime, vec![]);

    // ── readiness waiting ────────────────────────────────────────────────
    // Which of these std reaches for depends on architecture and libc
    // version, so the portable set is permitted. `epoll_wait` and `poll` do
    // not exist on aarch64/riscv64 (those use the `p`-suffixed variants
    // exclusively), where naming them would not even compile.
    #[cfg(target_arch = "x86_64")]
    {
        rules.insert(libc::SYS_epoll_wait, vec![]);
        rules.insert(libc::SYS_poll, vec![]);
    }
    rules.insert(libc::SYS_epoll_pwait, vec![]);
    rules.insert(libc::SYS_ppoll, vec![]);

    // ── termination ──────────────────────────────────────────────────────
    // `close` is needed to drop the IPC endpoint on the way out; `exit` and
    // `exit_group` must always be reachable or the process cannot honour an
    // EOF-triggered shutdown.
    rules.insert(libc::SYS_close, vec![]);
    rules.insert(libc::SYS_exit, vec![]);
    rules.insert(libc::SYS_exit_group, vec![]);

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        target_arch()?,
    )
    .map_err(to_ipc_err)?;

    let program: BpfProgram = filter.try_into().map_err(to_ipc_err)?;

    // TSYNC: apply to every thread in the process, not just this one. The
    // worker spawns its evaluation thread before confining (it cannot after,
    // since `clone` is denied), so the main thread — parked in `join` — would
    // otherwise stay unfiltered. A sandbox with an unconfined thread in it is
    // not a sandbox.
    apply_filter_all_threads(&program).map_err(to_ipc_err)
}
