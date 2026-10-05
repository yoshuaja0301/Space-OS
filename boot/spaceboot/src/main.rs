//! `spaceboot` – the Space OS UEFI bootloader.
//!
//! Responsibilities (see docs/adr/0002-uefi-bootloader.md):
//! 1. load `\EFI\SPACEOS\spacekernel.elf`, `initrd.tar` and `spaceos.cfg` from the ESP,
//!    and choose between a normal boot and recovery (see `recovery`, ADR-0027);
//! 2. place the kernel image, initrd, command line, boot stack and boot info in
//!    memory typed `MemKind::KERNEL` so the kernel never reclaims them by accident;
//! 3. build page tables: kernel at its link address, physical memory linear map at
//!    `PHYS_OFFSET` (and on x86-64 a temporary identity map);
//! 4. capture GOP framebuffer, ACPI RSDP, the time and the final UEFI memory map;
//! 5. exit boot services and jump to the kernel entry with the `BootInfo` pointer
//!    as the first argument.
//!
//! Steps 3 and 5 are the architecture's (`x86.rs`, `aarch64.rs`, ADR-0028).
//! Nothing here depends on the kernel besides the `spaceabi::boot` contract.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod recovery;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
use aarch64 as arch;
#[cfg(target_arch = "x86_64")]
mod x86;
#[cfg(target_arch = "x86_64")]
use x86 as arch;

use alloc::vec::Vec;
use core::mem;

use spaceabi::boot::{
    BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BOOT_STACK_SIZE, BootInfo, FramebufferInfo, MemRegion, PHYS_OFFSET,
    PhysRange, fb_format, mem_kind,
};
use spaceabi::elf::Elf;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::{MemoryDescriptor, MemoryMap, MemoryMapOwned};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::table::cfg::ACPI2_GUID;
use uefi::{CStr16, Status, cstr16, println};

const KERNEL_PATH: &CStr16 = cstr16!("\\EFI\\SPACEOS\\spacekernel.elf");
const INITRD_PATH: &CStr16 = cstr16!("\\EFI\\SPACEOS\\initrd.tar");
const CONFIG_PATH: &CStr16 = cstr16!("\\EFI\\SPACEOS\\spaceos.cfg");

/// UEFI memory type for everything the kernel must keep (OEM range 0x8000_0000+).
const MT_KERNEL: MemoryType = MemoryType::custom(0x8000_0000);

const PAGE: u64 = 4096;
const HUGE: u64 = 2 * 1024 * 1024;
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

/// One 4 KiB page of the kernel image.
#[derive(Clone, Copy)]
pub struct KernelPage {
    pub va: u64,
    pub pa: u64,
    pub writable: bool,
    pub executable: bool,
}

pub struct LoadedKernel {
    pub entry: u64,
    pub image: PhysRange,
    pub pages: Vec<KernelPage>,
}

