//! Page tables: the kernel's own PML4 and per-process address spaces.

use alloc::vec::Vec;

use spaceabi::boot::{BootInfo, PHYS_OFFSET};
use spaceabi::error::Error;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::mapper::{MappedFrame, TranslateResult};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use super::{PAGE_SIZE, USER_SPACE_END, frame};
use crate::sync::{SpinLock, StaticCell};

pub const HEAP_PML4_INDEX: usize = 288;
pub const KSTACK_PML4_INDEX: usize = 320;

/// Virtual address of a physical address inside the linear map.
pub fn phys_to_virt(pa: u64) -> VirtAddr {
    VirtAddr::new(PHYS_OFFSET + pa)
}

static KERNEL_PML4: StaticCell<u64> = StaticCell::new(0);
static KERNEL_MAP_LOCK: SpinLock<()> = SpinLock::new(());

pub struct KernelFrameAlloc;

// SAFETY: frames come from the global allocator and are zeroed; never handed out twice.
unsafe impl FrameAllocator<Size4KiB> for KernelFrameAlloc {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        frame::alloc_zeroed()
    }
}

fn table_at(phys: u64) -> &'static mut PageTable {
    // SAFETY: page tables live in RAM reachable through the linear map; callers
    // serialise access (single CPU, interrupts off or the map lock held).
    unsafe { &mut *phys_to_virt(phys).as_mut_ptr::<PageTable>() }
}

pub fn kernel_pml4() -> PhysFrame {
    // SAFETY: written once during init.
    PhysFrame::containing_address(PhysAddr::new(unsafe { *KERNEL_PML4.get() }))
}

pub fn init(bi: &BootInfo) {
    let new = frame::alloc_zeroed().expect("no frame for kernel PML4");
    let boot = table_at(bi.boot_pml4);
    let table = table_at(new.start_address().as_u64());
    for i in 256..512 {
        table[i] = boot[i].clone();
    }
    // Pre-populate the PDPTs of the heap and kernel-stack regions so that every
    // process PML4 (which copies slots 256..512) shares the same sub-tables.
    for idx in [HEAP_PML4_INDEX, KSTACK_PML4_INDEX] {
        let pdpt = frame::alloc_zeroed().expect("no frame for kernel PDPT");
        table[idx].set_frame(pdpt, PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
    }
    // SAFETY: single CPU, early boot.
    unsafe {
        *KERNEL_PML4.get_mut() = new.start_address().as_u64();
        Cr3::write(new, Cr3Flags::empty());
    }
    println!(
        "[kernel] paging: kernel PML4 at {:#x}; linear map {} GiB at {:#x}; boot identity map dropped",
        new.start_address(),
        bi.phys_map_end >> 30,
        PHYS_OFFSET
    );
}

/// Run `f` with a mapper over the kernel PML4 (upper-half mappings are shared by all
/// address spaces because their PML4 slots point at the same sub-tables).
pub fn with_kernel_mapper<R>(f: impl FnOnce(&mut OffsetPageTable<'static>) -> R) -> R {
    let _g = KERNEL_MAP_LOCK.lock();
    let table = table_at(kernel_pml4().start_address().as_u64());
    // SAFETY: the linear map is at PHYS_OFFSET and the table is the live kernel PML4.
    let mut mapper = unsafe { OffsetPageTable::new(table, VirtAddr::new(PHYS_OFFSET)) };
    f(&mut mapper)
}

pub fn map_kernel_page(va: u64, frame: PhysFrame, flags: PageTableFlags) -> Result<(), Error> {
    with_kernel_mapper(|m| {
        let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(va));
        // SAFETY: kernel-owned frame mapped at a kernel-owned virtual address.
        unsafe { m.map_to(page, frame, flags, &mut KernelFrameAlloc) }.map_err(|_| Error::NoMemory)?.flush();
        Ok(())
    })
}

pub fn unmap_kernel_page(va: u64) -> Option<PhysFrame> {
    with_kernel_mapper(|m| {
        let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(va));
        let (f, flush) = m.unmap(page).ok()?;
        flush.flush();
        Some(f)
    })
}

