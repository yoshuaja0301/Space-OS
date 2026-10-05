//! AArch64 (ADR-0028): kernel tables for `TTBR1_EL1`, built by hand, and the jump to
//! the kernel with `x0 = &BootInfo`.
//!
//! The firmware runs at EL1 with the MMU on and its identity map in `TTBR0_EL1`.
//! That stays: the bootloader only adds the upper half -- the kernel at its link
//! address, RAM at `PHYS_OFFSET`, the console UART at `EARLY_UART_VIRT` -- and turns
//! on walks through `TTBR1_EL1`. Changing `MAIR_EL1` under the firmware's live
//! tables would change what they mean, so the bootloader uses the attribute indices
//! the firmware already programmed (`spaceabi::boot::mair`) and refuses to boot when
//! they hold something else.

use core::arch::asm;

use spaceabi::boot::{EARLY_UART_VIRT, FramebufferInfo, PHYS_OFFSET, mair, uart_kind};
use uefi::mem::memory_map::MemoryMapOwned;

use crate::{HUGE, LoadedKernel, PAGE, alloc_kernel_pages, backs_ram};

pub const KERNEL_ELF_ERROR: &str = "kernel is not a valid ELF64 AArch64 executable";

const VALID: u64 = 1;
const TABLE: u64 = 1 << 1; // with VALID: table (levels 0-2) or page (level 3)
const AF: u64 = 1 << 10;
const SH_INNER: u64 = 3 << 8;
const AP_RO: u64 = 1 << 7; // EL1 read-only (EL0 never: AP[1] = 0)
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

fn attr(index: u64) -> u64 {
    index << 2
}

fn current_el() -> u64 {
    let el: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    (el >> 2) & 3
}

fn read_mair() -> u64 {
    let v: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, mair_el1", out(reg) v, options(nomem, nostack)) };
    v
}

pub fn check_environment() -> Result<(), &'static str> {
    if current_el() != 1 {
        return Err("firmware does not run at EL1; spaceboot on AArch64 supports EL1 only (ADR-0028)");
    }
    let m = read_mair();
    let at = |i: u64| ((m >> (8 * i)) & 0xFF) as u8;
    if at(mair::DEVICE) != mair::DEVICE_ATTR || at(mair::NORMAL) != mair::NORMAL_ATTR {
        return Err("firmware MAIR_EL1 does not hold Device-nGnRnE at 0 and write-back memory at 3");
    }
    let mmfr0: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nomem, nostack)) };
    // TGran4, bits 31:28: 0b1111 means 4 KiB pages are not implemented.
    if (mmfr0 >> 28) & 0xF == 0xF {
        return Err("the CPU has no 4 KiB translation granule");
    }
    Ok(())
}

/// The console UART from ACPI SPCR: `(physical address, uart_kind)`.
pub fn console_uart(rsdp: u64) -> (u64, u32) {
    if rsdp == 0 {
        return (0, uart_kind::NONE);
    }
    // SAFETY: during boot services the firmware identity-maps its ACPI tables, and
    // every read below stays inside a table's stated length.
    unsafe {
        let rd32 = |a: u64| core::ptr::read_unaligned(a as *const u32);
        let rd64 = |a: u64| core::ptr::read_unaligned(a as *const u64);
        let xsdt = rd64(rsdp + 24);
        if xsdt == 0 || &*(xsdt as *const [u8; 4]) != b"XSDT" {
            return (0, uart_kind::NONE);
        }
        let len = u64::from(rd32(xsdt + 4));
        let mut at = xsdt + 36;
        while at + 8 <= xsdt + len {
            let t = rd64(at);
            at += 8;
            if t == 0 || &*(t as *const [u8; 4]) != b"SPCR" || rd32(t + 4) < 52 {
                continue;
            }
            let interface = *((t + 36) as *const u8);
            let space = *((t + 40) as *const u8);
            let base = rd64(t + 44);
            // 0x03 ARM PL011, 0x0E ARM SBSA generic UART (the PL011 registers the
            // console needs); in system memory.
            if space == 0 && base != 0 && (interface == 0x03 || interface == 0x0E) {
                return (base, uart_kind::PL011);
            }
        }
    }
    (0, uart_kind::NONE)
}

pub struct Tables {
    pub root: u64,
    pub mapped_huge: u64,
}

fn table_at(pa: u64) -> &'static mut [u64; 512] {
    // SAFETY: tables are pages from `alloc_kernel_pages`, identity-mapped during
    // boot services, and only this module touches them.
    unsafe { &mut *(pa as *mut [u64; 512]) }
}

