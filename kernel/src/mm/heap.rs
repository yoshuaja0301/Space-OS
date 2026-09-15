//! Kernel heap: a fixed 16 MiB window backed by frames at boot.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;

use linked_list_allocator::Heap;
use x86_64::structures::paging::PageTableFlags;

use super::{HEAP_BASE, HEAP_SIZE, PAGE_SIZE, frame, paging};
use crate::sync::SpinLock;

struct HeapInner(Heap);
// SAFETY: the heap is only touched under the spinlock.
unsafe impl Send for HeapInner {}

struct KernelHeap(SpinLock<HeapInner>);

#[global_allocator]
static HEAP: KernelHeap = KernelHeap(SpinLock::new(HeapInner(Heap::empty())));

// SAFETY: standard first-fit allocator behind a lock.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut h = self.0.lock();
        h.0.allocate_first_fit(layout).map(|p| p.as_ptr()).unwrap_or(core::ptr::null_mut())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut h = self.0.lock();
        if let Some(p) = NonNull::new(ptr) {
            // SAFETY: `ptr` came from `alloc` with the same layout.
            unsafe { h.0.deallocate(p, layout) };
        }
    }
}

pub fn init() {
    let pages = HEAP_SIZE as u64 / PAGE_SIZE;
    for i in 0..pages {
        let f = frame::alloc_zeroed().expect("no frame for kernel heap");
        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::GLOBAL
            | PageTableFlags::NO_EXECUTE;
        paging::map_kernel_page(HEAP_BASE + i * PAGE_SIZE, f, flags).expect("map kernel heap");
    }
    let mut h = HEAP.0.lock();
    // SAFETY: the range was just mapped and is exclusively the heap's.
    unsafe { h.0.init(HEAP_BASE as *mut u8, HEAP_SIZE) };
    drop(h);
    println!("[kernel] heap: {} MiB at {:#x}", HEAP_SIZE >> 20, HEAP_BASE);
}

/// `(size, used)` in bytes.
pub fn stats() -> (usize, usize) {
    let h = HEAP.0.lock();
    (h.0.size(), h.0.used())
}