/// Translate through the *current* CR3 without taking locks (panic-safe, read only).
pub fn translate_current(va: u64) -> Option<u64> {
    let (cr3, _) = Cr3::read();
    let table = table_at(cr3.start_address().as_u64());
    // SAFETY: read-only walk of the live tables.
    let mapper = unsafe { OffsetPageTable::new(table, VirtAddr::new(PHYS_OFFSET)) };
    match mapper.translate(VirtAddr::new(va)) {
        TranslateResult::Mapped { frame, offset, .. } => Some(frame.start_address().as_u64() + offset),
        _ => None,
    }
}

/// Switch CR3 to `frame` unless it is already active.
pub fn activate_frame(frame: PhysFrame) {
    let (cur, _) = Cr3::read();
    if cur != frame {
        // SAFETY: only fully formed PML4s (kernel or process) are ever passed here.
        unsafe { Cr3::write(frame, Cr3Flags::empty()) };
    }
}

pub fn activate_kernel() {
    // SAFETY: the kernel PML4 maps everything the kernel needs.
    unsafe { Cr3::write(kernel_pml4(), Cr3Flags::empty()) };
}

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub start: u64,
    pub pages: usize,
}

/// A user address space: private lower half, shared kernel upper half.
pub struct AddressSpace {
    pml4: PhysFrame,
    regions: Vec<Region>,
    /// User pages mapped (charged against the process quota).
    pub used_pages: usize,
    mmap_next: u64,
    torn_down: bool,
}

pub const MMAP_BASE: u64 = 0x10_0000_0000;

impl AddressSpace {
    pub fn new() -> Result<Self, Error> {
        let pml4 = frame::alloc_zeroed().ok_or(Error::NoMemory)?;
        let kernel = table_at(kernel_pml4().start_address().as_u64());
        let table = table_at(pml4.start_address().as_u64());
        for i in 256..512 {
            table[i] = kernel[i].clone();
        }
        Ok(AddressSpace { pml4, regions: Vec::new(), used_pages: 0, mmap_next: MMAP_BASE, torn_down: false })
    }

    pub fn cr3(&self) -> PhysFrame {
        self.pml4
    }

    fn mapper(&mut self) -> OffsetPageTable<'_> {
        let table = table_at(self.pml4.start_address().as_u64());
        // SAFETY: linear map at PHYS_OFFSET; `&mut self` guarantees exclusivity.
        unsafe { OffsetPageTable::new(table, VirtAddr::new(PHYS_OFFSET)) }
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

