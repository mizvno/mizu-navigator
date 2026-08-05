//! # `shm` — Shared Memory Region Abstraction
//!
//! This module provides a cross-platform interface for creating, sealing, and
//! mapping anonymous shared memory regions used to transfer the
//! `WireReloadPayload` archive zero-copy from the broker to the worker.
//!
//! ## Platform implementations
//!
//! * [`unix`] — `memfd_create(MFD_CLOEXEC)` on Linux, `shm_open` on macOS.
//!   The sealed (read-only) fd is passed to the worker via `SCM_RIGHTS`.
//! * [`windows`] — `CreateFileMapping(INVALID_HANDLE_VALUE, …)`.
//!   The handle is passed to the worker by inheriting it on process creation
//!   or by `DuplicateHandle`.
//!
//! ## Lifecycle
//!
//! ```text
//! broker:                                  worker:
//!
//! ShmRegion::create(n)
//!   │  (write via as_mut_bytes)
//!   ▼
//! ShmRegion::seal_read_only()
//!   → SealedShmRegion
//!        │  .raw_handle()
//!        │  (sent over socket ancillary channel)
//!        │                                   SealedShmRegion::map_read_only(handle, n)
//!        │                                     → MappedShmSlice
//!        │                                          │  .as_bytes()
//!        │                                          │  rkyv::access(bytes)
//!        ▼
//!   (broker drops SealedShmRegion when worker has mapped it)
//! ```
//!
//! ## Safety boundary
//!
//! The `unsafe` code that calls `mmap` / `MapViewOfFile` lives exclusively
//! in the platform sub-modules.  The types exposed from this module (`ShmRegion`,
//! `SealedShmRegion`, `MappedShmSlice`) have only safe methods.


use crate::error::IpcError;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

// ── Cross-platform handle type ────────────────────────────────────────────────

/// A raw OS handle to a shared memory object.
///
/// On UNIX: a file descriptor (`i32`).
/// On Windows: a `HANDLE` (pointer-sized integer).
#[cfg(unix)]
pub type RawShmHandle = std::os::unix::io::RawFd;
#[cfg(windows)]
pub type RawShmHandle = windows_sys::Win32::Foundation::HANDLE;

// ── Public API ────────────────────────────────────────────────────────────────

/// An anonymous shared memory region open for writing.
///
/// Created by the broker to hold the serialized `WireReloadPayload`.
/// Write the rkyv archive bytes into [`as_mut_bytes`](Self::as_mut_bytes),
/// then call [`seal_read_only`](Self::seal_read_only) to pass the region to
/// the worker.
pub struct ShmRegion {
    #[cfg(unix)]
    inner: unix::UnixShmRegion,
    #[cfg(windows)]
    inner: windows::WindowsShmRegion,
}

impl ShmRegion {
    /// Allocate a new anonymous shared memory region of `byte_len` bytes.
    pub fn create(byte_len: usize) -> Result<Self, IpcError> {
        Ok(ShmRegion {
            #[cfg(unix)]
            inner: unix::UnixShmRegion::create(byte_len)?,
            #[cfg(windows)]
            inner: windows::WindowsShmRegion::create(byte_len)?,
        })
    }

    /// Return a mutable byte slice covering the entire region.
    ///
    /// The slice remains valid for the lifetime of this `ShmRegion`.
    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        self.inner.as_mut_bytes()
    }

    /// Seal the region read-only and return a descriptor the worker can map.
    ///
    /// After calling this, the broker must not write to the region.  The
    /// OS will enforce read-only semantics on the worker's mapping.
    pub fn seal_read_only(self) -> Result<SealedShmRegion, IpcError> {
        let byte_len = self.inner.byte_len();
        Ok(SealedShmRegion {
            #[cfg(unix)]
            inner: self.inner.seal_read_only()?,
            #[cfg(windows)]
            inner: self.inner.seal_read_only()?,
            byte_len,
        })
    }
}

/// A sealed (read-only) shared memory region ready to be sent to the worker.
pub struct SealedShmRegion {
    #[cfg(unix)]
    inner: unix::UnixSealedRegion,
    #[cfg(windows)]
    inner: windows::WindowsSealedRegion,
    byte_len: usize,
}

impl SealedShmRegion {
    /// The byte length of the archive stored in this region.
    pub fn byte_len(&self) -> usize {
        self.byte_len
    }

    /// The raw OS handle to pass to the worker process.
    ///
    /// On UNIX: a file descriptor to be sent via `SCM_RIGHTS`.
    /// On Windows: a `HANDLE` to be inherited or duplicated.
    pub fn raw_handle(&self) -> RawShmHandle {
        self.inner.raw_handle()
    }

    /// Map this region read-only.
    ///
    /// Called by the worker after receiving the OS handle.  Returns a
    /// `MappedShmSlice` that provides `as_bytes()` for zero-copy rkyv access.
    pub fn map_read_only(handle: RawShmHandle, byte_len: usize) -> Result<MappedShmSlice, IpcError> {
        Ok(MappedShmSlice {
            #[cfg(unix)]
            inner: unix::UnixMappedSlice::map_read_only(handle, byte_len)?,
            #[cfg(windows)]
            inner: windows::WindowsMappedSlice::map_read_only(handle, byte_len)?,
            byte_len,
        })
    }
}

/// A read-only view into a `SealedShmRegion`, used by the worker.
///
/// Provides a `&[u8]` slice that can be passed directly to `rkyv::access`
/// for zero-copy archive validation and access.
pub struct MappedShmSlice {
    #[cfg(unix)]
    inner: unix::UnixMappedSlice,
    #[cfg(windows)]
    inner: windows::WindowsMappedSlice,
    byte_len: usize,
}

impl MappedShmSlice {
    /// The rkyv archive bytes stored in this region.
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }

    /// The byte length of this mapping.
    pub fn byte_len(&self) -> usize {
        self.byte_len
    }
}
