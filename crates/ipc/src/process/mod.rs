//! # `process` — worker process lifecycle (spawn, handshake, shutdown)
//!
//! Phase 4's runtime glue: creating the IPC endpoint, launching the
//! `mizu-worker` binary, authenticating it, and tearing the pair down
//! cleanly.
//!
//! ## Rendezvous by name, not by inherited handle
//!
//! The worker finds the broker by *connecting to a name* (a Windows named
//! pipe, or a UNIX socket path) that the broker passes in the environment —
//! not by inheriting a file descriptor or handle across `spawn`.
//!
//! This is a deliberate choice, and it is the stronger one for the "no
//! handle leaks" constraint. Handle inheritance on Windows is all-or-nothing
//! per `CreateProcess` call unless you drive `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
//! by hand, and on UNIX every fd without `CLOEXEC` crosses `exec` implicitly —
//! both are opt-*out* models where the failure mode is a silent leak. Name-based
//! rendezvous is opt-*in*: the child inherits nothing, and acquires exactly one
//! fd by connecting to it itself.
//!
//! ## Why the connection is authenticated
//!
//! A name is not a capability — any process running as the same user can
//! connect to a pipe or socket it can guess. The name therefore embeds 128
//! bits of OS entropy, *and* the first frame the worker sends must carry a
//! matching secret token ([`HANDSHAKE_TOKEN_ENV`]) that only the real child
//! was given. A squatter that wins the race to connect cannot produce the
//! token and is dropped before it can send a single [`crate::WireWorkerEnvelope`].
//!
//! This defends against a same-user process racing the rendezvous. It does
//! not defend against a same-user process that simply reads our memory or
//! `ptrace`s us — that is not a boundary any userspace design can hold, and
//! is out of scope.
//!
//! ## The worker on the far end
//!
//! `mizu-worker` drives a `mizu_core::parser::logic_worker::TabSession` from
//! the frames this module delivers. One process serves one document, so the
//! OS boundary is the isolation between tabs — there is no `TabId` routing
//! inside a worker.
//!
//! ## Shutdown
//!
//! Dropping [`WorkerProcess`] closes the broker's end of the channel. The
//! worker's next `read` returns 0 bytes, which the framer surfaces as
//! [`IpcError::WorkerDied`], and the worker's loop treats EOF as "the broker
//! is gone" and exits. No signal, no kill, no orphan: closing the channel
//! *is* the shutdown protocol, so it works identically whether the broker
//! exited cleanly or crashed.

use std::process::{Child, Command};

use crate::error::IpcError;
use crate::transport::channel::{IpcReceiver, IpcSender};
use crate::transport::platform::DuplexStream;
use crate::wire::{WireUiEvent, WireWorkerEnvelope};

/// Environment variable carrying the rendezvous name (pipe name / socket path).
pub const CHANNEL_NAME_ENV: &str = "MIZU_IPC_CHANNEL";

/// Environment variable carrying the handshake secret the worker must echo.
pub const HANDSHAKE_TOKEN_ENV: &str = "MIZU_IPC_TOKEN";

/// Bytes of entropy in the rendezvous name and the handshake token.
const TOKEN_BYTES: usize = 16;

/// Generates a hex token with [`TOKEN_BYTES`] bytes of OS entropy.
///
/// Uses `getrandom` via the OS rather than a userspace PRNG: this value is
/// the only thing standing between a same-user squatter and the worker's
/// side of the channel, so it must not be predictable from process state.
fn random_token() -> Result<String, IpcError> {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom_bytes(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(windows)]
fn getrandom_bytes(buf: &mut [u8]) -> Result<(), IpcError> {
    crate::process::windows_rand::fill(buf)
}

#[cfg(unix)]
fn getrandom_bytes(buf: &mut [u8]) -> Result<(), IpcError> {
    use std::io::Read;
    // `/dev/urandom` rather than the `getrandom` syscall so this needs no
    // extra dependency; the broker is unsandboxed, so opening it is fine.
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| IpcError::Sandbox(format!("open /dev/urandom: {e}")))?;
    f.read_exact(buf)
        .map_err(|e| IpcError::Sandbox(format!("read /dev/urandom: {e}")))
}

#[cfg(windows)]
pub(crate) mod windows_rand;

/// The rendezvous address for one broker↔worker channel.
#[derive(Debug, Clone)]
pub struct ChannelName(String);

