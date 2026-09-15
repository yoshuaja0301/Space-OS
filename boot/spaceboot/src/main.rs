//! `spaceboot` – the Space OS UEFI bootloader.
//!
//! Responsibilities (see docs/adr/0002-uefi-bootloader.md):
//! 1. load `\EFI\SPACEOS\spacekernel.elf`, `initrd.tar` and `spaceos.cfg` from the ESP;
//! 2. place the kernel image, initrd, command line, boot stack and boot info in
//!    memory typed `MemKind::KERNEL` so the kernel never reclaims them by accident;
//! 3. build page tables: kernel at its link address, physical memory linear map at
//!    `PHYS_OFFSET`, plus a temporary identity map;
//! 4. capture GOP framebuffer, ACPI RSDP and the final UEFI memory map;
//! 5. exit boot services and jump to the kernel entry with `rdi = &BootInfo`.
//!
//! Nothing here depends on the kernel besides the `spaceabi::boot` contract.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use core::mem;

use spaceabi::boot::{
    BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BOOT_STACK_SIZE, BootInfo, FramebufferInfo, MemRegion, PHYS_OFFSET,
    PhysRange, fb_format, mem_kind,
};
use spaceabi::elf::Elf;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::{MemoryDescriptor, MemoryMap};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::table::cfg::ACPI2_GUID;
use uefi::{CStr16, Status, cstr16, println};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr3Flags, Efer, EferFlags};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size2MiB, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

const KERNEL_PATH: &CStr16 = cstr16!("\\EFI\\SPACEOS\\spacekernel.elf");
const INITRD_PATH: &CStr16 = cstr16!("\\EFI\\SPACEOS\\initrd.tar");
const CONFIG_PATH: &CStr16 = cstr16!("\\EFI\\SPACEOS\\spaceos.cfg");

/// UEFI memory type for everything the kernel must keep (OEM range 0x8000_0000+).
const MT_KERNEL: MemoryType = MemoryType::custom(0x8000_0000);

const PAGE: u64 = 4096;
const HUGE: u64 = 2 * 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;
/// Number of pages reserved for the memory-region array handed to the kernel.
const MEMMAP_PAGES: usize = 16;

#[uefi::entry]
fn main() -> Status {
    uefi::helpers::init().expect("uefi helpers");
    println!("spaceboot {}: Space OS UEFI bootloader", env!("CARGO_PKG_VERSION"));
    match run() {
        Ok(()) => Status::SUCCESS,
        Err(msg) => {
            println!("spaceboot: FATAL: {msg}");
            boot::stall(5_000_000);
            Status::LOAD_ERROR
        }
    }
}

/// Zero-filled page allocation of the `MT_KERNEL` type.
fn alloc_kernel_pages(pages: usize) -> Result<u64, &'static str> {
    let ptr = boot::allocate_pages(AllocateType::AnyPages, MT_KERNEL, pages)
        .map_err(|_| "allocate_pages failed")?;
    // SAFETY: freshly allocated, `pages * PAGE` bytes, exclusively ours.
    unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0, pages * PAGE as usize) };
    Ok(ptr.as_ptr() as u64)
}

struct BootFrameAllocator;

// SAFETY: frames come from UEFI AllocatePages and are never handed out twice.
unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let phys = alloc_kernel_pages(1).ok()?;
        Some(PhysFrame::containing_address(PhysAddr::new(phys)))
    }
}

struct LoadedKernel {
    entry: u64,
    image: PhysRange,
    /// (virtual page address, physical page address, flags) per 4 KiB page.
    pages: Vec<(u64, u64, PageTableFlags)>,
}

