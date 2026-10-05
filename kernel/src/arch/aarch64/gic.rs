//! GICv3 interrupt controller: distributor and this CPU's redistributor through
//! MMIO, the CPU interface through system registers.
//!
//! Where they are comes from the ACPI MADT (GICD and GICR structures, or the GICR
//! base in each GICC). Every interrupt is Group 1 Non-secure, which a kernel at EL1
//! receives as IRQ; priorities are all the same, so nothing preempts an interrupt
//! handler. SPIs go to the boot CPU.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{acpi, mm};

/// The software-generated interrupt that tells a CPU to look at the run queue.
pub const SGI_RESCHEDULE: u32 = 1;

const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_ICENABLER: u64 = 0x0180;
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ICFGR: u64 = 0x0C00;
const GICD_IROUTER: u64 = 0x6000;
const CTLR_RWP: u32 = 1 << 31;
/// Affinity routing, and Group 1 enabled (bits 0 and 1 cover both the
/// single-security-state and the Non-secure view of the register).
const CTLR_ENABLE: u32 = (1 << 4) | (1 << 1) | 1;

const GICR_TYPER: u64 = 0x0008;
const GICR_WAKER: u64 = 0x0014;
/// The SGI/PPI frame follows the redistributor's control frame.
const SGI_FRAME: u64 = 0x1_0000;
const PRIORITY: u8 = 0xA0;

static GICD: AtomicU64 = AtomicU64::new(0);
/// This CPU's redistributor (control frame).
static GICR: AtomicU64 = AtomicU64::new(0);

fn rd32(base: u64, off: u64) -> u32 {
    // SAFETY: `base` is a mapped GIC frame and `off` one of its registers.
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

fn wr32(base: u64, off: u64, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, v) }
}

fn wr64(base: u64, off: u64, v: u64) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile((base + off) as *mut u64, v) }
}

fn rd64(base: u64, off: u64) -> u64 {
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile((base + off) as *const u64) }
}

/// This CPU's affinity, packed the way GICR_TYPER and GICD_IROUTER hold it.
fn affinity() -> u64 {
    let mpidr: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    (mpidr & 0x00FF_FFFF) | ((mpidr >> 8) & 0xFF00_0000)
}

fn wait_rwp(gicd: u64) {
    crate::dev::wait::until(100, || rd32(gicd, GICD_CTLR) & CTLR_RWP == 0);
}

struct Layout {
    gicd: u64,
    version: u8,
    /// Redistributor regions: (base, length).
    gicr: alloc::vec::Vec<(u64, u64)>,
}

fn layout(rsdp: u64) -> Result<Layout, &'static str> {
    let madt = acpi::find(rsdp, b"APIC").map_err(|_| "no MADT")?;
    let mut l = Layout { gicd: 0, version: 0, gicr: alloc::vec::Vec::new() };
    let mut per_cpu = alloc::vec::Vec::new();
    for (kind, e) in acpi::madt_entries(madt) {
        match kind {
            // GIC distributor: base at 8, version at 20.
            0x0C if e.len() >= 21 => {
                l.gicd = acpi::u64_at(e, 8);
                l.version = e[20];
            }
            // GIC redistributor region: base at 4, length at 12.
            0x0E if e.len() >= 16 => l.gicr.push((acpi::u64_at(e, 4), u64::from(acpi::u32_at(e, 12)))),
            // GIC CPU interface: this CPU's redistributor at 60 when there is no
            // region structure.
            0x0B if e.len() >= 68 && acpi::u64_at(e, 60) != 0 => {
                per_cpu.push((acpi::u64_at(e, 60), 0x2_0000))
            }
            _ => {}
        }
    }
    if l.gicd == 0 {
        return Err("the MADT has no GIC distributor");
    }
    if l.gicr.is_empty() {
        l.gicr = per_cpu;
    }
    // Version 0 means "find out from the hardware"; GICD_PIDR2 says.
    if l.version != 0 && l.version < 3 {
        return Err("a GICv2 (only GICv3 and later are supported)");
    }
    if l.gicr.is_empty() {
        return Err("the MADT has no GIC redistributor");
    }
    Ok(l)
}

