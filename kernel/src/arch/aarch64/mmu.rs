//! What an AArch64 stage-1 descriptor says (4 KiB granule; the walk is `mm::pt`).
//!
//! The kernel half lives in `TTBR1_EL1` and never changes; a process's lower half is
//! `TTBR0_EL1`. A process root is a copy of the kernel root (as on x86-64), which on
//! AArch64 only matters for its lower half: `TTBR0` walks never reach slots
//! 256..512. No ASIDs are used: every switch of `TTBR0` flushes this CPU's TLB, the
//! same rule ADR-0024 relies on for x86-64.

use core::arch::asm;

use spaceabi::boot::mair;

use crate::mm::MapFlags;

const VALID: u64 = 1;
const TABLE_OR_PAGE: u64 = 1 << 1;
const AP_EL0: u64 = 1 << 6; // AP[1]: EL0 may access
const AP_RO: u64 = 1 << 7; // AP[2]: read-only
const SH_INNER: u64 = 3 << 8;
const AF: u64 = 1 << 10;
const NG: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ATTR: u64 = 7 << 2;
const ADDR: u64 = 0x0000_FFFF_FFFF_F000;
/// Upper-half addresses are translated through `TTBR1_EL1`.
const UPPER_HALF: u64 = 0xFFFF_0000_0000_0000;
/// The base address bits of a `TTBRn_EL1` (ASID and CnP masked off).
const TTBR_BADDR: u64 = 0x0000_FFFF_FFFF_FFFE;

pub fn present(e: u64) -> bool {
    e & VALID != 0
}

/// A 1 GiB or 2 MiB block rather than a pointer to the next table.
pub fn is_block(e: u64, level: usize) -> bool {
    (level == 1 || level == 2) && e & TABLE_OR_PAGE == 0
}

pub fn addr(e: u64) -> u64 {
    e & ADDR
}

/// A table descriptor. Access is the leaf's to decide: no table attributes.
pub fn table_entry(pa: u64, _flags: MapFlags) -> u64 {
    pa | TABLE_OR_PAGE | VALID
}

pub fn widen_table(e: u64, _flags: MapFlags) -> u64 {
    e
}

pub fn page_entry(pa: u64, flags: MapFlags) -> u64 {
    let mut e = pa | TABLE_OR_PAGE | VALID | AF;
    if flags.contains(MapFlags::DEVICE) {
        e |= mair::DEVICE << 2;
    } else {
        e |= (mair::NORMAL << 2) | SH_INNER;
    }
    if !flags.contains(MapFlags::WRITABLE) {
        e |= AP_RO;
    }
    if !flags.contains(MapFlags::GLOBAL) {
        e |= NG;
    }
    let exec = flags.contains(MapFlags::EXECUTABLE);
    if flags.contains(MapFlags::USER) {
        // The kernel never runs user pages (the PXN rule stands in for SMEP).
        e |= AP_EL0 | PXN;
        if !exec {
            e |= UXN;
        }
    } else {
        e |= UXN;
        if !exec {
            e |= PXN;
        }
    }
    e
}

pub fn flags(e: u64) -> MapFlags {
    let mut f = MapFlags::KERNEL_RO;
    if e & AP_RO == 0 {
        f |= MapFlags::WRITABLE;
    }
    let user = e & AP_EL0 != 0;
    if user {
        f |= MapFlags::USER;
    }
    if e & NG == 0 {
        f |= MapFlags::GLOBAL;
    }
    if (e & ATTR) >> 2 == mair::DEVICE {
        f |= MapFlags::DEVICE;
    }
    if (user && e & UXN == 0) || (!user && e & PXN == 0) {
        f |= MapFlags::EXECUTABLE;
    }
    f
}

/// Store a descriptor and make it visible to the table walker.
pub fn write_entry(slot: &mut u64, e: u64) {
    // SAFETY: a valid reference; the barrier orders the store before later walks.
    unsafe {
        core::ptr::write_volatile(slot, e);
        asm!("dsb ishst", options(nostack, preserves_flags));
    }
}

/// After a new page appeared at `va`: an invalid descriptor is never cached, so
/// only the instruction stream has to see the new one.
pub fn mapped(_va: u64) {
    // SAFETY: a barrier.
    unsafe { asm!("isb", options(nostack, preserves_flags)) };
}

/// After the page at `va` went away: every CPU forgets it, whatever ASID.
pub fn unmapped(va: u64) {
    // SAFETY: TLB maintenance for one page; no other effect.
    unsafe {
        asm!(
            "tlbi vaae1is, {}",
            "dsb ish",
            "isb",
            in(reg) (va >> 12) & 0x0000_0FFF_FFFF_FFFF,
            options(nostack, preserves_flags)
        )
    };
}

/// The root that translates `va` right now.
pub fn root_for(va: u64) -> u64 {
    let r: u64;
    // SAFETY: reading a system register.
    unsafe {
        if va >= UPPER_HALF {
            asm!("mrs {}, ttbr1_el1", out(reg) r, options(nomem, nostack));
        } else {
            asm!("mrs {}, ttbr0_el1", out(reg) r, options(nomem, nostack));
        }
    }
    r & TTBR_BADDR
}

/// TCR_EL1 for 48-bit halves with a 4 KiB granule and inner-shareable write-back
/// walks: T0SZ = T1SZ = 16, TG0 = 4 KiB, TG1 = 4 KiB, IRGN/ORGN = WB, SH = inner.
/// IPS (bits 34:32) is kept from the firmware, which set it to the physical
/// address size.
const TCR_HALVES: u64 =
    16 | (1 << 8) | (1 << 10) | (3 << 12) | (16 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30);
const TCR_IPS: u64 = 7 << 32;

/// Stop walks through `TTBR0_EL1` (TCR_EL1.EPD0) until [`install_kernel_root`]:
/// it still points at the firmware's identity map, whose tables live in memory the
/// frame allocator is about to hand out. The kernel never uses the lower half.
pub fn disable_lower_half() {
    // SAFETY: only lower-half translation is switched off, which nothing uses.
    unsafe {
        asm!(
            "mrs {t}, tcr_el1",
            "orr {t}, {t}, #(1 << 7)",
            "msr tcr_el1, {t}",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            t = out(reg) _,
            options(nostack)
        )
    };
}

/// Take over translation: `root` becomes `TTBR1_EL1`, and `TTBR0_EL1` -- the
/// firmware's identity map until now -- becomes `root` too, whose lower half is
/// empty: from here on no lower-half address means anything until a process runs.
pub fn install_kernel_root(root: u64) {
    let mut tcr: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack)) };
    tcr = (tcr & TCR_IPS) | TCR_HALVES;
    // SAFETY: `root` maps the whole kernel half, the same as the bootloader's
    // tables did; nothing below runs from the lower half.
    unsafe {
        asm!(
            "dsb ish",
            "msr ttbr0_el1, {r}",
            "msr ttbr1_el1, {r}",
            "msr tcr_el1, {t}",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            r = in(reg) root,
            t = in(reg) tcr,
            options(nostack)
        )
    };
}

/// Switch to the address space at `root`, flushing what this CPU cached of the
/// previous one -- even when it is the same root (ADR-0024).
pub fn load_user_root(root: u64) {
    // SAFETY: only complete roots (the kernel's, or a process's) are passed here.
    unsafe {
        asm!(
            "dsb ish",
            "msr ttbr0_el1, {}",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            in(reg) root,
            options(nostack)
        )
    };
}

/// True while `root` is this CPU's user address space.
pub fn user_root_active(root: u64) -> bool {
    root_for(0) == root
}