fn load_kernel(data: &[u8]) -> Result<LoadedKernel, &'static str> {
    let elf = Elf::parse(data).map_err(|_| "kernel is not a valid ELF64 x86-64 executable")?;
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    let mut segs = Vec::new();
    for seg in elf.load_segments() {
        let seg = seg.map_err(|_| "kernel ELF segment out of bounds")?;
        if seg.memsz == 0 {
            continue;
        }
        lo = lo.min(seg.vaddr & !(PAGE - 1));
        hi = hi.max((seg.vaddr + seg.memsz).div_ceil(PAGE) * PAGE);
        segs.push(seg);
    }
    if segs.is_empty() || lo < spaceabi::boot::KERNEL_VIRT_BASE {
        return Err("kernel has no loadable segments in the higher half");
    }
    let total_pages = ((hi - lo) / PAGE) as usize;
    let phys_base = alloc_kernel_pages(total_pages)?;
    let mut pages = Vec::with_capacity(total_pages);
    for seg in &segs {
        let dst = phys_base + (seg.vaddr - lo);
        let src = elf.segment_data(seg);
        // SAFETY: dst..dst+memsz lies inside the allocation (checked via lo/hi).
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len()) };
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::GLOBAL;
        if seg.writable() {
            flags |= PageTableFlags::WRITABLE;
        }
        if !seg.executable() {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        let first = seg.vaddr & !(PAGE - 1);
        let last = (seg.vaddr + seg.memsz).div_ceil(PAGE) * PAGE;
        let mut va = first;
        while va < last {
            pages.push((va, phys_base + (va - lo), flags));
            va += PAGE;
        }
    }
    println!(
        "spaceboot: kernel {} segments, {} KiB at phys {:#x}, entry {:#x}",
        segs.len(),
        total_pages * 4,
        phys_base,
        elf.entry
    );
    Ok(LoadedKernel { entry: elf.entry, image: PhysRange { phys: phys_base, len: hi - lo }, pages })
}

/// Copy `data` into fresh `MT_KERNEL` pages.
fn stash(data: &[u8]) -> Result<PhysRange, &'static str> {
    if data.is_empty() {
        return Ok(PhysRange::default());
    }
    let pages = data.len().div_ceil(PAGE as usize);
    let phys = alloc_kernel_pages(pages)?;
    // SAFETY: allocation is at least data.len() bytes.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), phys as *mut u8, data.len()) };
    Ok(PhysRange { phys, len: data.len() as u64 })
}

fn parse_cmdline(cfg: &[u8]) -> &[u8] {
    for line in cfg.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(v) = line.strip_prefix(b"cmdline=") {
            return v;
        }
    }
    &[]
}

fn framebuffer_info() -> FramebufferInfo {
    let Ok(handle) = boot::get_handle_for_protocol::<GraphicsOutput>() else {
        return FramebufferInfo::default();
    };
    let Ok(mut gop) = boot::open_protocol_exclusive::<GraphicsOutput>(handle) else {
        return FramebufferInfo::default();
    };
    let mode = gop.current_mode_info();
    let (width, height) = mode.resolution();
    let format = match mode.pixel_format() {
        PixelFormat::Rgb => fb_format::RGBX,
        PixelFormat::Bgr => fb_format::BGRX,
        _ => fb_format::OTHER,
    };
    let mut fb = gop.frame_buffer();
    FramebufferInfo {
        present: 1,
        format,
        width: width as u32,
        height: height as u32,
        stride: mode.stride() as u32,
        bytes_per_pixel: 4,
        phys_addr: fb.as_mut_ptr() as u64,
        size: fb.size() as u64,
    }
}

/// Memory types that are RAM (as opposed to MMIO or firmware-reserved holes).
fn is_ram(ty: MemoryType) -> bool {
    matches!(
        region_kind(ty),
        mem_kind::USABLE
            | mem_kind::BOOTLOADER_RECLAIMABLE
            | mem_kind::KERNEL
            | mem_kind::ACPI_RECLAIMABLE
            | mem_kind::ACPI_NVS
    ) || matches!(ty, MemoryType::RUNTIME_SERVICES_CODE | MemoryType::RUNTIME_SERVICES_DATA)
}

