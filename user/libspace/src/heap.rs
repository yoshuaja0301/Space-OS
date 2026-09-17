//! A small per-process heap: one anonymous region mapped on first use.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::NonNull;

use linked_list_allocator::Heap;

/// Pages mapped for the heap (counted against the process quota).
pub const HEAP_PAGES: usize = 32;

struct UserHeap(UnsafeCell<Option<Heap>>);

// SAFETY: processes are single-threaded in ABI v0.
unsafe impl Sync for UserHeap {}

#[global_allocator]
static HEAP: UserHeap = UserHeap(UnsafeCell::new(None));

impl UserHeap {
    #[allow(clippy::mut_from_ref)] // interior mutability; the process is single-threaded
    fn get(&self) -> Option<&mut Heap> {
        // SAFETY: single-threaded process; no re-entrancy from signals.
        let slot = unsafe { &mut *self.0.get() };
        if slot.is_none() {
            let size = HEAP_PAGES * spaceabi::PAGE_SIZE;
            let base = crate::sys::mem_map(size).ok()?;
            let mut h = Heap::empty();
            // SAFETY: freshly mapped, zeroed, exclusively ours.
            unsafe { h.init(base, size) };
            *slot = Some(h);
        }
        slot.as_mut()
    }
}

// SAFETY: standard first-fit allocator over a mapped region.
unsafe impl GlobalAlloc for UserHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.get() {
            Some(h) => h.allocate_first_fit(layout).map(|p| p.as_ptr()).unwrap_or(core::ptr::null_mut()),
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let (Some(h), Some(p)) = (self.get(), NonNull::new(ptr)) {
            // SAFETY: `ptr` came from `alloc` with the same layout.
            unsafe { h.deallocate(p, layout) };
        }
    }
}
