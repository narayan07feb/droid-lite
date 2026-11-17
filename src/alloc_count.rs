//! Test builds count live heap bytes, so the end-to-end tests can prove memory stays flat.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

static LIVE: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    /// Bytes this thread asked for, never less: a test reads how much one call allocated.
    static THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn count(n: usize) {
    let _ = THREAD.try_with(|t| t.set(t.get() + n));
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        count(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        LIVE.fetch_add(new as isize - l.size() as isize, Ordering::Relaxed);
        count(new.saturating_sub(l.size()));
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

pub fn live_bytes() -> isize {
    LIVE.load(Ordering::Relaxed)
}

/// Bytes this thread allocated so far (growth by `realloc` too; frees do not count down).
#[cfg(test)]
pub fn thread_allocated() -> usize {
    THREAD.with(|t| t.get())
}
