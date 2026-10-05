//! The tick: the EL1 virtual timer of the generic timer, a PPI of every CPU.
//!
//! Each tick sets the next compare value one period after the previous one rather
//! than after "now", so ticks do not drift with interrupt latency.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{cpu, gic};
use crate::acpi;

/// The virtual timer's interrupt on every Arm platform the BSA covers; the GTDT
/// says if it is another.
const DEFAULT_INTID: u32 = 27;

/// The other CPUs' tick: their timer only ends quanta (the boot CPU's keeps time),
/// at the rate x86-64's application processors use (ADR-0024).
pub const AP_TICK_HZ: u64 = 100;

static INTID: AtomicU32 = AtomicU32::new(DEFAULT_INTID);
static EDGE: AtomicU32 = AtomicU32::new(0);
/// Counts per tick: the boot CPU's, and the others'.
static PERIOD: AtomicU64 = AtomicU64::new(0);
static AP_PERIOD: AtomicU64 = AtomicU64::new(0);

fn period() -> u64 {
    if super::percpu::index() == 0 {
        PERIOD.load(Ordering::Relaxed)
    } else {
        AP_PERIOD.load(Ordering::Relaxed)
    }
}

pub fn intid() -> u32 {
    INTID.load(Ordering::Relaxed)
}

pub fn init(rsdp: u64, hz: u32) {
    // GTDT: the virtual EL1 timer's GSIV at offset 64, its flags (bit 0: edge) at 68.
    let mut edge = false;
    if let Ok(gtdt) = acpi::find(rsdp, b"GTDT")
        && gtdt.len() >= 72
        && acpi::u32_at(gtdt, 64) != 0
    {
        INTID.store(acpi::u32_at(gtdt, 64), Ordering::Relaxed);
        edge = acpi::u32_at(gtdt, 68) & 1 != 0;
    }
    let freq = cpu::frequency();
    PERIOD.store((freq / u64::from(hz)).max(1), Ordering::Relaxed);
    AP_PERIOD.store((freq / AP_TICK_HZ).max(1), Ordering::Relaxed);
    EDGE.store(u32::from(edge), Ordering::Relaxed);
    start_cpu();
    println!("[kernel] timer: generic timer at {freq} Hz, {hz} Hz tick (virtual timer, INTID {})", intid());
}

/// Start this CPU's tick (its own timer interrupt, a PPI of its redistributor).
pub fn start_cpu() {
    gic::enable(intid(), EDGE.load(Ordering::Relaxed) != 0);
    let next = cpu::timestamp() + period();
    // SAFETY: the EL1 virtual timer: compare value, then enabled and unmasked.
    unsafe {
        asm!(
            "msr cntv_cval_el0, {c}",
            "mov {t}, #1",
            "msr cntv_ctl_el0, {t}",
            "isb",
            c = in(reg) next,
            t = out(reg) _,
            options(nomem, nostack)
        )
    };
}

/// Set the next tick; called from the tick's interrupt.
pub fn rearm() {
    let period = period();
    let cval: u64;
    // SAFETY: reading the compare value.
    unsafe { asm!("mrs {}, cntv_cval_el0", out(reg) cval, options(nomem, nostack)) };
    let mut next = cval + period;
    let now = cpu::timestamp();
    // Fell behind by more than a period (a long stop under the host): skip ahead
    // rather than fire a burst of ticks.
    if next <= now {
        next = now + period;
    }
    // SAFETY: writing the compare value clears the timer condition.
    unsafe { asm!("msr cntv_cval_el0, {}", "isb", in(reg) next, options(nomem, nostack)) };
}