fn region_kind(ty: MemoryType) -> u32 {
    match ty {
        MemoryType::CONVENTIONAL => mem_kind::USABLE,
        MemoryType::BOOT_SERVICES_CODE
        | MemoryType::BOOT_SERVICES_DATA
        | MemoryType::LOADER_CODE
        | MemoryType::LOADER_DATA => mem_kind::BOOTLOADER_RECLAIMABLE,
        MT_KERNEL => mem_kind::KERNEL,
        MemoryType::ACPI_RECLAIM => mem_kind::ACPI_RECLAIMABLE,
        MemoryType::ACPI_NON_VOLATILE => mem_kind::ACPI_NVS,
        MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => mem_kind::MMIO,
        MemoryType::UNUSABLE => mem_kind::BAD,
        _ => mem_kind::RESERVED,
    }
}

fn run() -> Result<(), &'static str> {
    // ---- 1. files ---------------------------------------------------------
    let fs_proto =
        boot::get_image_file_system(boot::image_handle()).map_err(|_| "cannot open the boot volume")?;
    let mut fs = uefi::fs::FileSystem::new(fs_proto);
    let kernel_data = fs.read(KERNEL_PATH).map_err(|_| "spacekernel.elf not found on ESP")?;
    let initrd_data = fs.read(INITRD_PATH).unwrap_or_default();
    let cfg_data = fs.read(CONFIG_PATH).unwrap_or_default();
    println!(
        "spaceboot: kernel {} bytes, initrd {} bytes, config {} bytes",
        kernel_data.len(),
        initrd_data.len(),
        cfg_data.len()
    );
    drop(fs);

    // ---- 2. place kernel-owned data ---------------------------------------
    let kernel = load_kernel(&kernel_data)?;
    let initrd = stash(&initrd_data)?;
    let cmdline = stash(parse_cmdline(&cfg_data))?;
    let stack_phys = alloc_kernel_pages(BOOT_STACK_SIZE / PAGE as usize)?;
    let bootinfo_phys = alloc_kernel_pages(1)?;
    let memmap_phys = alloc_kernel_pages(MEMMAP_PAGES)?;
    let rsdp = uefi::system::with_config_table(|entries| {
        entries.iter().find(|e| e.guid == ACPI2_GUID).map(|e| e.address as u64).unwrap_or(0)
    });
    mem::forget(kernel_data);
    mem::forget(initrd_data);
    mem::forget(cfg_data);

    // ---- 3. page tables -----------------------------------------------------
    // Extent of the linear map: all RAM reported now, at least 4 GiB (LAPIC, IOAPIC,
    // HPET live below 4 GiB), rounded to 2 MiB. High MMIO windows (64-bit PCI BARs)
    // are deliberately left out; the kernel maps device memory on demand.
    let prelim = boot::memory_map(MemoryType::LOADER_DATA).map_err(|_| "memory_map failed")?;
    let mut phys_map_end = 4 * GIB;
    for d in prelim.entries() {
        if is_ram(d.ty) {
            phys_map_end = phys_map_end.max(d.phys_start + d.page_count * PAGE);
        }
    }
    drop(prelim);
    // The framebuffer is captured last (opening GOP exclusively may detach the text console).
    let fb = framebuffer_info();
    if fb.present != 0 {
        phys_map_end = phys_map_end.max(fb.phys_addr + fb.size);
    }
    phys_map_end = phys_map_end.div_ceil(HUGE) * HUGE;

    let mut falloc = BootFrameAllocator;
    let pml4_frame = falloc.allocate_frame().ok_or("cannot allocate PML4")?;
    // SAFETY: UEFI identity-maps memory, so the physical address is also the virtual one.
    let pml4: &mut PageTable = unsafe { &mut *(pml4_frame.start_address().as_u64() as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(pml4, VirtAddr::new(0)) };

    for &(va, pa, flags) in &kernel.pages {
        let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(va));
        let frame = PhysFrame::containing_address(PhysAddr::new(pa));
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
    while pa < phys_map_end {
        let frame: PhysFrame<Size2MiB> = PhysFrame::containing_address(PhysAddr::new(pa));
        let linear: Page<Size2MiB> = Page::containing_address(VirtAddr::new(PHYS_OFFSET + pa));
        let ident: Page<Size2MiB> = Page::containing_address(VirtAddr::new(pa));
        // SAFETY: fresh page tables; mapping physical memory at a kernel-owned window.
        unsafe {
            mapper.map_to(linear, frame, linear_flags, &mut falloc).map_err(|_| "linear map")?.ignore();
            mapper.map_to(ident, frame, identity_flags, &mut falloc).map_err(|_| "identity map")?.ignore();
        }
        pa += HUGE;
    }
    println!(
        "spaceboot: linear map 0..{:#x} at {:#x}, framebuffer {}x{} @ {:#x}",
        phys_map_end, PHYS_OFFSET, fb.width, fb.height, fb.phys_addr
    );
    println!("spaceboot: exiting boot services and jumping to kernel");

    // ---- 4. exit boot services ---------------------------------------------
    // SAFETY: no further UEFI calls or allocations follow; every buffer we still
    // need is in MT_KERNEL memory we allocated above.
    let mm = unsafe { boot::exit_boot_services(Some(MT_KERNEL)) };

    let max_regions = MEMMAP_PAGES * PAGE as usize / mem::size_of::<MemRegion>();
    // SAFETY: memmap_phys is MEMMAP_PAGES zeroed pages, identity-mapped.
    let regions: &mut [MemRegion] =
        unsafe { core::slice::from_raw_parts_mut(memmap_phys as *mut MemRegion, max_regions) };
    let mut n = 0usize;
    for d in mm.entries() {
        let d: &MemoryDescriptor = d;
        if d.page_count == 0 || n == max_regions {
            continue;
        }
        let mut kind = region_kind(d.ty);
        if fb.present != 0 && d.phys_start == fb.phys_addr {
            kind = mem_kind::FRAMEBUFFER;
        }
        regions[n] = MemRegion { start: d.phys_start, len: d.page_count * PAGE, kind, _pad: 0 };
        n += 1;
    }
    mem::forget(mm);
    // Insertion sort by start address (no allocation allowed here).
    for i in 1..n {
        let mut j = i;
        while j > 0 && regions[j - 1].start > regions[j].start {
            regions.swap(j - 1, j);
            j -= 1;
        }
    }
    // Merge adjacent regions of the same kind.
    let mut w = 0usize;
    for r in 0..n {
        if w > 0 && regions[w - 1].kind == regions[r].kind && regions[w - 1].end() == regions[r].start {
            regions[w - 1].len += regions[r].len;
        } else {
            regions[w] = regions[r];
            w += 1;
        }
    }
    let region_count = w as u64;

    // SAFETY: bootinfo_phys is one zeroed page, identity-mapped.
    let bi = unsafe { &mut *(bootinfo_phys as *mut BootInfo) };
    *bi = BootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        _pad: 0,
        phys_offset: PHYS_OFFSET,
        phys_map_end,
        memory_map: PhysRange { phys: memmap_phys, len: (MEMMAP_PAGES as u64) * PAGE },
        memory_map_entries: region_count,
        kernel_image: kernel.image,
        initrd,
        cmdline,
        boot_pml4: pml4_frame.start_address().as_u64(),
        boot_stack: PhysRange { phys: stack_phys, len: BOOT_STACK_SIZE as u64 },
        rsdp,
        framebuffer: fb,
    };

    // ---- 5. switch page tables and jump --------------------------------------
    let stack_top = PHYS_OFFSET + stack_phys + BOOT_STACK_SIZE as u64;
    let bootinfo_virt = PHYS_OFFSET + bootinfo_phys;
    // SAFETY: the new tables identity-map the code we are executing and the UEFI
    // stack; NXE/WP are enabled before pages with NX bits become active.
    unsafe {
        asm!("cli", options(nomem, nostack));
        Efer::write(Efer::read() | EferFlags::NO_EXECUTE_ENABLE);
        Cr0::write(Cr0::read() | Cr0Flags::WRITE_PROTECT);
        Cr3::write(pml4_frame, Cr3Flags::empty());
        asm!(
            "mov rsp, {stack}",
            "mov rdi, {bi}",
            "xor rbp, rbp",
            "jmp {entry}",
            stack = in(reg) stack_top,
            bi = in(reg) bootinfo_virt,
            entry = in(reg) kernel.entry,
            options(noreturn)
        );
    }
}
