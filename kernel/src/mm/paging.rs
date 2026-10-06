//! Page tables: the kernel's own and per-process address spaces. The walk is
//! `pt`; this module decides what goes where.

use alloc::sync::Arc;
use alloc::vec::Vec;

use spaceabi::boot::{BootInfo, PHYS_OFFSET};
use spaceabi::error::Error;
use x86_64::structures::paging::PhysFrame;
use x86_64::{PhysAddr, VirtAddr};

use super::{MapFlags, PAGE_SIZE, USER_SPACE_END, frame, pt};
use crate::arch::mmu;
use crate::proc::handles::MemoryObject;
use crate::sync::{SpinLock, StaticCell};

pub const HEAP_PML4_INDEX: usize = 288;
pub const KSTACK_PML4_INDEX: usize = 320;
pub const MMIO_PML4_INDEX: usize = 352;

/// Virtual address of a physical address inside the linear map.
pub fn phys_to_virt(pa: u64) -> VirtAddr {
    VirtAddr::new(PHYS_OFFSET + pa)
}

static KERNEL_PML4: StaticCell<u64> = StaticCell::new(0);
static KERNEL_MAP_LOCK: SpinLock<()> = SpinLock::new(());

fn kernel_root() -> u64 {
    // SAFETY: written once during init.
    unsafe { *KERNEL_PML4.get() }
}

#[cfg(target_arch = "x86_64")]
pub fn kernel_pml4() -> PhysFrame {
    PhysFrame::containing_address(PhysAddr::new(kernel_root()))
}

pub fn init(bi: &BootInfo) {
    let new = frame::alloc_zeroed().expect("no frame for kernel PML4").start_address().as_u64();
    // SAFETY: the bootloader's root and a fresh frame; single CPU, early boot.
    let (boot, table) = unsafe { (pt::table(bi.boot_pml4), pt::table(new)) };
    table[256..512].copy_from_slice(&boot[256..512]);
    // Pre-populate the PDPTs of the heap and kernel-stack regions so that every
    // process PML4 (which copies slots 256..512) shares the same sub-tables.
    for idx in [HEAP_PML4_INDEX, KSTACK_PML4_INDEX, MMIO_PML4_INDEX] {
        let pdpt = frame::alloc_zeroed().expect("no frame for kernel PDPT").start_address().as_u64();
        mmu::write_entry(&mut table[idx], mmu::table_entry(pdpt, MapFlags::WRITABLE));
    }
    // SAFETY: single CPU, early boot.
    unsafe { *KERNEL_PML4.get_mut() = new };
    mmu::install_kernel_root(new);
    println!(
        "[kernel] paging: kernel PML4 at {:#x}; linear map {} GiB at {:#x}; boot identity map dropped",
        new,
        bi.phys_map_end >> 30,
        PHYS_OFFSET
    );
}

pub fn map_kernel_page(va: u64, frame: PhysFrame, flags: MapFlags) -> Result<(), Error> {
    let _g = KERNEL_MAP_LOCK.lock();
    pt::map(kernel_root(), va, frame.start_address().as_u64(), flags).map_err(|_| Error::NoMemory)
}

pub fn unmap_kernel_page(va: u64) -> Option<PhysFrame> {
    let _g = KERNEL_MAP_LOCK.lock();
    pt::unmap(kernel_root(), va).map(|pa| PhysFrame::containing_address(PhysAddr::new(pa)))
}

/// Translate through the live tables without taking locks (panic-safe, read only).
pub fn translate_current(va: u64) -> Option<u64> {
    pt::translate(mmu::root_for(va), va).map(|(pa, _, _)| pa)
}

/// How much of the address space around `va` one mapping covers, if `va` is
/// mapped: 4 KiB for a page, 2 MiB for a block (larger blocks are counted in
/// 2 MiB steps). Read only, no locks, like [`translate_current`].
pub fn mapped_extent(va: u64) -> Option<u64> {
    pt::translate(mmu::root_for(va), va).map(|(_, _, small)| if small { PAGE_SIZE } else { 2 << 20 })
}

/// Switch to `frame`'s address space, even when it is already there: the scheduler
/// relies on every switch flushing what this CPU cached of other address spaces
/// (ADR-0024).
pub fn load_cr3(frame: PhysFrame) {
    mmu::load_user_root(frame.start_address().as_u64());
}

/// Leave whatever user address space this CPU is in: only the kernel stays mapped.
pub fn activate_kernel() {
    mmu::load_user_root(kernel_root());
}

pub struct Region {
    pub start: u64,
    pub pages: usize,
    /// Private pages (ELF image, anonymous memory) own their frames and free them
    /// on unmap, so `object` is `None`. Pages backed by a shared memory object hold
    /// a reference to it instead: the frames belong to the object, and keeping the
    /// reference here means the object outlives every mapping of it. Without that a
    /// process could map a buffer, drop the last handle, and keep writing to frames
    /// the allocator had already handed to somebody else.
    pub object: Option<Arc<MemoryObject>>,
}

