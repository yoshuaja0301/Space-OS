//! x86-64: 4-level page tables built with the `x86_64` crate, and the jump to the
//! kernel with `rdi = &BootInfo`.

use core::arch::asm;

use spaceabi::boot::{FramebufferInfo, PHYS_OFFSET, uart_kind};
use uefi::mem::memory_map::MemoryMapOwned;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr3Flags, Cr4, Cr4Flags, Efer, EferFlags};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size2MiB, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::{HUGE, LoadedKernel, alloc_kernel_pages, backs_ram};

pub const KERNEL_ELF_ERROR: &str = "kernel is not a valid ELF64 x86-64 executable";

pub fn check_environment() -> Result<(), &'static str> {
    // The page tables built below are 4-level. If the firmware runs with 5-level
    // paging (LA57) the CPU would walk our PML4 as a PML5 and triple-fault right
    // after the CR3 switch, so refuse with a message instead.
    if Cr4::read().contains(Cr4Flags::L5_PAGING) {
        return Err("firmware runs with 5-level paging (CR4.LA57); spaceboot supports 4-level paging only");
    }
    Ok(())
}

/// The console is the 16550 at COM1, which the kernel reaches by port: nothing to
/// hand over.
pub fn console_uart(_rsdp: u64) -> (u64, u32) {
    (0, uart_kind::NONE)
}

pub struct Tables {
    pub root: u64,
    pub mapped_huge: u64,
}

struct BootFrameAllocator;

// SAFETY: frames come from UEFI AllocatePages and are never handed out twice.
unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let phys = alloc_kernel_pages(1).ok()?;
        Some(PhysFrame::containing_address(PhysAddr::new(phys)))
    }
}

/// The kernel image at its link address, RAM at `PHYS_OFFSET` and identity-mapped.
pub fn build_tables(
    kernel: &LoadedKernel,
    prelim: &MemoryMapOwned,
    fb: &FramebufferInfo,
    phys_map_end: u64,
    _uart: u64,
) -> Result<Tables, &'static str> {
    let mut falloc = BootFrameAllocator;
    let pml4_frame = falloc.allocate_frame().ok_or("cannot allocate PML4")?;
    // SAFETY: UEFI identity-maps memory, so the physical address is also the virtual one.
    let pml4: &mut PageTable = unsafe { &mut *(pml4_frame.start_address().as_u64() as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(pml4, VirtAddr::new(0)) };

    for p in &kernel.pages {
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::GLOBAL;
        if p.writable {
            flags |= PageTableFlags::WRITABLE;
        }
        if !p.executable {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(p.va));
        let frame = PhysFrame::containing_address(PhysAddr::new(p.pa));
        // SAFETY: fresh page tables; the kernel image frames are exclusively ours.
        unsafe { mapper.map_to(page, frame, flags, &mut falloc) }
            .map_err(|_| "map kernel page failed")?
            .ignore();
    }
    let linear_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::HUGE_PAGE
        | PageTableFlags::GLOBAL
        | PageTableFlags::NO_EXECUTE;
    let identity_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::HUGE_PAGE;
    let mut pa = 0u64;
    let mut mapped_huge = 0u64;
    while pa < phys_map_end {
        if !backs_ram(prelim, pa, fb) {
            pa += HUGE;
            continue;
        }
        let frame: PhysFrame<Size2MiB> = PhysFrame::containing_address(PhysAddr::new(pa));
        let linear: Page<Size2MiB> = Page::containing_address(VirtAddr::new(PHYS_OFFSET + pa));
        let ident: Page<Size2MiB> = Page::containing_address(VirtAddr::new(pa));
        // SAFETY: fresh page tables; mapping physical memory at a kernel-owned window.
        unsafe {
            mapper.map_to(linear, frame, linear_flags, &mut falloc).map_err(|_| "linear map")?.ignore();
            mapper.map_to(ident, frame, identity_flags, &mut falloc).map_err(|_| "identity map")?.ignore();
        }
        mapped_huge += 1;
        pa += HUGE;
    }
    Ok(Tables { root: pml4_frame.start_address().as_u64(), mapped_huge })
}

/// Switch to the new tables and enter the kernel. Boot services are gone by now.
pub fn jump(tables: &Tables, kernel: &LoadedKernel, stack_top: u64, bootinfo_virt: u64) -> ! {
    let pml4 = PhysFrame::containing_address(PhysAddr::new(tables.root));
    // SAFETY: the new tables identity-map the code we are executing and the UEFI
    // stack; NXE/WP are enabled before pages with NX bits become active.
    unsafe {
        asm!("cli", options(nomem, nostack));
        Efer::write(Efer::read() | EferFlags::NO_EXECUTE_ENABLE);
        Cr0::write(Cr0::read() | Cr0Flags::WRITE_PROTECT);
        Cr3::write(pml4, Cr3Flags::empty());
        // Operands are pinned to explicit registers so that no instruction in the
        // sequence can clobber another operand (an `in(reg)` could have been
        // allocated to rdi or rbp).
        asm!(
            "mov rsp, rcx",
            "xor ebp, ebp",
            "jmp rax",
            in("rcx") stack_top,
            in("rdi") bootinfo_virt,
            in("rax") kernel.entry,
            options(noreturn)
        );
    }
}
