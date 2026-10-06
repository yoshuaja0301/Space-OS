//! `spaceboot` – the Space OS UEFI bootloader.
//!
//! Responsibilities (see docs/adr/0002-uefi-bootloader.md):
//! 1. load `\EFI\SPACEOS\spacekernel.elf`, `initrd.tar` and `spaceos.cfg` from the ESP,
//!    and choose between a normal boot and recovery (see `recovery`, ADR-0027);
//! 2. place the kernel image, initrd, command line, boot stack and boot info in
//!    memory typed `MemKind::KERNEL` so the kernel never reclaims them by accident;
//! 3. build page tables: kernel at its link address, physical memory linear map at
//!    `PHYS_OFFSET` (and on x86-64 a temporary identity map);
//! 4. capture GOP framebuffer, ACPI RSDP, the time, entropy from the firmware's RNG
//!    and the final UEFI memory map;
//! 5. exit boot services and jump to the kernel entry with the `BootInfo` pointer
//!    as the first argument.
//!
//! Steps 3 and 5 are the architecture's (`x86.rs`, `aarch64.rs`, ADR-0028).
//! Nothing here depends on the kernel besides the `spaceabi::boot` contract, which
//! the kernel checks before it believes any of it (ADR-0032): every range handed
//! over is listed as a reservation with its owner, the boot image comes with the
//! SHA-256 taken here, and `fault` can damage the result on purpose for a test.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod fault;
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
use core::ptr::NonNull;

use spaceabi::boot::{
    BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BOOT_STACK_SIZE, BootInfo, Entropy, FramebufferInfo, LINEAR_MAP_MAX,
    MAX_RESERVATIONS, MEMORY_MAP_FORMAT, MemRegion, PHYS_OFFSET, PhysRange, Reservation, entropy_source,
    fb_format, flags, mem_kind, owner, uart_kind,
};
use spaceabi::elf::Elf;
use spaceabi::sha256;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::{MemoryDescriptor, MemoryMap, MemoryMapOwned};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::proto::rng::Rng;
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

/// The page tables the kernel is entered on, taken from one allocation so that a
/// single reservation covers them all. Sized for the worst case before the tables
/// are built; what is left over goes back to the firmware afterwards.
pub struct TablePool {
    base: u64,
    pages: usize,
    used: usize,
}

impl TablePool {
    /// Enough pages for the kernel image's 4 KiB pages and 2 MiB blocks up to
    /// `phys_map_end`, twice over (x86-64 also builds an identity map), plus the
    /// AArch64 UART page and some room.
    fn for_map(kernel: &LoadedKernel, phys_map_end: u64) -> Result<TablePool, &'static str> {
        const GIB: u64 = 1 << 30;
        let kernel_tables = kernel.image.len.div_ceil(HUGE) as usize + 4;
        let gibs = phys_map_end.div_ceil(GIB) as usize;
        let tibs = phys_map_end.div_ceil(512 * GIB) as usize;
        let pages = 1 + kernel_tables + 2 * (gibs + tibs) + 8;
        Ok(TablePool { base: alloc_kernel_pages(pages)?, pages, used: 0 })
    }

    /// One zeroed page for a table.
    pub fn take(&mut self) -> Result<u64, &'static str> {
        if self.used == self.pages {
            return Err("page-table pool exhausted");
        }
        let pa = self.base + self.used as u64 * PAGE;
        self.used += 1;
        Ok(pa)
    }

    /// Give the pages no table needed back to the firmware; the range the tables
    /// are in.
    fn finish(self) -> PhysRange {
        let unused = self.pages - self.used;
        let tail = self.base + self.used as u64 * PAGE;
        if unused > 0
            && let Some(ptr) = NonNull::new(tail as *mut u8)
        {
            // SAFETY: pages of our own allocation that nothing refers to. Should the
            // firmware refuse, they stay ours, inside nobody's reservation: harmless.
            let _ = unsafe { boot::free_pages(ptr, unused) };
        }
        PhysRange { phys: self.base, len: self.used as u64 * PAGE }
    }
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

/// Bytes from the firmware's RNG protocol, if it has one. Their quality is not
/// assumed (PRD v0.2 §7.2): the kernel mixes them in and never relies on them alone.
fn firmware_entropy() -> Entropy {
    let mut e = Entropy::NONE;
    let rng = boot::get_handle_for_protocol::<Rng>().and_then(boot::open_protocol_exclusive::<Rng>);
    let Ok(mut rng) = rng else {
        println!("spaceboot: entropy: the firmware has no RNG protocol");
        return e;
    };
    let mut buf = [0u8; 32];
    match rng.get_rng(None, &mut buf) {
        Ok(()) => {
            e.source = entropy_source::UEFI_RNG;
            e.len = buf.len() as u32;
            e.bytes[..buf.len()].copy_from_slice(&buf);
            println!("spaceboot: entropy: {} bytes from the firmware's RNG protocol", buf.len());
        }
        Err(err) => println!("spaceboot: entropy: the firmware's RNG protocol failed ({:?})", err.status()),
    }
    buf.fill(0);
    e
}