    /// Map `pages` zero-filled, writable pages at a fresh anonymous address.
    ///
    /// The address cursor only advances when the mapping succeeds, so a failed
    /// attempt reuses the same address (and the page tables already built for it)
    /// instead of stranding page-table frames at every abandoned address.
    pub fn map_anonymous(&mut self, pages: usize) -> Result<u64, Error> {
        let start = self.mmap_next;
        let len = (pages as u64).checked_mul(PAGE_SIZE).ok_or(Error::Invalid)?;
        let end = start.checked_add(len).ok_or(Error::Invalid)?;
        if end > USER_SPACE_END / 2 {
            return Err(Error::NoMemory);
        }
        self.map_region(start, pages, true, false)?;
        self.mmap_next = end + PAGE_SIZE; // leave a guard gap between mappings
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
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if writable {
            flags |= PageTableFlags::WRITABLE;
        }
        if !executable {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        let mut mapped = 0usize;
        let mut mapper = self.mapper();
        let mut err = None;
        for i in 0..pages {
            let va = VirtAddr::new(start + i as u64 * PAGE_SIZE);
            let Some(frame) = frame::alloc_zeroed() else {
                err = Some(Error::NoMemory);
                break;
            };
            // SAFETY: fresh frame, user-space page in this process's tables.
            match unsafe {
                mapper.map_to(Page::<Size4KiB>::containing_address(va), frame, flags, &mut KernelFrameAlloc)
            } {
                Ok(flush) => {
                    flush.flush();
                    mapped += 1;
                }
                Err(_) => {
                    frame::free(frame);
                    err = Some(Error::NoMemory);
                    break;
                }
            }
        }
        if let Some(e) = err {
            for i in 0..mapped {
                let va = VirtAddr::new(start + i as u64 * PAGE_SIZE);
                if let Ok((f, flush)) = mapper.unmap(Page::<Size4KiB>::containing_address(va)) {
                    flush.flush();
                    frame::free(f);
                }
            }
            return Err(e);
        }
        self.regions.push(Region { start, pages });
        self.used_pages += pages;
        Ok(())
    }

    /// Unmap a region previously created by `map_region` (exact match required).
    pub fn unmap_region(&mut self, start: u64, pages: usize) -> Result<(), Error> {
        let idx =
            self.regions.iter().position(|r| r.start == start && r.pages == pages).ok_or(Error::Invalid)?;
        let mut mapper = self.mapper();
        for i in 0..pages {
            let va = VirtAddr::new(start + i as u64 * PAGE_SIZE);
            if let Ok((f, flush)) = mapper.unmap(Page::<Size4KiB>::containing_address(va)) {
                flush.flush();
                frame::free(f);
            }
        }
        self.regions.swap_remove(idx);
        self.used_pages -= pages;
        Ok(())
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
        let mapper = self.mapper();
        let mut page = addr & !(PAGE_SIZE - 1);
        while page < end {
            match mapper.translate(VirtAddr::new(page)) {
                TranslateResult::Mapped { frame: MappedFrame::Size4KiB(_), flags, .. } => {
                    if !flags.contains(PageTableFlags::USER_ACCESSIBLE) {
                        return false;
                    }
                    if write && !flags.contains(PageTableFlags::WRITABLE) {
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
        let mapper = self.mapper();
        let mut off = 0usize;
        while off < data.len() {
            let cur = va + off as u64;
            let TranslateResult::Mapped { frame: MappedFrame::Size4KiB(f), offset, .. } =
                mapper.translate(VirtAddr::new(cur))
            else {
                return Err(Error::Fault);
            };
            let chunk = ((PAGE_SIZE - offset) as usize).min(data.len() - off);
            let dst = phys_to_virt(f.start_address().as_u64() + offset).as_mut_ptr::<u8>();
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
        while let Some(r) = self.regions.pop() {
            let mut mapper = self.mapper();
            for i in 0..r.pages {
                let va = VirtAddr::new(r.start + i as u64 * PAGE_SIZE);
                if let Ok((f, flush)) = mapper.unmap(Page::<Size4KiB>::containing_address(va)) {
                    flush.ignore();
                    frame::free(f);
                }
            }
        }
        self.used_pages = 0;
        let pml4 = table_at(self.pml4.start_address().as_u64());
        for i in 0..256 {
            let e = &pml4[i];
            if e.is_unused() {
                continue;
            }
            let pdpt_phys = e.addr().as_u64();
            let pdpt = table_at(pdpt_phys);
            for j in 0..512 {
                let e2 = &pdpt[j];
                if e2.is_unused() || e2.flags().contains(PageTableFlags::HUGE_PAGE) {
                    continue;
                }
                let pd_phys = e2.addr().as_u64();
                let pd = table_at(pd_phys);
                for k in 0..512 {
                    let e3 = &pd[k];
                    if e3.is_unused() || e3.flags().contains(PageTableFlags::HUGE_PAGE) {
                        continue;
                    }
                    frame::free(PhysFrame::containing_address(e3.addr()));
                }
                frame::free(PhysFrame::containing_address(PhysAddr::new(pd_phys)));
            }
            frame::free(PhysFrame::containing_address(PhysAddr::new(pdpt_phys)));
            pml4[i].set_unused();
        }
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        self.teardown();
        let (cur, _) = Cr3::read();
        assert!(cur != self.pml4, "dropping the active address space");
        frame::free(self.pml4);
    }
}