pub fn init(rsdp: u64) -> Result<(), &'static str> {
    let l = layout(rsdp)?;
    let gicd = mm::mmio::map(l.gicd, 0x1_0000).map_err(|_| "cannot map the distributor")?;
    let me = affinity();
    let mut mine = 0;
    'regions: for &(base, len) in &l.gicr {
        let virt = mm::mmio::map(base, len).map_err(|_| "cannot map the redistributors")?;
        let mut at = 0;
        while at + 0x2_0000 <= len {
            let typer = rd64(virt, at + GICR_TYPER);
            if typer >> 32 == me {
                mine = virt + at;
                break 'regions;
            }
            // VLPIS (bit 1) adds two frames; Last (bit 4) ends the region.
            if typer & (1 << 4) != 0 {
                break;
            }
            at += if typer & 2 != 0 { 0x4_0000 } else { 0x2_0000 };
        }
    }
    if mine == 0 {
        return Err("no redistributor for this CPU");
    }
    GICD.store(gicd, Ordering::Relaxed);
    GICR.store(mine, Ordering::Relaxed);

    // Distributor: affinity routing and Group 1 on; every SPI masked, Group 1,
    // one priority.
    wr32(gicd, GICD_CTLR, rd32(gicd, GICD_CTLR) | CTLR_ENABLE);
    wait_rwp(gicd);
    let lines = ((rd32(gicd, GICD_TYPER) & 0x1F) + 1) * 32;
    for n in 1..lines / 32 {
        wr32(gicd, GICD_ICENABLER + 4 * u64::from(n), u32::MAX);
        wr32(gicd, GICD_IGROUPR + 4 * u64::from(n), u32::MAX);
    }
    wait_rwp(gicd);
    init_cpu()?;
    println!(
        "[kernel] gic: GICv3 distributor at {:#x}, {} interrupt lines, redistributor for affinity {:#x}",
        l.gicd, lines, me
    );
    Ok(())
}

/// This CPU's redistributor and CPU interface.
fn init_cpu() -> Result<(), &'static str> {
    let gicr = GICR.load(Ordering::Relaxed);
    // Wake the redistributor: clear ProcessorSleep, wait for ChildrenAsleep to clear.
    wr32(gicr, GICR_WAKER, rd32(gicr, GICR_WAKER) & !(1 << 1));
    if !crate::dev::wait::until(100, || rd32(gicr, GICR_WAKER) & (1 << 2) == 0) {
        return Err("the redistributor does not wake up");
    }
    let sgi = gicr + SGI_FRAME;
    wr32(sgi, GICD_IGROUPR, u32::MAX);
    for i in 0..8 {
        wr32(sgi, GICD_IPRIORITYR + 4 * i, u32::from_ne_bytes([PRIORITY; 4]));
    }
    // SAFETY: the GICv3 CPU interface registers: system-register access on, every
    // priority let through, Group 1 on.
    unsafe {
        asm!(
            "mrs {t}, icc_sre_el1",
            "orr {t}, {t}, #1",
            "msr icc_sre_el1, {t}",
            "isb",
            "mov {t}, #0xff",
            "msr icc_pmr_el1, {t}",
            "msr icc_bpr1_el1, xzr",
            "mov {t}, #1",
            "msr icc_igrpen1_el1, {t}",
            "isb",
            t = out(reg) _,
            options(nostack)
        )
    };
    Ok(())
}

/// Let interrupt `intid` through to the boot CPU: a PPI (16..32) in this CPU's
/// redistributor, an SPI (32..) in the distributor. `edge` for edge-triggered.
pub fn enable(intid: u32, edge: bool) {
    let (word, bit) = (u64::from(intid / 32), intid % 32);
    let cfg_word = u64::from(intid / 16);
    let cfg_bit = (intid % 16) * 2 + 1;
    if intid < 32 {
        let sgi = GICR.load(Ordering::Relaxed) + SGI_FRAME;
        let cfg = rd32(sgi, GICD_ICFGR + 4 * cfg_word);
        wr32(sgi, GICD_ICFGR + 4 * cfg_word, if edge { cfg | (1 << cfg_bit) } else { cfg & !(1 << cfg_bit) });
        wr32(sgi, GICD_ISENABLER, 1 << bit);
        return;
    }
    let gicd = GICD.load(Ordering::Relaxed);
    let prio = GICD_IPRIORITYR + u64::from(intid);
    // SAFETY: a byte register of the mapped distributor.
    unsafe { core::ptr::write_volatile((gicd + prio) as *mut u8, PRIORITY) };
    let cfg = rd32(gicd, GICD_ICFGR + 4 * cfg_word);
    wr32(gicd, GICD_ICFGR + 4 * cfg_word, if edge { cfg | (1 << cfg_bit) } else { cfg & !(1 << cfg_bit) });
    wr64(gicd, GICD_IROUTER + 8 * u64::from(intid), affinity());
    wr32(gicd, GICD_IGROUPR + 4 * word, rd32(gicd, GICD_IGROUPR + 4 * word) | (1 << bit));
    wr32(gicd, GICD_ISENABLER + 4 * word, 1 << bit);
    wait_rwp(gicd);
}

/// The interrupt being signalled, acknowledged; `None` if it was spurious.
pub fn acknowledge() -> Option<u32> {
    let id: u64;
    // SAFETY: reading the acknowledge register is the acknowledgement.
    unsafe { asm!("mrs {}, icc_iar1_el1", out(reg) id, options(nomem, nostack)) };
    let id = (id & 0xFF_FFFF) as u32;
    (id < 1020).then_some(id)
}

/// Done with `intid` (priority drop and deactivation: EOImode 0).
pub fn end(intid: u32) {
    // SAFETY: ending an interrupt this CPU acknowledged.
    unsafe { asm!("msr icc_eoir1_el1, {}", "isb", in(reg) u64::from(intid), options(nomem, nostack)) };
}
