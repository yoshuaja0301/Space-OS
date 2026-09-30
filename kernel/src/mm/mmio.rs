//! Device MMIO mappings.
//!
//! Device registers must not be cached: the linear map of physical memory is
//! write-back, so MMIO gets its own window mapped with `PCD` (cache disable) and
//! `PWT`, plus `NO_EXECUTE`. Mappings are permanent (devices live for the whole
//! kernel lifetime), so a bump allocator over the window is enough.

use spaceabi::error::Error;
use x86_64::PhysAddr;
use x86_64::structures::paging::{PageTableFlags, PhysFrame};

use super::{MMIO_BASE, MMIO_WINDOW, PAGE_SIZE, paging};
use crate::sync::SpinLock;

static NEXT: SpinLock<u64> = SpinLock::new(MMIO_BASE);

pub fn init() {
    println!("[kernel] mmio window: {} MiB at {:#x}", MMIO_WINDOW >> 20, MMIO_BASE);
}

/// Map `len` bytes of physical device memory and return its virtual address.
///
/// The returned address points at `phys` (the page offset is preserved).
pub fn map(phys: u64, len: u64) -> Result<u64, Error> {
    if len == 0 {
        return Err(Error::Invalid);
    }
    let offset = phys % PAGE_SIZE;
    let first = phys - offset;
    let pages = (offset + len).div_ceil(PAGE_SIZE);
    let mut next = NEXT.lock();
    let base = *next;
    let end = base.checked_add(pages * PAGE_SIZE).ok_or(Error::NoMemory)?;
    if end > MMIO_BASE + MMIO_WINDOW {
        return Err(Error::NoMemory);
    }
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::WRITE_THROUGH
        | PageTableFlags::GLOBAL
        | PageTableFlags::NO_EXECUTE;
    for i in 0..pages {
        let frame = PhysFrame::containing_address(PhysAddr::new(first + i * PAGE_SIZE));
        paging::map_kernel_page(base + i * PAGE_SIZE, frame, flags)?;
    }
    *next = end;
    Ok(base + offset)
}
