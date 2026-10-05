//! Time since boot, kept by a counter rather than by counting interrupts (ADR-0029).
//!
//! The tick says when to look -- sleepers, quanta -- but not how much time went by. A
//! tick interrupt that is taken late, or that merges with the next one because its
//! line was still raised when the next period began, is otherwise time lost for good:
//! on a busy QEMU host a stress run lost three ticks in ten, and the guest's clock
//! ran at 0.70 of the host's. So the boot CPU's tick reads a counter that runs
//! whatever the processors do -- the ACPI PM timer on x86-64, the generic timer's
//! count on AArch64 -- and the tick count becomes what that counter says. A machine
//! without such a counter keeps counting interrupts, and says so at boot.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;

/// A free-running counter: what it is called, how fast it counts, and the bits it
/// has (it wraps from `mask` to 0).
pub struct Counter {
    pub name: &'static str,
    pub hz: u64,
    pub mask: u64,
}

/// Counts per second; 0 while there is no counter.
static RATE: AtomicU64 = AtomicU64::new(0);
static MASK: AtomicU64 = AtomicU64::new(0);
/// The counter's last reading, and the counts since [`init`].
static LAST: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);

/// Find the platform's counter and start from now. Boot CPU, before the scheduler.
pub fn init(rsdp: u64) {
    match arch::clock::counter(rsdp) {
        Ok(c) => {
            LAST.store(arch::clock::read() & c.mask, Ordering::Relaxed);
            MASK.store(c.mask, Ordering::Relaxed);
            RATE.store(c.hz, Ordering::Relaxed);
            println!("[kernel] clock: {} at {} Hz, {} bits", c.name, c.hz, c.mask.count_ones());
        }
        Err(why) => println!("[kernel] clock: counting tick interrupts ({why}); a lost tick is lost time"),
    }
}

/// Ticks of `hz` since [`init`] by the counter, or `None` without one. Only the boot
/// CPU's tick calls this, with interrupts disabled: it must look at least once per
/// wrap of the counter (4.7 s for a 24-bit PM timer), or a whole wrap goes missing.
pub fn ticks(hz: u64) -> Option<u64> {
    let rate = RATE.load(Ordering::Relaxed);
    if rate == 0 {
        return None;
    }
    let mask = MASK.load(Ordering::Relaxed);
    let now = arch::clock::read() & mask;
    let delta = now.wrapping_sub(LAST.swap(now, Ordering::Relaxed)) & mask;
    let total = TOTAL.fetch_add(delta, Ordering::Relaxed) + delta;
    Some((u128::from(total) * u128::from(hz) / u128::from(rate)) as u64)
}
