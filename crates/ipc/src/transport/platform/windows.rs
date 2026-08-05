//! # `transport::platform::windows` — Named Pipe IPC transport
//!
//! Provides [`NamedPipeStream`], which implements [`super::DuplexStream`]
//! over **two** named pipes — one per direction.
//!
//! ## Why two pipes and not one duplex pipe
//!
//! The obvious design is a single `PIPE_ACCESS_DUPLEX` pipe, split into
//! halves by duplicating its handle. That works only as long as reads and
//! writes never overlap in time, and deadlocks the moment they do.
//!
//! Windows serializes synchronous I/O **per file object**, not per handle.
//! `DuplicateHandle` produces a second handle to the *same* file object, so a
//! reader thread parked in a blocking `ReadFile` holds that object's lock and
//! any concurrent `WriteFile` — even through the duplicate — queues behind it
//! until the read completes. With a request/response protocol the read only
//! completes when the peer answers, and the peer only answers after receiving
//! the write that is now stuck: a textbook deadlock.
//!
//! It stays invisible while one thread owns both directions (the synchronous
//! `WorkerHost` never overlaps them). It appears immediately once a reader
//! thread is introduced to keep the UI responsive — which is exactly what the
//! async bridge does.
//!
//! Giving each direction its own pipe gives each its own file object, so a
//! blocked read cannot hold up a write. `FILE_FLAG_OVERLAPPED` would also fix
//! it, at the cost of an async I/O layer and considerably more `unsafe`.
//!
//! Pipe naming is derived from one base: `<base>-b2w` carries broker→worker,
//! `<base>-w2b` carries worker→broker. Both sides create/open them in the
//! same order, so the pairing needs no negotiation.
//!
//! ## Safety
//!
//! All `unsafe` blocks are individually justified. This file is the single
//! approved unsafe surface for the Named Pipe transport.

#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::DuplexStream;
use crate::error::IpcError;

use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// Compute the base named pipe path for the given process ID.
pub fn pipe_name(pid: u32) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\mizu-worker-{pid}"))
}

/// Suffix for the broker→worker direction.
fn broker_to_worker(base: &Path) -> Vec<u16> {
    wide(&format!("{}-b2w", base.to_string_lossy()))
}

/// Suffix for the worker→broker direction.
fn worker_to_broker(base: &Path) -> Vec<u16> {
    wide(&format!("{}-w2b", base.to_string_lossy()))
}

fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// A Windows Named Pipe transport: one pipe per direction.
pub struct NamedPipeStream {
    /// This side writes here.
    write: File,
    /// This side reads here.
    read: File,
}

/// Creates one server pipe instance. Does **not** wait for a client.
///
/// Creation is split from acceptance because `ConnectNamedPipe` blocks: if
/// the two pipes were created-and-accepted one after the other, the second
/// name would not exist until the first client had already connected, and a
/// worker opening them in order would hit `ERROR_FILE_NOT_FOUND` on the
/// second. Both names must exist before the worker is spawned.
fn create_pipe(name: &[u16]) -> Result<File, IpcError> {
    use std::os::windows::io::FromRawHandle;

    // SAFETY: `name` is NUL-terminated; the remaining arguments are valid
    // constants. `nMaxInstances = 1` means this name accepts exactly one
    // connection, so a late squatter cannot open a second instance.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,     // nMaxInstances
            65536, // nOutBufferSize
            65536, // nInBufferSize
            0,     // nDefaultTimeOut (platform default)
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE as _ {
        return Err(IpcError::Transport(std::io::Error::last_os_error()));
    }
    // SAFETY: handle is valid and ownership transfers to the File.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

/// Blocks until a client connects to an already-created pipe instance.
fn accept_pipe(pipe: &File) -> Result<(), IpcError> {
    use std::os::windows::io::AsRawHandle;
    let handle = pipe.as_raw_handle() as _;

    // SAFETY: handle is a live pipe-server handle owned by `pipe`.
    // ERROR_PIPE_CONNECTED (535) is a success condition: the client
    // connected between creation and this call.
    let ok = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
    if ok == 0 {
        // SAFETY: reading the calling thread's last error code.
        let code = unsafe { GetLastError() };
        if code != 535 {
            return Err(IpcError::Transport(std::io::Error::from_raw_os_error(
                code as i32,
            )));
        }
    }
    Ok(())
}

/// Both server pipe instances, created but not yet connected.
pub struct PendingPipes {
    b2w: File,
    w2b: File,
}

impl PendingPipes {
    /// Creates both pipe instances so the names exist before the worker is
    /// spawned. Returns immediately.
    pub fn create(base: &Path) -> Result<Self, IpcError> {
        Ok(PendingPipes {
            b2w: create_pipe(&broker_to_worker(base))?,
            w2b: create_pipe(&worker_to_broker(base))?,
        })
    }

    /// Waits for the worker to connect to both, in the order it opens them.
    pub fn accept(self) -> Result<NamedPipeStream, IpcError> {
        accept_pipe(&self.b2w)?;
        accept_pipe(&self.w2b)?;
        Ok(NamedPipeStream {
            write: self.b2w,
            read: self.w2b,
        })
    }
}

impl NamedPipeStream {
    /// **Broker side**: create both pipes and wait for the worker on each.
    ///
    /// Creation order matters and is mirrored by
    /// [`connect_client`](Self::connect_client): the broker accepts `b2w`
    /// first, so the worker must open `b2w` first too.
    /// Convenience for callers that create and accept in one step. The
    /// spawner uses [`PendingPipes`] directly so it can create both names,
    /// spawn the worker, and only then block in accept.
    pub fn create_server(pipe_path: &Path) -> Result<Self, IpcError> {
        PendingPipes::create(pipe_path)?.accept()
    }

    /// **Worker side**: open both pipes, in the broker's accept order.
    pub fn connect_client(pipe_path: &Path) -> Result<Self, IpcError> {
        let b2w = open_client(&format!("{}-b2w", pipe_path.to_string_lossy()))?;
        let w2b = open_client(&format!("{}-w2b", pipe_path.to_string_lossy()))?;
        Ok(NamedPipeStream {
            // Mirrored: what the broker writes, the worker reads.
            write: w2b,
            read: b2w,
        })
    }
}

fn open_client(name: &str) -> Result<File, IpcError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(name)
        .map_err(IpcError::Transport)
}

impl Read for NamedPipeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read.read(buf)
    }
}

impl Write for NamedPipeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.write.flush()
    }
}

/// Read half: owns the inbound pipe outright.
pub struct PipeReadHalf(File);
/// Write half: owns the outbound pipe outright.
pub struct PipeWriteHalf(File);

impl Read for PipeReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for PipeWriteHalf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl DuplexStream for NamedPipeStream {
    type ReadHalf = PipeReadHalf;
    type WriteHalf = PipeWriteHalf;

    /// Splitting is now a plain move: each direction already has its own
    /// pipe, so no handle duplication — and no shared file object — is
    /// involved. This is what makes concurrent read and write safe.
    fn split(self) -> Result<(Self::WriteHalf, Self::ReadHalf), IpcError> {
        Ok((PipeWriteHalf(self.write), PipeReadHalf(self.read)))
    }
}
