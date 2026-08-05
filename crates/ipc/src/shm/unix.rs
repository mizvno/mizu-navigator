//! # `shm::unix` — UNIX shared memory (memfd / mmap)
//!
//! ## Implementation strategy
//!
//! On **Linux**: `memfd_create(MFD_CLOEXEC | MFD_ALLOW_SEALING)` + `ftruncate`
//! + `mmap(PROT_READ | PROT_WRITE)`.  Before sealing,
//! `fcntl(F_ADD_SEALS, F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW)` is
//! applied so the kernel prevents further writes even if a writable fd escapes.
//!
//! On **macOS / other POSIX**: `shm_open(O_CREAT | O_RDWR)` + immediately
//! `shm_unlink` (the fd stays valid) + `ftruncate` + `mmap`.
//!
//! ## Safety
//!
//! All `unsafe` blocks are individually justified.  This file is the single
//! approved location for `unsafe` code in the UNIX SHM path.

#![allow(unsafe_code)]

use std::os::unix::io::RawFd;

use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use nix::unistd::ftruncate;

use crate::error::IpcError;

// ── platform fd creation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn make_anon_fd(byte_len: usize) -> Result<RawFd, IpcError> {
    use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
    use std::ffi::CString;

    let name   = CString::new("mizu-reload").expect("static string");
    let flags  = MemFdCreateFlag::MFD_CLOEXEC | MemFdCreateFlag::MFD_ALLOW_SEALING;
    let owned  = memfd_create(&name, flags)
        .map_err(|e| IpcError::Shm(format!("memfd_create: {e}")))?;
    let fd: RawFd = {
        use std::os::unix::io::IntoRawFd;
        owned.into_raw_fd()
    };
    // SAFETY: fd is open and valid.
    let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
    ftruncate(borrowed, byte_len as i64)
        .map_err(|e| { close_fd(fd); IpcError::Shm(format!("ftruncate: {e}")) })?;
    Ok(fd)
}