/// The table one level down from `entry`, creating it when absent.
fn next(entry: &mut u64) -> Result<u64, &'static str> {
    if *entry & VALID == 0 {
        let t = alloc_kernel_pages(1)?;
        *entry = t | TABLE | VALID;
    }
    Ok(*entry & 0x0000_FFFF_FFFF_F000)
}

fn index(va: u64, level: u32) -> usize {
    ((va >> (12 + 9 * level)) & 511) as usize
}

/// Map one 4 KiB page (`level` 0) or 2 MiB block (`level` 1) with descriptor bits
/// `bits`.
fn map(root: u64, va: u64, pa: u64, level: u32, bits: u64) -> Result<(), &'static str> {
    let mut t = root;
    for l in ((level + 1)..=3).rev() {
        t = next(&mut table_at(t)[index(va, l)])?;
    }
    let slot = &mut table_at(t)[index(va, level)];
    if *slot & VALID != 0 {
        return Err("kernel tables: address mapped twice");
    }
    *slot = pa | bits;
    Ok(())
}

pub fn build_tables(
    kernel: &LoadedKernel,
    prelim: &MemoryMapOwned,
    fb: &FramebufferInfo,
    phys_map_end: u64,
    uart: u64,
) -> Result<Tables, &'static str> {
    let root = alloc_kernel_pages(1)?;
    for p in &kernel.pages {
        let mut bits = VALID | TABLE | AF | SH_INNER | attr(mair::NORMAL) | UXN;
        if !p.writable {
            bits |= AP_RO;
        }
        if !p.executable {
            bits |= PXN;
        }
        map(root, p.va, p.pa, 0, bits)?;
    }
    let block = VALID | AF | SH_INNER | attr(mair::NORMAL) | UXN | PXN;
    let mut pa = 0u64;
    let mut mapped_huge = 0u64;
    while pa < phys_map_end {
        if backs_ram(prelim, pa, fb) {
            map(root, PHYS_OFFSET + pa, pa, 1, block)?;
            mapped_huge += 1;
        }
        pa += HUGE;
    }
    if uart != 0 {
        let device = VALID | TABLE | AF | attr(mair::DEVICE) | UXN | PXN;
        map(root, EARLY_UART_VIRT, uart & !(PAGE - 1), 0, device)?;
    }
    Ok(Tables { root, mapped_huge })
}

/// Make the kernel image visible to instruction fetch: it was copied in through
/// the data cache.
fn sync_icache(kernel: &LoadedKernel) {
    let ctr: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let line = 4u64 << ((ctr >> 16) & 0xF); // DminLine: log2 of words
    for p in kernel.pages.iter().filter(|p| p.executable) {
        let mut a = p.pa;
        while a < p.pa + PAGE {
            // SAFETY: cleaning a cache line of memory we own changes no data.
            unsafe { asm!("dc cvau, {}", in(reg) a, options(nostack)) };
            a += line;
        }
    }
    // SAFETY: barriers and an instruction-cache invalidate have no other effect.
    unsafe { asm!("dsb ish", "ic iallu", "dsb ish", "isb", options(nostack)) };
}

/// Turn on the upper half and enter the kernel. Boot services are gone by now.
pub fn jump(tables: &Tables, kernel: &LoadedKernel, stack_top: u64, bootinfo_virt: u64) -> ! {
    sync_icache(kernel);
    let mut tcr: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack)) };
    // T1SZ = 16 (48-bit upper half), walks through TTBR1 on, inner-shareable
    // write-back walks, 4 KiB granule; the TTBR0 fields stay the firmware's.
    tcr = (tcr & !TCR_T1_MASK) | TCR_T1;
    // SAFETY: the tables are complete and the code running now stays mapped (the
    // firmware's identity map in TTBR0 is untouched). Operands sit in explicit
    // registers so that clearing x29/x30 cannot clobber one.
    unsafe {
        asm!(
            "msr daifset, #0xf",
            "dsb ish",
            "msr tcr_el1, x4",
            "msr ttbr1_el1, x1",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            "msr spsel, #1",
            "mov sp, x2",
            "mov x29, xzr",
            "mov x30, xzr",
            "br x3",
            in("x0") bootinfo_virt,
            in("x1") tables.root,
            in("x2") stack_top,
            in("x3") kernel.entry,
            in("x4") tcr,
            options(noreturn)
        );
    }
}

/// TCR_EL1 fields for TTBR1: T1SZ (21:16), A1 (22), EPD1 (23), IRGN1 (25:24),
/// ORGN1 (27:26), SH1 (29:28), TG1 (31:30).
const TCR_T1_MASK: u64 = 0xFFFF_0000;
const TCR_T1: u64 = (16 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30);
