// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The key's page is ZERO when it is freed — the real `Drop` path,
//! observed from the allocator. This test binary installs an allocator
//! that, on every page-aligned, page-sized free, reads the whole block
//! before handing it back to the system and counts any that still hold a
//! set byte. One test only: the counters are process-wide.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use signer_ed25519::{Ed25519Signer, SIG_B64_LEN};

struct WatchPageFrees;

static PAGE_FREES: AtomicUsize = AtomicUsize::new(0);
static DIRTY_PAGE_FREES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's arguments;
// `dealloc` only reads the block (still owned by the caller) first.
unsafe impl GlobalAlloc for WatchPageFrees {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() >= 4096 && layout.align() == layout.size() {
            PAGE_FREES.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the block is live and `layout.size()` bytes long until
            // the `System.dealloc` below.
            let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
            let mut i = 0usize;
            while i < bytes.len() {
                if bytes[i] != 0 {
                    DIRTY_PAGE_FREES.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                i += 1;
            }
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static WATCH: WatchPageFrees = WatchPageFrees;

#[test]
fn the_key_page_is_zero_when_it_is_freed() {
    // Break-and-watch on the watcher itself: a page freed with a byte set
    // IS seen.
    let seen = DIRTY_PAGE_FREES.load(Ordering::SeqCst);
    let layout = Layout::from_size_align(4096, 4096).expect("layout");
    // SAFETY: a non-zero-sized layout; the block is written in bounds and
    // freed once with the same layout.
    unsafe {
        let p = std::alloc::alloc(layout);
        assert!(!p.is_null());
        p.write(0x01);
        std::alloc::dealloc(p, layout);
    }
    assert_eq!(DIRTY_PAGE_FREES.load(Ordering::SeqCst) - seen, 1, "the watcher must see a dirty page");

    let frees = PAGE_FREES.load(Ordering::SeqCst);
    let dirty = DIRTY_PAGE_FREES.load(Ordering::SeqCst);
    let signer = Ed25519Signer::from_seed(&[0x5A; 32]).expect("signer");
    let mut b64 = [0u8; SIG_B64_LEN];
    assert_eq!(signer.sign_b64(b"the page holds a live key", &mut b64), SIG_B64_LEN);
    drop(signer);
    assert_eq!(PAGE_FREES.load(Ordering::SeqCst) - frees, 1, "exactly the key's page was freed");
    assert_eq!(
        DIRTY_PAGE_FREES.load(Ordering::SeqCst) - dirty,
        0,
        "the key's page was freed with a byte still set"
    );
}