impl Region {
    /// True when unmapping this region must return its frames to the allocator.
    fn owns_frames(&self) -> bool {
        self.object.is_none()
    }
}

/// A user address space: private lower half, shared kernel upper half.
pub struct AddressSpace {
    pml4: PhysFrame,
    regions: Vec<Region>,
    /// User pages mapped (charged against the process quota).
    pub used_pages: usize,
    torn_down: bool,
}

pub const MMAP_BASE: u64 = 0x10_0000_0000;

impl AddressSpace {
    pub fn new() -> Result<Self, Error> {
        let pml4 = frame::alloc_zeroed().ok_or(Error::NoMemory)?;
        // SAFETY: the live kernel root and a fresh frame nobody else has seen.
        let (kernel, table) = unsafe { (pt::table(kernel_root()), pt::table(pml4.start_address().as_u64())) };
        table[256..512].copy_from_slice(&kernel[256..512]);
        Ok(AddressSpace { pml4, regions: Vec::new(), used_pages: 0, torn_down: false })
    }

    pub fn cr3(&self) -> PhysFrame {
        self.pml4
    }

    fn root(&self) -> u64 {
        self.pml4.start_address().as_u64()
    }

    fn range_ok(start: u64, pages: usize) -> bool {
        let len = (pages as u64).checked_mul(PAGE_SIZE);
        match len {
            Some(len) => {
                start.is_multiple_of(PAGE_SIZE)
                    && pages > 0
                    && start.checked_add(len).is_some_and(|e| e <= USER_SPACE_END)
            }
            None => false,
        }
    }

    fn overlaps(&self, start: u64, pages: usize) -> bool {
        let end = start + pages as u64 * PAGE_SIZE;
        self.regions.iter().any(|r| {
            let rend = r.start + r.pages as u64 * PAGE_SIZE;
            start < rend && r.start < end
        })
    }

    /// The lowest address from [`MMAP_BASE`] up where `pages` pages fit with an
    /// unmapped guard page on either side.
    ///
    /// Addresses are reused once unmapped, and with them the page tables already
    /// built there. A cursor that only moved on would add a page table for every
    /// 2 MiB a process ever mapped and keep all of them until it exits: a program
    /// that maps and unmaps for hours would grow without holding anything.
    fn free_range(&self, pages: usize) -> Result<u64, Error> {
        let len = (pages as u64).checked_mul(PAGE_SIZE).ok_or(Error::Invalid)?;
        let mut start = MMAP_BASE;
        loop {
            let end = start.checked_add(len).ok_or(Error::Invalid)?;
            if end > USER_SPACE_END / 2 {
                return Err(Error::NoMemory);
            }
            let clash = self
                .regions
                .iter()
                .map(|r| (r.start, r.start + r.pages as u64 * PAGE_SIZE))
                .filter(|&(rs, re)| start < re + PAGE_SIZE && rs < end + PAGE_SIZE)
                .map(|(_, re)| re)
                .max();
            match clash {
                None => return Ok(start),
                Some(re) => start = re + PAGE_SIZE,
            }
        }
    }

    /// Map `pages` zero-filled, writable pages at a free anonymous address.
    pub fn map_anonymous(&mut self, pages: usize) -> Result<u64, Error> {
        let start = self.free_range(pages)?;
        self.map_region(start, pages, true, false)?;
        Ok(start)
    }

    /// Map `pages` zero-filled frames at `start`. Rolls back completely on failure.
    /// Quota is checked by the caller (`proc`), which knows the process.
    pub fn map_region(
        &mut self,
        start: u64,
        pages: usize,
        writable: bool,
        executable: bool,
    ) -> Result<(), Error> {
        if !Self::range_ok(start, pages) {
            return Err(Error::Invalid);
        }
        if self.overlaps(start, pages) {
            return Err(Error::Invalid);
        }
        // Reserve the bookkeeping slot up front so a failed push cannot leave frames mapped.
        self.regions.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let mut flags = MapFlags::USER;
        if writable {
            flags |= MapFlags::WRITABLE;
        }
        if executable {
            flags |= MapFlags::EXECUTABLE;
        }
        let root = self.root();
        let mut mapped = 0usize;
        let mut err = None;
        for i in 0..pages {
            let va = start + i as u64 * PAGE_SIZE;
            let Some(frame) = frame::alloc_zeroed() else {
                err = Some(Error::NoMemory);
                break;
            };
            match pt::map(root, va, frame.start_address().as_u64(), flags) {
                Ok(()) => mapped += 1,
                Err(_) => {
                    frame::free(frame);
                    err = Some(Error::NoMemory);
                    break;
                }
            }
        }
        if let Some(e) = err {
            for i in 0..mapped {
                if let Some(f) = pt::unmap(root, start + i as u64 * PAGE_SIZE) {
                    frame::free_phys(f);
                }
            }
            return Err(e);
        }
        self.regions.push(Region { start, pages, object: None });
        self.used_pages += pages;
        Ok(())
    }

