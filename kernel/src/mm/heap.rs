//! Kernel heap: a fixed 16 MiB window backed by frames at boot.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use linked_list_allocator::Heap;

use super::{HEAP_BASE, HEAP_SIZE, MapFlags, PAGE_SIZE, frame, paging};
use crate::sync::SpinLock;

struct HeapInner(Heap);
// SAFETY: the heap is only touched under the spinlock.
unsafe impl Send for HeapInner {}

struct KernelHeap(SpinLock<HeapInner>);

#[global_allocator]
static HEAP: KernelHeap = KernelHeap(SpinLock::new(HeapInner(Heap::empty())));

/// The most bytes in use at once since boot. Written under the heap lock, so a
/// plain load/store pair is enough; atomic only so `stats` needs no second lock.
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: standard first-fit allocator behind a lock.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut h = self.0.lock();
        let p = h.0.allocate_first_fit(layout).map(|p| p.as_ptr()).unwrap_or(core::ptr::null_mut());
        let used = h.0.used();
        if used > PEAK.load(Ordering::Relaxed) {
            PEAK.store(used, Ordering::Relaxed);
        }
        p
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
        let flags = MapFlags::WRITABLE | MapFlags::GLOBAL;
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

/// The most bytes that have been in use at once since boot.
pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Free space the kernel keeps for its own bookkeeping; user-driven allocations
/// are refused before they can eat into it (an OOM in the kernel heap would panic).
pub const HEADROOM: usize = 1024 * 1024;

/// Check that a user-driven allocation of about `bytes` leaves [`HEADROOM`] free.
pub fn reserve(bytes: usize) -> Result<(), spaceabi::error::Error> {
    let h = HEAP.0.lock();
    if h.0.free() >= bytes.saturating_add(HEADROOM) { Ok(()) } else { Err(spaceabi::error::Error::NoMemory) }
}
