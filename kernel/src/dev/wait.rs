//! Waiting on a device that does not interrupt.
//!
//! The storage drivers poll. They start before the timer does, and their commands
//! run under a spinlock with interrupts off, so a wait for a device cannot be
//! counted in timer ticks: it is measured with the CPU's own counter
//! (`arch::cpu::timestamp`). On x86-64 that is the TSC, whose rate is not known this
//! early: taking it to count at 5 GHz, faster than any x86 TSC runs, makes every
//! wait at least as long as asked -- a few times longer on a slow counter -- so a
//! timeout never fires early; it only has to fire at all, and well before a CPU
//! waiting for the driver's lock would take the wait for a deadlock. On AArch64 the
//! generic timer states its rate, and the wait is exact.

use crate::arch::cpu::{counts_per_ms, timestamp};

/// Poll `ready` until it holds or `ms` milliseconds have passed. True if it held.
pub fn until(ms: u64, mut ready: impl FnMut() -> bool) -> bool {
    let start = timestamp();
    let span = ms.saturating_mul(counts_per_ms());
    loop {
        if ready() {
            return true;
        }
        if timestamp().wrapping_sub(start) > span {
            // One last look, so a device that finished just now is not failed.
            return ready();
        }
        core::hint::spin_loop();
    }
}

/// Wait at least `ms` milliseconds, for a device that must be left alone that long.
pub fn pause(ms: u64) {
    until(ms, || false);
}