    /// Map the frames of a shared memory object at a fresh address.
    ///
    /// The frames belong to the object, so they are not freed on unmap and are not
    /// charged again here: the process that created the object already paid for
    /// them against its quota.
    pub fn map_shared(&mut self, object: Arc<MemoryObject>, writable: bool) -> Result<u64, Error> {
        if object.frames.is_empty() {
            return Err(Error::Invalid);
        }
        let pages = object.frames.len();
        let start = self.free_range(pages)?;
        self.regions.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let mut flags = MapFlags::USER;
        if writable {
            flags |= MapFlags::WRITABLE;
        }
        let root = self.root();
        let mut mapped = 0usize;
        let mut err = None;
        for (i, frame) in object.frames.iter().enumerate() {
            match pt::map(root, start + i as u64 * PAGE_SIZE, frame.start_address().as_u64(), flags) {
                Ok(()) => mapped += 1,
                Err(_) => {
                    err = Some(Error::NoMemory);
                    break;
                }
            }
        }
        if let Some(e) = err {
            for i in 0..mapped {
                pt::unmap(root, start + i as u64 * PAGE_SIZE);
            }
            return Err(e);
        }
        self.regions.push(Region { start, pages, object: Some(object) });
        Ok(start)
    }

    /// Unmap a region previously created by `map_region` (exact match required).
    /// Returns the memory object the region was backed by, if any. The caller drops
    /// it *after* releasing the address-space lock: the last drop refunds the
    /// creator's quota, and that creator may be this very process.
    pub fn unmap_region(&mut self, start: u64, pages: usize) -> Result<Option<Arc<MemoryObject>>, Error> {
        let idx =
            self.regions.iter().position(|r| r.start == start && r.pages == pages).ok_or(Error::Invalid)?;
        let owned = self.regions[idx].owns_frames();
        let root = self.root();
        for i in 0..pages {
            if let Some(f) = pt::unmap(root, start + i as u64 * PAGE_SIZE)
                && owned
            {
                frame::free_phys(f);
            }
        }
        let region = self.regions.swap_remove(idx);
        if owned {
            self.used_pages -= pages;
        }
        Ok(region.object)
    }

    /// Verify that `[addr, addr+len)` is user memory mapped with the needed access.
    pub fn check_user_range(&mut self, addr: u64, len: u64, write: bool) -> bool {
        let Some(end) = addr.checked_add(len) else { return false };
        if end > USER_SPACE_END {
            return false;
        }
        if len == 0 {
            return true;
        }
        let root = self.root();
        let mut page = addr & !(PAGE_SIZE - 1);
        while page < end {
            match pt::translate(root, page) {
                Some((_, flags, true)) => {
                    if !flags.contains(MapFlags::USER) {
                        return false;
                    }
                    if write && !flags.contains(MapFlags::WRITABLE) {
                        return false;
                    }
                }
                _ => return false,
            }
            page += PAGE_SIZE;
        }
        true
    }

    /// Copy `data` into user memory that was just mapped (used by the ELF loader,
    /// before the space is ever active). Goes through the linear map.
    pub fn write_initial(&mut self, va: u64, data: &[u8]) -> Result<(), Error> {
        let root = self.root();
        let mut off = 0usize;
        while off < data.len() {
            let cur = va + off as u64;
            let Some((pa, _, true)) = pt::translate(root, cur) else {
                return Err(Error::Fault);
            };
            let offset = pa & (PAGE_SIZE - 1);
            let chunk = ((PAGE_SIZE - offset) as usize).min(data.len() - off);
            let dst = phys_to_virt(pa).as_mut_ptr::<u8>();
            // SAFETY: `chunk` bytes inside one mapped frame of this address space.
            unsafe { core::ptr::copy_nonoverlapping(data[off..].as_ptr(), dst, chunk) };
            off += chunk;
        }
        Ok(())
    }

    /// Free every user page and every lower-half page table. Idempotent.
    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        // Pop instead of collecting into a fresh Vec: process exit must never need
        // the kernel heap (an allocation failure there would panic the kernel).
        let root = self.root();
        while let Some(r) = self.regions.pop() {
            for i in 0..r.pages {
                if let Some(f) = pt::unmap(root, r.start + i as u64 * PAGE_SIZE)
                    && r.owns_frames()
                {
                    frame::free_phys(f);
                }
            }
        }
        self.used_pages = 0;
        pt::free_tables(root, 0..256);
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        self.teardown();
        assert!(!mmu::user_root_active(self.root()), "dropping the active address space");
        frame::free(self.pml4);
    }
}
