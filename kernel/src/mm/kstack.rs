//! Kernel stacks: 64 KiB virtual slots, 32 KiB mapped at the top, the rest is a
//! guard so an overflow hits an unmapped page (→ double fault on the IST stack)
//! instead of silently corrupting memory.

use spaceabi::error::Error;
use x86_64::structures::paging::PageTableFlags;

use super::{KSTACK_BASE, PAGE_SIZE, frame, paging};
use crate::sync::SpinLock;

pub const SLOT_SIZE: u64 = 64 * 1024;
pub const STACK_PAGES: u64 = 8;
const MAX_SLOTS: usize = 4096;

static SLOTS: SpinLock<[u64; MAX_SLOTS / 64]> = SpinLock::new([0; MAX_SLOTS / 64]);

pub fn init() {
    println!(
        "[kernel] kernel stacks: {} slots of {} KiB ({} KiB mapped) at {:#x}",
        MAX_SLOTS,
        SLOT_SIZE / 1024,
        STACK_PAGES * PAGE_SIZE / 1024,
        KSTACK_BASE
    );
}

pub struct KernelStack {
    slot: usize,
    pub top: u64,
}

impl KernelStack {
    pub fn new() -> Result<Self, Error> {
        let slot = {
            let mut s = SLOTS.lock();
            let mut found = None;
            for (w, word) in s.iter_mut().enumerate() {
                if *word != u64::MAX {
                    let bit = word.trailing_ones() as usize;
                    *word |= 1 << bit;
                    found = Some(w * 64 + bit);
                    break;
                }
            }
            found.ok_or(Error::NoMemory)?
        };
        let top = KSTACK_BASE + (slot as u64 + 1) * SLOT_SIZE;
        let bottom = top - STACK_PAGES * PAGE_SIZE;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        for i in 0..STACK_PAGES {
            let va = bottom + i * PAGE_SIZE;
            let Some(f) = frame::alloc_zeroed() else {
                Self::release(slot, i);
                return Err(Error::NoMemory);
            };
            if paging::map_kernel_page(va, f, flags).is_err() {
                frame::free(f);
                Self::release(slot, i);
                return Err(Error::NoMemory);
            }
        }
        Ok(KernelStack { slot, top })
    }

    fn release(slot: usize, mapped_pages: u64) {
        let top = KSTACK_BASE + (slot as u64 + 1) * SLOT_SIZE;
        let bottom = top - STACK_PAGES * PAGE_SIZE;
        for i in 0..mapped_pages {
            if let Some(f) = paging::unmap_kernel_page(bottom + i * PAGE_SIZE) {
                frame::free(f);
            }
        }
        let mut s = SLOTS.lock();
        s[slot / 64] &= !(1u64 << (slot % 64));
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        Self::release(self.slot, STACK_PAGES);
    }
}