fn load_kernel(data: &[u8]) -> Result<LoadedKernel, &'static str> {
    let elf = Elf::parse(data).map_err(|_| arch::KERNEL_ELF_ERROR)?;
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
        let first = seg.vaddr & !(PAGE - 1);
        let last = (seg.vaddr + seg.memsz).div_ceil(PAGE) * PAGE;
        let mut va = first;
        while va < last {
            pages.push(KernelPage {
                va,
                pa: phys_base + (va - lo),
                writable: seg.writable(),
                executable: seg.executable(),
            });
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
        // No linear framebuffer: `frame_buffer()` would assert. Serial console only.
        PixelFormat::BltOnly => return FramebufferInfo::default(),
        PixelFormat::Bitmask => fb_format::OTHER,
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
/// True when the 2 MiB frame at `pa` holds memory the kernel may touch through the
/// linear map: RAM of any flavour, or the framebuffer. Everything else (device MMIO,
/// firmware-reserved and unusable ranges) stays out of the write-back linear map.
pub fn backs_ram(map: &MemoryMapOwned, pa: u64, fb: &FramebufferInfo) -> bool {
    let end = pa + HUGE;
    if fb.present != 0 && fb.size != 0 && pa < fb.phys_addr + fb.size && fb.phys_addr < end {
        return true;
    }
    map.entries().any(|d| {
        is_ram(d.ty) && d.page_count != 0 && pa < d.phys_start + d.page_count * PAGE && d.phys_start < end
    })
}

pub fn is_ram(ty: MemoryType) -> bool {
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
    // ---- 0. environment checks ---------------------------------------------
    arch::check_environment()?;

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
    let cmdline = stash(&recovery::choose(&recovery::parse_config(&cfg_data)))?;
    let stack_phys = alloc_kernel_pages(BOOT_STACK_SIZE / PAGE as usize)?;
    let bootinfo_phys = alloc_kernel_pages(1)?;
    let memmap_phys = alloc_kernel_pages(MEMMAP_PAGES)?;
    let rsdp = uefi::system::with_config_table(|entries| {
        entries.iter().find(|e| e.guid == ACPI2_GUID).map(|e| e.address as u64).unwrap_or(0)
    });
    let boot_time = uefi::runtime::get_time().map(|t| unix_seconds(&t)).unwrap_or(0);
    let (uart, uart_kind) = arch::console_uart(rsdp);
    mem::forget(kernel_data);
    mem::forget(initrd_data);
    mem::forget(cfg_data);

    // ---- 3. page tables -----------------------------------------------------
    // The linear map covers RAM and the framebuffer, and nothing else. Device MMIO
    // (PCI BARs, LAPIC, IOAPIC, HPET) is deliberately left out: the kernel maps it
    // uncached on demand, and a second write-back mapping of the same physical page
    // would be an aliased memory type — undefined per the SDM, and enough for a
    // speculative read through the linear map to touch a device register.
    let prelim = boot::memory_map(MemoryType::LOADER_DATA).map_err(|_| "memory_map failed")?;
    let mut phys_map_end = 0u64;
    for d in prelim.entries() {
        if is_ram(d.ty) {
            phys_map_end = phys_map_end.max(d.phys_start + d.page_count * PAGE);
        }
    }
    // The framebuffer is captured last (opening GOP exclusively may detach the text console).
    let fb = framebuffer_info();
    if fb.present != 0 {
        phys_map_end = phys_map_end.max(fb.phys_addr + fb.size);
    }
    phys_map_end = phys_map_end.div_ceil(HUGE) * HUGE;

    let tables = arch::build_tables(&kernel, &prelim, &fb, phys_map_end, uart)?;
    drop(prelim);
    println!(
        "spaceboot: linear map {} MiB of RAM below {:#x} at {:#x} (device MMIO excluded), framebuffer {}x{} @ {:#x}",
        tables.mapped_huge * HUGE / (1024 * 1024),
        phys_map_end,
        PHYS_OFFSET,
        fb.width,
        fb.height,
        fb.phys_addr
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
        boot_pml4: tables.root,
        boot_stack: PhysRange { phys: stack_phys, len: BOOT_STACK_SIZE as u64 },
        rsdp,
        framebuffer: fb,
        boot_time,
        uart,
        uart_kind,
        _pad2: 0,
    };

    // ---- 5. switch page tables and jump --------------------------------------
    let stack_top = PHYS_OFFSET + stack_phys + BOOT_STACK_SIZE as u64;
    let bootinfo_virt = PHYS_OFFSET + bootinfo_phys;
    arch::jump(&tables, &kernel, stack_top, bootinfo_virt)
}

/// Seconds since the Unix epoch of a UEFI time, read as UTC.
fn unix_seconds(t: &uefi::runtime::Time) -> u64 {
    // Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let (y, m, d) = (i64::from(t.year()), i64::from(t.month()), i64::from(t.day()));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs =
        days * 86_400 + i64::from(t.hour()) * 3600 + i64::from(t.minute()) * 60 + i64::from(t.second());
    u64::try_from(secs).unwrap_or(0)
}