#[cfg(not(target_os = "linux"))]
fn make_anon_fd(byte_len: usize) -> Result<RawFd, IpcError> {
    use nix::fcntl::OFlag;
    use nix::sys::mman::{shm_open, shm_unlink};
    use nix::sys::stat::Mode;
    use std::ffi::CString;
    use std::os::unix::io::IntoRawFd;

    let name = CString::new(format!("/mizu-reload-{}", std::process::id()))
        .map_err(|e| IpcError::Shm(e.to_string()))?;
    let owned = shm_open(
        name.as_c_str(),
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_EXCL,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .map_err(|e| IpcError::Shm(format!("shm_open: {e}")))?;
    let _ = shm_unlink(name.as_c_str()); // unlink name; fd remains valid
    let fd: RawFd = owned.into_raw_fd();
    // SAFETY: fd is open and valid.
    let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
    ftruncate(borrowed, byte_len as i64)
        .map_err(|e| { close_fd(fd); IpcError::Shm(format!("ftruncate: {e}")) })?;
    Ok(fd)
}

fn close_fd(fd: RawFd) {
    let _ = nix::unistd::close(fd);
}

// ── mmap helper ───────────────────────────────────────────────────────────────

/// Map `fd` with the given protection flags.
///
/// # Safety
///
/// `fd` must be open and valid; `byte_len` must be > 0 and match the size the
/// fd was ftruncate'd to.  The returned `NonNull<u8>` remains valid until
/// `munmap` is called with the same pointer and length.
unsafe fn do_mmap(
    fd: RawFd,
    byte_len: usize,
    prot: ProtFlags,
) -> Result<std::ptr::NonNull<u8>, IpcError> {
    let nz = std::num::NonZeroUsize::new(byte_len)
        .ok_or_else(|| IpcError::Shm("zero-length SHM region".into()))?;

    // Duplicate the fd so we can wrap it in an OwnedFd for nix::mmap without
    // worrying about double-close: nix::mmap requires a reference to an
    // AsFd type, and the mapping itself holds no reference to the fd after
    // the call returns.
    let dup_fd = {
        use std::os::unix::io::{FromRawFd, OwnedFd};
        let raw_dup = nix::unistd::dup(fd)
            .map_err(|e| IpcError::Shm(format!("dup: {e}")))?;
        // SAFETY: raw_dup is valid and owned.
        unsafe { OwnedFd::from_raw_fd(raw_dup) }
    };

    // SAFETY: dup_fd is valid; nz > 0; offset = 0.
    let ptr = unsafe {
        mmap(None, nz, prot, MapFlags::MAP_SHARED, dup_fd, 0)
            .map_err(|e| IpcError::Shm(format!("mmap: {e}")))?
    };
    // OwnedFd (dup_fd) is dropped here — that is fine; the mapping outlives it.
    Ok(ptr.cast())
}

// ── UnixShmRegion ─────────────────────────────────────────────────────────────

/// Writable phase of a UNIX shared memory region (broker write path).
pub struct UnixShmRegion {
    fd:       RawFd,
    ptr:      std::ptr::NonNull<u8>,
    byte_len: usize,
}

// SAFETY: the ptr points to an OS-mapped region owned exclusively by this
// struct; it does not alias any Rust reference.
unsafe impl Send for UnixShmRegion {}

impl UnixShmRegion {
    pub fn create(byte_len: usize) -> Result<Self, IpcError> {
        let fd = make_anon_fd(byte_len)?;
        // SAFETY: fd is valid; byte_len matches ftruncate; PROT_READ|WRITE.
        let ptr = unsafe {
            do_mmap(fd, byte_len, ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)?
        };
        Ok(UnixShmRegion { fd, ptr, byte_len })
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for byte_len bytes (mmap'd PROT_WRITE) and
        // we hold &mut self so no other reference to this region exists.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.byte_len) }
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub fn seal_read_only(self) -> Result<UnixSealedRegion, IpcError> {
        // Unmap the write view.
        // SAFETY: ptr and byte_len came from do_mmap in create().
        unsafe {
            munmap(self.ptr.cast(), self.byte_len)
                .map_err(|e| IpcError::Shm(format!("munmap: {e}")))?;
        }

        // On Linux, add kernel-level write/size seals.
        #[cfg(target_os = "linux")]
        {
            use nix::fcntl::{fcntl, FcntlArg, SealFlag};
            fcntl(self.fd, FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_WRITE | SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW,
            ))
            .map_err(|e| IpcError::Shm(format!("F_ADD_SEALS: {e}")))?;
        }

        let fd = self.fd;
        std::mem::forget(self); // suppress Drop so we don't close fd here
        Ok(UnixSealedRegion { fd })
    }
}

impl Drop for UnixShmRegion {
    fn drop(&mut self) {
        // SAFETY: ptr and byte_len valid (mmap'd in create).
        unsafe { let _ = munmap(self.ptr.cast(), self.byte_len); }
        close_fd(self.fd);
    }
}

// ── UnixSealedRegion ──────────────────────────────────────────────────────────

/// Sealed (read-only) phase — the fd to pass to the worker via SCM_RIGHTS.
pub struct UnixSealedRegion {
    fd: RawFd,
}

impl UnixSealedRegion {
    pub fn raw_handle(&self) -> RawFd {
        self.fd
    }
}

impl Drop for UnixSealedRegion {
    fn drop(&mut self) {
        close_fd(self.fd);
    }
}

// ── UnixMappedSlice ───────────────────────────────────────────────────────────

/// Read-only mapping on the worker side.
pub struct UnixMappedSlice {
    ptr:      std::ptr::NonNull<u8>,
    byte_len: usize,
}

// SAFETY: same reasoning as UnixShmRegion.
unsafe impl Send for UnixMappedSlice {}

impl UnixMappedSlice {
    pub fn map_read_only(fd: RawFd, byte_len: usize) -> Result<Self, IpcError> {
        // SAFETY: fd received from the broker via SCM_RIGHTS; byte_len from
        // WireReloadHandle, which the broker set to match the archive size.
        let ptr = unsafe { do_mmap(fd, byte_len, ProtFlags::PROT_READ)? };
        // Close our copy of the fd — the mapping holds its own reference at
        // the kernel level and remains valid even after the fd is closed.
        close_fd(fd);
        Ok(UnixMappedSlice { ptr, byte_len })
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: ptr is valid for byte_len bytes (PROT_READ mapping);
        // the kernel guarantees the pages are pinned until munmap.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.byte_len) }
    }
}

impl Drop for UnixMappedSlice {
    fn drop(&mut self) {
        // SAFETY: ptr and byte_len came from do_mmap in map_read_only.
        unsafe { let _ = munmap(self.ptr.cast(), self.byte_len); }
    }
}
