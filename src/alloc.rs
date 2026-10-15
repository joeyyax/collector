use std::alloc::{GlobalAlloc, Layout};

// musl's allocator holds on to freed pages: RSS grew to 19 MiB over a 1.3 MiB heap.
#[cfg(not(feature = "jemalloc"))]
use std::alloc::System;
#[cfg(feature = "jemalloc")]
use tikv_jemallocator::Jemalloc as System;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The system allocator plus a live-bytes counter, so heap growth shows apart from RSS.
pub struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            LIVE.fetch_add(new, Ordering::Relaxed);
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}