fn hex(digest: &[u8; 32]) -> alloc::string::String {
    let h = sha256::to_hex(digest);
    alloc::string::String::from(core::str::from_utf8(&h).unwrap_or("?"))
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
    let config = recovery::parse_config(&cfg_data);
    let fault = match config.bootinfo_fault.as_slice() {
        [] => None,
        name => match fault::find(name) {
            Some(f) => {
                println!(
                    "spaceboot: bootinfo_fault={}: handing the kernel {} (a test of the kernel's checks; never on a real boot)",
                    f.name, f.what
                );
                Some(f)
            }
            None => {
                let name = core::str::from_utf8(name).unwrap_or("(not text)");
                println!("spaceboot: bootinfo_fault={name} is not a fault spaceboot knows; ignored");
                None
            }
        },
    };
    let kernel = load_kernel(&kernel_data)?;
    let initrd_sha256 = if initrd_data.is_empty() { [0; 32] } else { sha256::digest(&initrd_data) };
    if !initrd_data.is_empty() {
        println!("spaceboot: boot image {} bytes, sha256 {}", initrd_data.len(), hex(&initrd_sha256));
    }
    let initrd = stash(&initrd_data)?;
    let choice = recovery::choose(&config);
    if let Err(e) = spaceabi::boot::validate_cmdline(&choice.cmdline) {
        println!("spaceboot: the command line in spaceos.cfg cannot be handed over: {e}");
        return Err("the command line must be UTF-8 and at most 4096 bytes");
    }
    let cmdline = stash(&choice.cmdline)?;
    let stack_phys = alloc_kernel_pages(BOOT_STACK_SIZE / PAGE as usize)?;
    let bootinfo_phys = alloc_kernel_pages(1)?;
    let memmap_phys = alloc_kernel_pages(MEMMAP_PAGES)?;
    let rsdp = uefi::system::with_config_table(|entries| {
        entries.iter().find(|e| e.guid == ACPI2_GUID).map(|e| e.address as u64).unwrap_or(0)
    });
    let boot_time = uefi::runtime::get_time().map(|t| unix_seconds(&t)).unwrap_or(0);
    let (uart, uart_kind) = arch::console_uart(rsdp);
    let entropy = firmware_entropy();
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
    phys_map_end = phys_map_end.div_ceil(HUGE) * HUGE;
    if phys_map_end > LINEAR_MAP_MAX {
        return Err("RAM above 16 TiB: the kernel's linear map cannot hold it");
    }
    // The framebuffer is captured last (opening GOP exclusively may detach the text
    // console). One the contract cannot describe -- beyond what the linear map
    // holds, or not page-aligned -- is left behind rather than handed over wrong.
    let mut fb = framebuffer_info();
    if fb.present != 0 {
        let end = fb.phys_addr.checked_add(fb.size).map(|e| e.div_ceil(HUGE) * HUGE).unwrap_or(u64::MAX);
        let with_fb = phys_map_end.max(end).min(LINEAR_MAP_MAX);
        match fb.check(with_fb) {
            Ok(()) => phys_map_end = with_fb,
            Err(e) => {
                println!("spaceboot: the framebuffer is not handed over ({e}); serial console only");
                fb = FramebufferInfo::default();
            }
        }
    }

    let mut pool = TablePool::for_map(&kernel, phys_map_end)?;
    let tables = arch::build_tables(&kernel, &prelim, &fb, phys_map_end, uart, &mut pool)?;
    let page_tables = pool.finish();
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

    // Every range handed over, with what it holds.
    let pages = |r: PhysRange| PhysRange { phys: r.phys, len: r.len.div_ceil(PAGE) * PAGE };
    let boot_stack = PhysRange { phys: stack_phys, len: BOOT_STACK_SIZE as u64 };
    let memory_map = PhysRange { phys: memmap_phys, len: (MEMMAP_PAGES as u64) * PAGE };
    let held = [
        (owner::KERNEL_IMAGE, kernel.image),
        (owner::BOOT_IMAGE, pages(initrd)),
        (owner::CMDLINE, pages(cmdline)),
        (owner::BOOT_STACK, boot_stack),
        (owner::BOOT_INFO, PhysRange { phys: bootinfo_phys, len: PAGE }),
        (owner::MEMORY_MAP, memory_map),
        (owner::PAGE_TABLES, page_tables),
    ];
    let mut reservations = [Reservation::default(); MAX_RESERVATIONS];
    let mut reservation_count = 0;
    for (who, range) in held.into_iter().filter(|(_, r)| r.len != 0) {
        reservations[reservation_count] = Reservation { range, owner: who, _pad: 0 };
        reservation_count += 1;
    }
    let present = [
        (flags::FRAMEBUFFER, fb.present != 0),
        (flags::ACPI, rsdp != 0),
        (flags::BOOT_IMAGE, initrd.len != 0),
        (flags::CMDLINE, cmdline.len != 0),
        (flags::UART, uart_kind != uart_kind::NONE),
        (flags::BOOT_TIME, boot_time != 0),
        (flags::ENTROPY, entropy.source != entropy_source::NONE),
    ];
    let info_flags = present.iter().filter(|(_, there)| *there).fold(0, |f, (bit, _)| f | bit);

    // SAFETY: bootinfo_phys is one zeroed page, identity-mapped.
    let bi = unsafe { &mut *(bootinfo_phys as *mut BootInfo) };
    *bi = BootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        size: mem::size_of::<BootInfo>() as u32,
        flags: info_flags,
        phys_offset: PHYS_OFFSET,
        phys_map_end,
        memory_map,
        memory_map_entries: region_count,
        memory_map_entry_size: mem::size_of::<MemRegion>() as u32,
        memory_map_format: MEMORY_MAP_FORMAT,
        kernel_image: kernel.image,
        initrd,
        initrd_sha256,
        cmdline,
        boot_pml4: tables.root,
        boot_stack,
        rsdp,
        framebuffer: fb,
        boot_time,
        uart,
        uart_kind,
        reservation_count: reservation_count as u32,
        reservations,
        entropy,
        boot_slot: choice.slot,
    };
    if let Some(f) = fault {
        f.apply(bi, &mut regions[..w]);
    }

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
