// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! One page of its own for the expanded key: page-aligned, page-sized,
//! `mlock`'d, overwritten with volatile zero writes before it is
//! `munlock`'d and freed.
//!
//! **Why a whole page and not a heap slot.** `mlock` locks whole pages and
//! does not stack: one `munlock` unlocks the page for every range on it.
//! A key sharing a heap page with another locked secret would be unlocked
//! the moment either one is dropped. A page that holds nothing else has no
//! neighbour to unlock.
//!
//! Allocation, locking, wiping and freeing run at boot and drop only. The
//! one thing that runs per signature is [`LockedPage::as_ptr`] — a
//! pointer read.

use core::ptr::NonNull;
use core::sync::atomic::{compiler_fence, Ordering};
use std::alloc::{alloc_zeroed, dealloc, Layout};

use crate::SignerErr;

/// The smallest page this crate assumes; the real size comes from
/// `sysconf(_SC_PAGESIZE)` (16 KiB on Apple silicon, 4 KiB on x86-64
/// Linux) and is never smaller than this.
pub(crate) const MIN_PAGE: usize = 4096;

/// An owned, zeroed, locked page.
pub(crate) struct LockedPage {
    ptr: NonNull<u8>,
    layout: Layout,
    mlocked: bool,
}

impl LockedPage {
    /// Allocate one zeroed page of its own and lock it into RAM.
    pub(crate) fn new() -> Result<Self, SignerErr> {
        let size = page_size();
        let layout = match Layout::from_size_align(size, size) {
            Ok(l) => l,
            Err(_) => return Err(SignerErr::PageAlloc),
        };
        // SAFETY: `layout` has a non-zero size (≥ MIN_PAGE).
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => return Err(SignerErr::PageAlloc),
        };
        #[cfg(unix)]
        {
            // SAFETY: `ptr` is a live allocation of exactly `size` bytes.
            let rc = unsafe { libc::mlock(ptr.as_ptr().cast(), size) };
            if rc != 0 {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                // SAFETY: allocated just above with this `layout`; nothing
                // was written to it but the allocator's zeroes.
                unsafe { dealloc(ptr.as_ptr(), layout) };
                return Err(SignerErr::Mlock(errno));
            }
        }
        Ok(Self {
            ptr,
            layout,
            mlocked: cfg!(unix),
        })
    }

    /// The page's first byte.
    #[inline(always)]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// The page's first byte, writable.
    #[inline(always)]
    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The page's length in bytes.
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for LockedPage {
    fn drop(&mut self) {
        // SAFETY: the page is ours for exactly `len` bytes until the
        // `dealloc` below.
        unsafe { wipe(self.ptr.as_ptr(), self.len()) };
        #[cfg(unix)]
        {
            if self.mlocked {
                // SAFETY: symmetric with `new`'s `mlock`: same pointer, same length.
                let _ = unsafe { libc::munlock(self.ptr.as_ptr().cast(), self.len()) };
            }
        }
        // SAFETY: allocated in `new` with this very `layout`, freed once.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// Overwrite `len` bytes at `p` with zeros the optimizer may not remove:
/// volatile stores, then a compiler fence so no later access is hoisted
/// above them.
///
/// # Safety
///
/// `p` must be valid for writes of `len` bytes, and nothing may read
/// those bytes as a typed value afterwards unless all-zeros is a valid
/// value of that type.
#[inline(never)]
pub(crate) unsafe fn wipe(p: *mut u8, len: usize) {
    let mut i = 0usize;
    while i < len {
        // SAFETY: `i < len` and the caller guarantees `p..p + len` is writable.
        unsafe { core::ptr::write_volatile(p.add(i), 0) };
        i += 1;
    }
    compiler_fence(Ordering::SeqCst);
}

/// The OS page size, never below [`MIN_PAGE`].
fn page_size() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf` has no preconditions; a failure returns -1.
        let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if n > MIN_PAGE as libc::c_long {
            return n as usize;
        }
    }
    MIN_PAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_page_aligned_page_sized_and_zeroed() {
        let p = LockedPage::new().expect("page");
        assert!(p.len() >= MIN_PAGE);
        assert!(p.len().is_power_of_two());
        assert_eq!(p.as_ptr() as usize % p.len(), 0, "aligned to its own size");
        // SAFETY: the page is live and `len` bytes long.
        let bytes = unsafe { core::slice::from_raw_parts(p.as_ptr(), p.len()) };
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn wipe_zeroes_every_byte_it_is_given_and_nothing_else() {
        let mut buf = [0xA5u8; 96];
        // SAFETY: `buf[8..88]` is writable.
        unsafe { wipe(buf.as_mut_ptr().add(8), 80) };
        assert!(buf[..8].iter().all(|&b| b == 0xA5));
        assert!(buf[8..88].iter().all(|&b| b == 0));
        assert!(buf[88..].iter().all(|&b| b == 0xA5));
    }

    #[test]
    fn the_drop_path_wipes_the_whole_page() {
        // Drop runs `wipe(page, len)`: pin it on a live page by running the
        // same call before the drop and reading the page back.
        let mut p = LockedPage::new().expect("page");
        let len = p.len();
        // SAFETY: the page is ours and `len` bytes long.
        unsafe { core::ptr::write_bytes(p.as_mut_ptr(), 0x5A, len) };
        // SAFETY: as above.
        unsafe { wipe(p.as_mut_ptr(), len) };
        // SAFETY: as above.
        let bytes = unsafe { core::slice::from_raw_parts(p.as_ptr(), len) };
        assert!(bytes.iter().all(|&b| b == 0));
    }
}
