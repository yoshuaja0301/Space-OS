//! Floating-point and vector registers belong to the thread that wrote them
//! (ADR-0031, PRD v0.2 K03).
//!
//! Every user thread has an area its FP/SIMD state is kept in while it is not on a
//! CPU. The scheduler saves the leaving thread's registers there and loads the next
//! thread's on every switch -- eagerly, with no "first use" trap: a register a
//! thread did not write never holds what another thread left in it. A new thread's
//! area starts as the architecture's initial state (every register zero; x87
//! control word 0x37F and MXCSR 0x1F80 on x86-64, FPCR and FPSR zero on AArch64).
//! Idle threads run only kernel code, which is built soft-float and never touches
//! these units, so they have no area.

use core::alloc::Layout;
use core::ptr::NonNull;

use spaceabi::error::Error;

use crate::arch;

pub struct FpuArea {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: the area is plain memory. It is written by the CPU switching away from its
// thread and read by the CPU switching to it, which the scheduler orders (a thread
// is queued again only after the switch away from it is complete).
unsafe impl Send for FpuArea {}
unsafe impl Sync for FpuArea {}

impl FpuArea {
    /// A new thread's area, in the initial state.
    pub fn new() -> Result<FpuArea, Error> {
        let (size, align) = arch::fpu::area_layout();
        let layout = Layout::from_size_align(size, align).map_err(|_| Error::Invalid)?;
        crate::mm::heap::reserve(size)?;
        // SAFETY: a non-zero size; a null result is a refusal, not a panic.
        let ptr = NonNull::new(unsafe { alloc::alloc::alloc(layout) }).ok_or(Error::NoMemory)?;
        // SAFETY: `ptr` is `size` bytes, aligned as the architecture asks.
        unsafe { arch::fpu::initial_state(ptr.as_ptr(), size) };
        Ok(FpuArea { ptr, layout })
    }

    /// Store this CPU's FP/SIMD registers in the area.
    ///
    /// # Safety
    /// Only on the CPU that is switching away from the area's thread, before the
    /// thread can be picked up anywhere else.
    pub unsafe fn save(&self) {
        // SAFETY: as the caller promises; the area is the right size and alignment.
        unsafe { arch::fpu::save(self.ptr.as_ptr()) }
    }

    /// Load this CPU's FP/SIMD registers from the area.
    ///
    /// # Safety
    /// Only on the CPU that is switching to the area's thread.
    pub unsafe fn restore(&self) {
        // SAFETY: as above.
        unsafe { arch::fpu::restore(self.ptr.as_ptr()) }
    }
}

impl Drop for FpuArea {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}