impl ChannelName {
    /// Mints a fresh, unguessable channel name for this platform.
    pub fn generate() -> Result<Self, IpcError> {
        let token = random_token()?;
        #[cfg(windows)]
        let name = format!(r"\\.\pipe\mizu-worker-{}-{token}", std::process::id());
        #[cfg(unix)]
        let name = std::env::temp_dir()
            .join(format!("mizu-worker-{}-{token}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
        Ok(ChannelName(name))
    }

    /// Reconstructs a channel name the broker passed to this process.
    #[must_use]
    pub fn from_raw(name: String) -> Self {
        ChannelName(name)
    }

    /// The platform-native address string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Owns the worker's OS process, and reaps it on drop.
///
/// Separated from [`WorkerProcess`] so the latter has no `Drop` impl of its
/// own and can therefore be destructured by
/// [`WorkerProcess::into_parts`] — moving the receiver onto a reader thread
/// while the process handle stays with the caller. A `Drop` on the outer
/// struct would make that move impossible without `unsafe`.
pub struct ChildGuard {
    child: Child,
    #[cfg(unix)]
    socket_path: String,
}

impl ChildGuard {
    /// The worker's OS process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Reaps the worker if it has already exited, without blocking.
    ///
    /// The broker calls this after a transport failure to find out *why* the
    /// channel broke. A worker killed by its own sandbox — `SIGSYS` from
    /// seccomp, or terminated by the Job Object — is a security event and
    /// looks completely different from one that exited 0 because the tab was
    /// closed, but both present identically as a broken pipe until the exit
    /// status is inspected.
    ///
    /// Returns `Ok(None)` if the worker is still running.
    pub fn try_exit_status(&mut self) -> Result<Option<std::process::ExitStatus>, IpcError> {
        self.child.try_wait().map_err(IpcError::Transport)
    }

    /// Waits for the worker to exit, killing it if `grace` elapses first.
    ///
    /// The caller must already have closed the channel — that is the actual
    /// shutdown request, and this only waits for the worker to act on it.
    pub fn wait_for_exit(
        mut self,
        grace: std::time::Duration,
    ) -> Result<std::process::ExitStatus, IpcError> {
        let deadline = std::time::Instant::now() + grace;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = self.child.kill();
                        return self.child.wait().map_err(IpcError::Transport);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(IpcError::Transport(e)),
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: whoever owns the channel halves drops them too, so the
        // worker gets its EOF regardless. Reaping here prevents a zombie if
        // the caller dropped us instead of waiting.
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

/// A live, authenticated worker process and its IPC channel.
///
/// Dropping this closes the channel, which the worker observes as EOF and
/// responds to by exiting — see the module docs on shutdown.
pub struct WorkerProcess {
    guard: ChildGuard,
    /// Events the broker sends to the worker.
    pub tx: IpcSender<WireUiEvent>,
    /// Responses the worker sends back.
    pub rx: IpcReceiver<WireWorkerEnvelope>,
}

impl WorkerProcess {
    /// The spawned worker's OS process id, for logging and diagnostics.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.guard.id()
    }

    /// See [`ChildGuard::try_exit_status`].
    pub fn try_exit_status(&mut self) -> Result<Option<std::process::ExitStatus>, IpcError> {
        self.guard.try_exit_status()
    }

    /// Splits into the two channel halves and the process handle.
    ///
    /// This is what makes the asynchronous bridge possible: the receiver
    /// moves onto a dedicated reader thread that can block on `recv` without
    /// stalling the UI, while the sender and the process handle stay on the
    /// thread that owns them.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        IpcSender<WireUiEvent>,
        IpcReceiver<WireWorkerEnvelope>,
        ChildGuard,
    ) {
        (self.tx, self.rx, self.guard)
    }

    /// Asks the worker to exit and waits for it, returning its exit status.
    ///
    /// Closing the channel is the request: this drops both halves, the
    /// worker's blocking `read` returns EOF, and it exits on its own. Only
    /// if it fails to do so within `grace` is it killed — a worker stuck in
    /// a pathological evaluation would otherwise hang the broker's shutdown.
    pub fn shutdown(
        mut self,
        grace: std::time::Duration,
    ) -> Result<std::process::ExitStatus, IpcError> {
        self.tx.close();
        self.rx.close();
        self.guard.wait_for_exit(grace)
    }
}

/// Spawns `worker_exe` as a sandboxed worker and completes the handshake.
///
/// Blocks until the worker connects and proves it is the process we spawned,
/// or until the OS reports the spawn failed.
///
/// `args` are passed through to the child unchanged; production callers pass
/// `&[]` (the rendezvous travels in the environment, not the command line,
/// so it never appears in `ps` output). Tests use it to re-execute their own
/// test binary with a filter selecting exactly one worker entry point.
///
/// # Errors
///
/// * [`IpcError::Transport`] if the binary could not be launched or the
///   endpoint could not be created.
/// * [`IpcError::Protocol`] if the peer that connected could not produce the
///   handshake token — i.e. something other than our child won the race.
pub fn spawn_worker(
    worker_exe: &std::path::Path,
    args: &[&str],
) -> Result<WorkerProcess, IpcError> {
    let name = ChannelName::generate()?;
    let token = random_token()?;

    // The listener must exist before the child is launched, or the child can
    // lose a connect race against a server that is not yet bound.
    let listener = PendingChannel::bind(&name)?;

    let child = Command::new(worker_exe)
        .args(args)
        .env(CHANNEL_NAME_ENV, name.as_str())
        .env(HANDSHAKE_TOKEN_ENV, &token)
        .spawn()
        .map_err(IpcError::Transport)?;

    let stream = listener.accept()?;
    let (write_half, read_half) = stream.split()?;
    let tx = IpcSender::<WireUiEvent>::new(write_half);
    let mut rx = IpcReceiver::<WireWorkerEnvelope>::new(read_half);

    // Authenticate before trusting anything else on this channel.
    match rx.recv() {
        Ok(WireWorkerEnvelope::Hello { token: got }) if got == token => {}
        Ok(WireWorkerEnvelope::Hello { .. }) => {
            return Err(IpcError::Protocol(
                "worker handshake token mismatch; a foreign process won the \
                 rendezvous race and was rejected"
                    .to_string(),
            ));
        }
        Ok(other) => {
            return Err(IpcError::Protocol(format!(
                "expected Hello as the first worker frame, got {other:?}"
            )));
        }
        Err(e) => return Err(e),
    }

    Ok(WorkerProcess {
        guard: ChildGuard {
            child,
            #[cfg(unix)]
            socket_path: name.as_str().to_string(),
        },
        tx,
        rx,
    })
}

// ── Platform listener ────────────────────────────────────────────────────────

/// A bound-but-not-yet-connected channel endpoint on the broker side.
struct PendingChannel {
    #[cfg(windows)]
    pipes: crate::transport::platform::windows::PendingPipes,
    #[cfg(unix)]
    listener: std::os::unix::net::UnixListener,
}

impl PendingChannel {
    #[cfg(windows)]
    fn bind(name: &ChannelName) -> Result<Self, IpcError> {
        // Both pipe instances are created here, before the worker is
        // spawned, so both names already exist when it connects. Accepting
        // blocks, so it cannot happen until after the spawn.
        Ok(PendingChannel {
            pipes: crate::transport::platform::windows::PendingPipes::create(
                std::path::Path::new(name.as_str()),
            )?,
        })
    }

    #[cfg(unix)]
    fn bind(name: &ChannelName) -> Result<Self, IpcError> {
        use std::os::unix::fs::PermissionsExt;
        let path = name.as_str();
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path).map_err(IpcError::Transport)?;
        // 0600: only this user may connect at all. The handshake token is the
        // second line of defence, not the first.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(IpcError::Transport)?;
        Ok(PendingChannel { listener })
    }

    #[cfg(windows)]
    fn accept(self) -> Result<crate::transport::platform::NamedPipeStream, IpcError> {
        self.pipes.accept()
    }

    #[cfg(unix)]
    fn accept(self) -> Result<crate::transport::platform::UnixIpcStream, IpcError> {
        crate::transport::platform::UnixIpcStream::from_listener(self.listener)
    }
}

// ── Worker side ──────────────────────────────────────────────────────────────

/// The worker's view of its channel, before sandboxing.
pub struct WorkerChannel {
    /// Responses the worker sends to the broker.
    pub tx: IpcSender<WireWorkerEnvelope>,
    /// Events the worker receives.
    pub rx: IpcReceiver<WireUiEvent>,
    /// Raw fds for the seccomp filter's fd-equality conditions.
    #[cfg(unix)]
    pub handles: crate::sandbox::WorkerIpcHandles,
}

/// Connects back to the broker using the environment the broker set, and
/// sends the handshake frame.
///
/// Called by `mizu-worker` as its very first action, *before*
/// [`crate::confine_current_process`] — connecting requires syscalls the
/// sandbox is about to deny forever.
///
/// # Errors
///
/// [`IpcError::Protocol`] if the environment variables are absent (the
/// binary was run directly rather than spawned by a broker), or
/// [`IpcError::Transport`] if the rendezvous fails.
pub fn connect_to_broker() -> Result<WorkerChannel, IpcError> {
    let name = std::env::var(CHANNEL_NAME_ENV).map_err(|_| {
        IpcError::Protocol(format!(
            "{CHANNEL_NAME_ENV} is not set; mizu-worker is spawned by the \
             browser process and cannot be run directly"
        ))
    })?;
    let token = std::env::var(HANDSHAKE_TOKEN_ENV).map_err(|_| {
        IpcError::Protocol(format!("{HANDSHAKE_TOKEN_ENV} is not set"))
    })?;

    let stream = connect_stream(&name)?;
    #[cfg(unix)]
    let handles = {
        let fd = stream.as_raw_fd();
        crate::sandbox::WorkerIpcHandles {
            read_fd: fd,
            write_fd: fd,
        }
    };

    let (write_half, read_half) = stream.split()?;
    let mut tx = IpcSender::<WireWorkerEnvelope>::new(write_half);
    let rx = IpcReceiver::<WireUiEvent>::new(read_half);

    tx.send(&WireWorkerEnvelope::Hello { token })?;

    Ok(WorkerChannel {
        tx,
        rx,
        #[cfg(unix)]
        handles,
    })
}

#[cfg(windows)]
fn connect_stream(name: &str) -> Result<crate::transport::platform::NamedPipeStream, IpcError> {
    crate::transport::platform::NamedPipeStream::connect_client(std::path::Path::new(name))
}

#[cfg(unix)]
fn connect_stream(name: &str) -> Result<crate::transport::platform::UnixIpcStream, IpcError> {
    crate::transport::platform::UnixIpcStream::connect(std::path::Path::new(name))
}
