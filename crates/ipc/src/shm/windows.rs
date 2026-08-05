//! # `shm::windows` — Windows shared memory (CreateFileMapping / MapViewOfFile)
//!
//! ## Implementation strategy
//!
//! Uses `CreateFileMappingW(INVALID_HANDLE_VALUE, …, PAGE_READWRITE, …)` to
//! create a page-file–backed section object.  The broker maps it
//! `FILE_MAP_WRITE` and the worker maps the duplicated handle `FILE_MAP_READ`.
//!
//! ## Safety
//!
//! All `unsafe` blocks are individually justified.

#![allow(unsafe_code)]

use std::os::windows::io::RawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile,
    FILE_MAP_READ, FILE_MAP_WRITE, PAGE_READWRITE,
};

use crate::error::IpcError;

fn last_os_error() -> IpcError {
    IpcError::Transport(std::io::Error::last_os_error())
}

// ── WindowsShmRegion ──────────────────────────────────────────────────────────

pub struct WindowsShmRegion {
    mapping:  HANDLE,
    view:     *mut u8,
    byte_len: usize,
}

// SAFETY: HANDLE and view pointer are stable, process-scoped, single-owner.
unsafe impl Send for WindowsShmRegion {}

impl WindowsShmRegion {
    pub fn create(byte_len: usize) -> Result<Self, IpcError> {
        let hi = ((byte_len as u64) >> 32) as u32;
        let lo = (byte_len & 0xFFFF_FFFF) as u32;

        // SAFETY: INVALID_HANDLE_VALUE → page-file-backed section; no security
        // attributes (uses process default); no name (anonymous).
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                hi,
                lo,
                std::ptr::null(),
            )
        };
        if mapping.is_null() {
            return Err(last_os_error());
        }

        // SAFETY: mapping is non-null and valid.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, byte_len) };
        if view.Value.is_null() {
            // SAFETY: mapping is valid.
            unsafe { CloseHandle(mapping) };
            return Err(last_os_error());
        }

        Ok(WindowsShmRegion { mapping, view: view.Value as *mut u8, byte_len })
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: view is valid for byte_len bytes, mapped FILE_MAP_WRITE;
        // &mut self ensures exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.view, self.byte_len) }
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub fn seal_read_only(mut self) -> Result<WindowsSealedRegion, IpcError> {
        // Unmap the write view; keep the mapping handle for the worker.
        // SAFETY: view is valid (mapped in create).
        unsafe { UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ }) };
        self.view = std::ptr::null_mut(); // prevent double-unmap in Drop

        let mapping = self.mapping;
        self.mapping = std::ptr::null_mut(); // prevent close in Drop
        Ok(WindowsSealedRegion { mapping })
    }
}

impl Drop for WindowsShmRegion {
    fn drop(&mut self) {
        if !self.view.is_null() {
            // SAFETY: view is valid.
            unsafe { UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ }) };
        }
        if !self.mapping.is_null() {
            // SAFETY: mapping is valid.
            unsafe { CloseHandle(self.mapping) };
        }
    }
}

// ── WindowsSealedRegion ───────────────────────────────────────────────────────

pub struct WindowsSealedRegion {
    mapping: HANDLE,
}

impl WindowsSealedRegion {
    pub fn raw_handle(&self) -> RawHandle {
        self.mapping as RawHandle
    }
}

impl Drop for WindowsSealedRegion {
    fn drop(&mut self) {
        if !self.mapping.is_null() {
            // SAFETY: mapping is valid.
            unsafe { CloseHandle(self.mapping) };
        }
    }
}

// ── WindowsMappedSlice ────────────────────────────────────────────────────────

pub struct WindowsMappedSlice {
    view:     *const u8,
    byte_len: usize,
    mapping:  HANDLE,
}

// SAFETY: view pointer is stable, single-owner.
unsafe impl Send for WindowsMappedSlice {}

impl WindowsMappedSlice {
    pub fn map_read_only(handle: RawHandle, byte_len: usize) -> Result<Self, IpcError> {
        let mapping = handle as HANDLE;
        // SAFETY: handle was duplicated by the broker; FILE_MAP_READ is safe.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, byte_len) };
        if view.Value.is_null() {
            return Err(last_os_error());
        }
        Ok(WindowsMappedSlice { view: view.Value as *const u8, byte_len, mapping })
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: view is valid for byte_len bytes (FILE_MAP_READ mapping).
        unsafe { std::slice::from_raw_parts(self.view, self.byte_len) }
    }
}

impl Drop for WindowsMappedSlice {
    fn drop(&mut self) {
        // SAFETY: view is valid.
        unsafe { UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ }) };
        if !self.mapping.is_null() {
            // SAFETY: mapping is valid.
            unsafe { CloseHandle(self.mapping) };
        }
    }
}
